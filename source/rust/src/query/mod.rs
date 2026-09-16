//! Query engine: full-text search backed by [Tantivy].
//!
//! Tantivy (<https://github.com/quickwit-oss/tantivy>) is a Rust full-text
//! search library modelled on Apache Lucene. It is used as an embedded, in-process
//! dependency rather than rolling our own full-text search (see
//! `docs/decisions/0003-search-engine.md`).
//!
//! Semantics (see `docs/supported_operations.md`):
//! - Token-based full-text match across `searchable: true` fields, using a
//!   per-field analyzer (the index default is the English analyzer:
//!   lowercasing, punctuation splitting, English stopword removal, English
//!   stemming, approximating Azure's basic English analyzer). Fields may
//!   declare an Azure analyzer name (`keyword`, `whitespace`, `alphanum`,
//!   `latin`, `ngram`, `edgeNgram`, CJK, and the `*.microsoft` language
//!   analyzers); indexed and query terms are always analyzed with the same
//!   analyzer.
//! - `*` or an empty search term matches all documents.
//! - A multi-term search combines required clauses with OR (`searchMode=any`,
//!   the default, matching Azure) or AND (`searchMode=all`).
//! - Simple-query boolean operators: `+term` (required, the default),
//!   `-term` (excluded), `"quoted phrases"`, and Lucene-style fuzzy terms
//!   (`term~` for the default edit distance 2, `term~1` for distance 1).
//! - `queryType=full` (Lucene) queries are parsed by Tantivy's query parser:
//!   `AND`/`OR`/`NOT`, `+`/`-`, `"phrases"`, `field:term`, `term~N` fuzzy,
//!   `term*` wildcards, `field:[a TO b]` ranges, and `^N` boosts.
//! - Field-specific search via [`FullTextQuery::fields`], with per-field
//!   boosts from `searchFields` weights (`field^N`).
//! - Results carry Tantivy BM25 relevance scores for ranking.
//!
//! The engine returns matching keys with scores; the service layer resolves
//! those keys back to the full stored documents, applies filters, ordering,
//! projection, and paging.
use std::collections::BTreeMap;

use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    TermQuery,
};
use tantivy::schema::Value as _;
use tantivy::schema::{Field, IndexRecordOption, Schema, STORED, STRING};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::storage::{Document, FieldDefinition};
use crate::sync_util::{read_unpoisoned, write_unpoisoned};

use self::analyzers::text_options_for;

pub mod analyzers;
pub mod lucene;
pub mod simple;

pub use analyzers::{
    analyze, analyze_with, analyze_with_offsets, analyze_with_offsets_and_analyzer,
    analyzer_tokenizer_name, emulator_tokenizer_manager, register_analyzers, AnalyzeToken,
    KNOWN_ANALYZERS,
};
pub(crate) use lucene::build_lucene_query;
pub use lucene::lucene_query_terms;
pub use simple::{parse_search_text, Clause, FullTextQuery, QueryType, SearchMode};

/// Reserved Tantivy field name used to store each document's key.
const KEY_FIELD_NAME: &str = "__aisearch_key";

/// A searchable field: the Azure field path, its Tantivy field, and the
/// field's declared analyzer name (`None` for the English default).
#[derive(Debug, Clone)]
pub(crate) struct SearchableField {
    pub(crate) name: String,
    pub(crate) field: Field,
    pub(crate) analyzer: Option<String>,
}

/// Heap budget (bytes) for each per-index Tantivy writer.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Upper bound on the number of matching documents collected per search. The
/// emulator targets local development and testing, where index sizes are small.
const MAX_MATCHES: usize = 1_000_000;

/// Errors produced by the query engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// The requested search index does not exist.
    #[error("index {0:?} not found")]
    IndexNotFound(String),
    /// The search text is malformed (e.g. an unparseable Lucene query).
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// An underlying Tantivy operation failed.
    #[error("search engine error: {0}")]
    Engine(String),
}

/// Maps an underlying Tantivy failure onto [`QueryError::Engine`].
fn engine_err(error: impl std::fmt::Display) -> QueryError {
    QueryError::Engine(error.to_string())
}

/// A single Tantivy-backed search index.
struct EngineIndex {
    /// The underlying index, kept (cheaply cloneable) so Lucene queries can
    /// be parsed with a [`QueryParser`] bound to the schema and tokenizers.
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    key_field: Field,
    /// Searchable fields; `analyzer` is the field's declared Azure analyzer
    /// name (`None` for the English default).
    searchable: Vec<SearchableField>,
}

/// The full-text search engine. Owns one Tantivy index per emulator index.
///
/// Safe for concurrent use: searches take a read lock, mutations take a write
/// lock, so concurrent requests cannot corrupt state.
#[derive(Default)]
pub struct SearchEngine {
    inner: std::sync::RwLock<BTreeMap<String, EngineIndex>>,
}

impl SearchEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds and registers a Tantivy index for `name`, derived from the
    /// emulator index schema.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::Engine`] if the index already exists or Tantivy
    /// fails to create the underlying index.
    pub fn create_index(&self, name: &str, fields: &[FieldDefinition]) -> Result<(), QueryError> {
        let (schema, key_field, searchable) = build_schema(fields);
        let index = Index::create_in_ram(schema);
        // Register every supported analyzer so each field's declared
        // tokenizer name resolves at index time.
        register_analyzers(index.tokenizers());
        let reader = index.reader().map_err(engine_err)?;
        let writer = index.writer(WRITER_HEAP_BYTES).map_err(engine_err)?;
        let mut guard = write_unpoisoned(&self.inner);
        if guard.contains_key(name) {
            return Err(QueryError::Engine(format!(
                "search index {name:?} already exists"
            )));
        }
        guard.insert(
            name.to_owned(),
            EngineIndex {
                index,
                reader,
                writer,
                key_field,
                searchable,
            },
        );
        Ok(())
    }

    /// Removes the Tantivy index for `name`, if present.
    pub fn delete_index(&self, name: &str) {
        let mut guard = write_unpoisoned(&self.inner);
        guard.remove(name);
    }

    /// Indexes `documents` into the Tantivy index for `name`, replacing any
    /// previously indexed document with the same key. Commits so the documents
    /// are immediately searchable.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::IndexNotFound`] if the index does not exist, or
    /// [`QueryError::Engine`] if a Tantivy operation fails.
    pub fn index_documents(&self, name: &str, documents: &[Document]) -> Result<(), QueryError> {
        let mut guard = write_unpoisoned(&self.inner);
        let engine = guard
            .get_mut(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        for document in documents {
            engine
                .writer
                .delete_term(Term::from_field_text(engine.key_field, &document.key));
            let mut tantivy_doc = TantivyDocument::new();
            tantivy_doc.add_text(engine.key_field, &document.key);
            for searchable in &engine.searchable {
                for value in document.resolve_path(&searchable.name) {
                    // A plain collection field (e.g. `Edm.Collection(Edm.String)`)
                    // resolves to a single JSON array; index each element.
                    if let Value::Array(items) = value {
                        for item in items {
                            if let Some(text) = text_representation(item) {
                                tantivy_doc.add_text(searchable.field, text);
                            }
                        }
                    } else if let Some(text) = text_representation(value) {
                        tantivy_doc.add_text(searchable.field, text);
                    }
                }
            }
            engine
                .writer
                .add_document(tantivy_doc)
                .map_err(engine_err)?;
        }
        engine.writer.commit().map_err(engine_err)?;
        // Reload synchronously so newly indexed documents are immediately
        // searchable (the emulator guarantees immediate consistency).
        engine.reader.reload().map_err(engine_err)?;
        Ok(())
    }

    /// Removes the documents with the given keys from the Tantivy index for
    /// `name`. Commits so the deletions are immediately visible.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::IndexNotFound`] if the index does not exist, or
    /// [`QueryError::Engine`] if a Tantivy operation fails.
    pub fn delete_documents(&self, name: &str, keys: &[String]) -> Result<(), QueryError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut guard = write_unpoisoned(&self.inner);
        let engine = guard
            .get_mut(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        for key in keys {
            engine
                .writer
                .delete_term(Term::from_field_text(engine.key_field, key));
        }
        engine.writer.commit().map_err(engine_err)?;
        engine.reader.reload().map_err(engine_err)?;
        Ok(())
    }

    /// Runs a full-text search, returning the matching document keys with
    /// their BM25 relevance scores for ranking by the service layer.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::IndexNotFound`] if the index does not exist, or
    /// [`QueryError::Engine`] if a Tantivy operation fails.
    pub fn search(
        &self,
        name: &str,
        query: &FullTextQuery,
    ) -> Result<BTreeMap<String, f32>, QueryError> {
        let guard = read_unpoisoned(&self.inner);
        let engine = guard
            .get(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        let fields: Vec<SearchableField> = match &query.fields {
            Some(names) => engine
                .searchable
                .iter()
                .filter(|searchable| names.contains(&searchable.name))
                .cloned()
                .collect(),
            None => engine.searchable.clone(),
        };
        // A non-match-all query cannot match when there are no searchable
        // fields in scope.
        if !query.is_match_all() && fields.is_empty() {
            return Ok(BTreeMap::new());
        }
        let tantivy_query = match &query.lucene {
            Some(text) => {
                build_lucene_query(&engine.index, text, &fields, &query.boosts, query.mode)?
            }
            None => build_query(query, &fields),
        };
        let searcher = engine.reader.searcher();
        let top_docs = searcher
            .search(
                &*tantivy_query,
                &TopDocs::with_limit(MAX_MATCHES).order_by_score(),
            )
            .map_err(engine_err)?;
        let mut scored = BTreeMap::new();
        for (score, doc_address) in top_docs {
            if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                if let Some(key) = doc
                    .get_first(engine.key_field)
                    .and_then(|value| value.as_str())
                {
                    scored
                        .entry(key.to_owned())
                        .and_modify(|best: &mut f32| *best = best.max(score))
                        .or_insert(score);
                }
            }
        }
        Ok(scored)
    }

    /// Clears all Tantivy indexes.
    pub fn reset(&self) {
        let mut guard = write_unpoisoned(&self.inner);
        guard.clear();
    }
}

/// Builds the Tantivy schema for an emulator index: a stored key field plus one
/// tokenized text field per `searchable: true` field, each with the field's
/// declared analyzer (the English default when none is declared). Searchable
/// complex-type subfields are indexed under their field path (`Address/City`).
fn build_schema(fields: &[FieldDefinition]) -> (Schema, Field, Vec<SearchableField>) {
    let mut builder = Schema::builder();
    let key_field = builder.add_text_field(KEY_FIELD_NAME, STRING | STORED);
    let mut searchable = Vec::new();
    collect_searchable(&mut builder, &mut searchable, "", fields);
    (builder.build(), key_field, searchable)
}

fn collect_searchable(
    builder: &mut tantivy::schema::SchemaBuilder,
    searchable: &mut Vec<SearchableField>,
    prefix: &str,
    fields: &[FieldDefinition],
) {
    for field in fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}/{}", field.name)
        };
        if field.is_complex_type() {
            // Complex types (single or collection) index their searchable
            // subfields under the field path; a collection contributes one
            // value per element at index time.
            collect_searchable(builder, searchable, &path, &field.subfields);
        } else if field.searchable && !field.is_vector_field() {
            // Vector fields require `searchable: true` per Azure but are not
            // full-text indexed (their values are numeric arrays).
            let tantivy_field =
                builder.add_text_field(&path, text_options_for(field.analyzer.as_deref()));
            searchable.push(SearchableField {
                name: path,
                field: tantivy_field,
                analyzer: field.analyzer.clone(),
            });
        }
    }
}

/// The full-text representation of a scalar document value: strings as-is,
/// numbers in their original (shortest round-trip) form, booleans as
/// `true` / `false`. `null`, objects, and arrays have no full-text
/// representation.
fn text_representation(value: &Value) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    match value {
        Value::String(text) => Some(Cow::Borrowed(text.as_str())),
        Value::Number(number) => Some(Cow::Owned(number.to_string())),
        Value::Bool(flag) => {
            if *flag {
                Some(Cow::Borrowed("true"))
            } else {
                Some(Cow::Borrowed("false"))
            }
        }
        _ => None,
    }
}

/// Builds the Tantivy query for a [`FullTextQuery`]: match-all when the query
/// has no clauses, otherwise a boolean combination where each required clause
/// is `Should` (`searchMode=any`, the default) or `Must` (`searchMode=all`)
/// and each excluded clause is `MustNot`. Each clause is an OR over the
/// in-scope searchable fields (a term query per analyzer token, or a phrase
/// query for quoted phrases), analyzed with each field's own analyzer. A
/// clause that analyzes to no tokens (e.g. a stopword-only term, or
/// punctuation only) matches nothing.
fn build_query(query: &FullTextQuery, fields: &[SearchableField]) -> Box<dyn Query> {
    if query.is_match_all() {
        return Box::new(AllQuery);
    }
    let required_occur = match query.mode {
        SearchMode::All => Occur::Must,
        SearchMode::Any => Occur::Should,
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        Vec::with_capacity(query.required.len() + query.excluded.len() + 1);
    // An exclusion-only query (no required clauses) starts from all documents
    // and removes the excluded matches; a boolean query needs a positive clause.
    if query.required.is_empty() {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }
    for clause in &query.required {
        clauses.push((
            required_occur,
            clause_query(clause, fields, &query.boosts, &query.synonyms),
        ));
    }
    for clause in &query.excluded {
        clauses.push((
            Occur::MustNot,
            clause_query(clause, fields, &query.boosts, &query.synonyms),
        ));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// Looks up the score boost for a field (`1.0` when no weight was given).
fn field_boost(boosts: &BTreeMap<String, f32>, field: &str) -> f32 {
    boosts.get(field).copied().unwrap_or(1.0)
}

/// Wraps a per-field query in a boost when the field carries a weight.
fn maybe_boost(query: Box<dyn Query>, boost: f32) -> Box<dyn Query> {
    if (boost - 1.0).abs() < f32::EPSILON {
        query
    } else {
        Box::new(BoostQuery::new(query, boost))
    }
}

/// Builds the per-field OR query for a single clause. Each field's tokens are
/// analyzed with that field's own analyzer, so a `keyword`-analyzed field
/// matches its verbatim value and an English-analyzed field matches stemmed
/// forms. A `Term` clause also OR-matches its synonym expansions (raw
/// `(term, expansions)` pairs; expansions are analyzed per field like the
/// term itself). Fuzzy and phrase clauses do not expand.
/// Builds the per-field `Should` clauses for a clause: for each in-scope
/// field, the queries `per_field` builds for it, each wrapped in the
/// field's boost. Returns `EmptyQuery` when no field produces a query.
fn per_field_queries(
    fields: &[SearchableField],
    boosts: &BTreeMap<String, f32>,
    per_field: impl Fn(&SearchableField) -> Vec<Box<dyn Query>>,
) -> Box<dyn Query> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for searchable in fields {
        let boost = field_boost(boosts, &searchable.name);
        for query in per_field(searchable) {
            clauses.push((Occur::Should, maybe_boost(query, boost)));
        }
    }
    if clauses.is_empty() {
        Box::new(EmptyQuery)
    } else {
        Box::new(BooleanQuery::new(clauses))
    }
}

fn clause_query(
    clause: &Clause,
    fields: &[SearchableField],
    boosts: &BTreeMap<String, f32>,
    synonyms: &[(String, Vec<String>)],
) -> Box<dyn Query> {
    match clause {
        Clause::Term(term) => per_field_queries(fields, boosts, |searchable| {
            let mut queries: Vec<Box<dyn Query>> = Vec::new();
            for token in analyze_with(term, searchable.analyzer.as_deref()) {
                let term = Term::from_field_text(searchable.field, &token);
                queries.push(Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)));
            }
            // Synonym expansions for this term, analyzed with the same
            // field analyzer so `keyword` fields match verbatim forms and
            // English fields match stemmed forms.
            if let Some((_, expansions)) = synonyms.iter().find(|(raw, _)| raw == term) {
                for expansion in expansions {
                    for token in analyze_with(expansion, searchable.analyzer.as_deref()) {
                        let term = Term::from_field_text(searchable.field, &token);
                        queries.push(Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)));
                    }
                }
            }
            queries
        }),
        Clause::FuzzyTerm { term, distance } => {
            // Azure lowercases fuzzy terms but bypasses analysis (no stemming,
            // no stopword removal, no punctuation splitting), so the raw
            // lowercased term is the single fuzzy token.
            let token = term.to_lowercase();
            if token.is_empty() {
                return Box::new(EmptyQuery);
            }
            per_field_queries(fields, boosts, |searchable| {
                let term = Term::from_field_text(searchable.field, &token);
                vec![Box::new(FuzzyTermQuery::new(term, *distance, true))]
            })
        }
        Clause::Phrase(phrase) => per_field_queries(fields, boosts, |searchable| {
            let tokens = analyze_with(phrase, searchable.analyzer.as_deref());
            if tokens.is_empty() {
                return Vec::new();
            }
            let terms = tokens
                .iter()
                .map(|token| Term::from_field_text(searchable.field, token))
                .collect();
            vec![Box::new(PhraseQuery::new(terms))]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::analyzers::{
        ANALYZER_ALPHANUM, ANALYZER_CJK, ANALYZER_EDGE_NGRAM, ANALYZER_ENGLISH, ANALYZER_FRENCH,
        ANALYZER_KEYWORD, ANALYZER_LATIN, ANALYZER_NGRAM, ANALYZER_WHITESPACE,
    };
    use super::*;
    use crate::storage::FieldType;
    use serde_json::{Map, Value};
    use std::collections::BTreeSet;

    fn fields() -> Vec<FieldDefinition> {
        vec![
            FieldDefinition {
                name: "id".to_owned(),
                field_type: FieldType::String,
                is_key: true,
                searchable: false,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "title".to_owned(),
                field_type: FieldType::String,
                is_key: false,
                searchable: true,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "body".to_owned(),
                field_type: FieldType::String,
                is_key: false,
                searchable: true,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "count".to_owned(),
                field_type: FieldType::Int32,
                is_key: false,
                searchable: false,
                filterable: true,
                sortable: true,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "price".to_owned(),
                field_type: FieldType::Double,
                is_key: false,
                searchable: true,
                filterable: true,
                sortable: true,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
        ]
    }

    fn document(key: &str, title: &str, body: &str, price: i64) -> Document {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(key.to_owned()));
        map.insert("title".to_owned(), Value::String(title.to_owned()));
        map.insert("body".to_owned(), Value::String(body.to_owned()));
        map.insert("count".to_owned(), Value::Number(3.into()));
        map.insert("price".to_owned(), Value::from(price));
        Document {
            key: key.to_owned(),
            fields: map,
        }
    }

    fn engine_with_docs() -> SearchEngine {
        let engine = SearchEngine::new();
        engine
            .create_index("items", &fields())
            .unwrap_or_else(|e| panic!("create_index failed: {e}"));
        engine
            .index_documents(
                "items",
                &[
                    // Prices are 3-digit so their tokens (e.g. "100") stay
                    // more than 2 edits away from the fuzzy test terms.
                    document("1", "Azure Search", "the quick brown fox", 100),
                    document("2", "Azure Emulators", "lazy dogs", 200),
                    document("3", "Other", "nothing relevant", 300),
                ],
            )
            .unwrap_or_else(|e| panic!("index_documents failed: {e}"));
        engine
    }

    fn keys(engine: &SearchEngine, term: &str) -> Vec<String> {
        let query =
            parse_search_text(term).unwrap_or_else(|e| panic!("parse failed for {term:?}: {e}"));
        let mut keys: Vec<String> = engine
            .search("items", &query)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_keys()
            .collect();
        keys.sort();
        keys
    }

    fn scored(engine: &SearchEngine, term: &str) -> Vec<(String, f32)> {
        let query =
            parse_search_text(term).unwrap_or_else(|e| panic!("parse failed for {term:?}: {e}"));
        let mut pairs: Vec<(String, f32)> = engine
            .search("items", &query)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_iter()
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    #[test]
    fn empty_and_wildcard_match_all() {
        let engine = engine_with_docs();
        assert_eq!(keys(&engine, ""), vec!["1", "2", "3"]);
        assert_eq!(keys(&engine, "  "), vec!["1", "2", "3"]);
        assert_eq!(keys(&engine, "*"), vec!["1", "2", "3"]);
    }

    #[test]
    fn case_insensitive_token_match() {
        let engine = engine_with_docs();
        assert_eq!(keys(&engine, "azure"), vec!["1", "2"]);
        assert_eq!(keys(&engine, "SEARCH"), vec!["1"]);
        assert_eq!(keys(&engine, "quick"), vec!["1"]);
        assert!(keys(&engine, "horse").is_empty());
    }

    #[test]
    fn multi_term_all_mode_requires_all_terms() {
        let engine = engine_with_docs();
        // Explicit `all` (AND): both terms must match.
        let mut all =
            parse_search_text("azure fox").unwrap_or_else(|e| panic!("parse failed: {e}"));
        all.mode = SearchMode::All;
        let mut keys: Vec<String> = engine
            .search("items", &all)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_keys()
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["1"]);
        // AND of a present and an absent term matches nothing.
        let mut none =
            parse_search_text("azure horse").unwrap_or_else(|e| panic!("parse failed: {e}"));
        none.mode = SearchMode::All;
        assert!(engine
            .search("items", &none)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_keys()
            .collect::<Vec<_>>()
            .is_empty());
    }

    #[test]
    fn numeric_fields_are_full_text_indexed() {
        let engine = engine_with_docs();
        // Numeric field values are full-text indexed like Azure: "100"
        // matches only the document whose `price` is 100.
        assert_eq!(keys(&engine, "100"), vec!["1"]);
        assert_eq!(keys(&engine, "200"), vec!["2"]);
        assert_eq!(keys(&engine, "300"), vec!["3"]);
        // The non-searchable `count` field (3 on every document) is not
        // full-text indexed.
        assert!(keys(&engine, "3").is_empty());
    }

    fn searchable_field(name: &str, field_type: FieldType) -> FieldDefinition {
        FieldDefinition {
            name: name.to_owned(),
            field_type,
            is_key: false,
            searchable: true,
            filterable: false,
            sortable: false,
            facetable: false,
            retrievable: true,
            stored: true,
            vector_dimensions: None,
            vector_search_profile: None,
            vectorizer: None,
            subfields: Vec::new(),
            analyzer: None,
            synonym_maps: Vec::new(),
            raw: Value::Null,
        }
    }

    #[test]
    fn non_string_fields_are_full_text_indexed() {
        let fields = vec![
            FieldDefinition {
                name: "id".to_owned(),
                field_type: FieldType::String,
                is_key: true,
                searchable: false,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                stored: true,
                vector_dimensions: None,
                vector_search_profile: None,
                vectorizer: None,
                subfields: Vec::new(),
                analyzer: None,
                synonym_maps: Vec::new(),
                raw: Value::Null,
            },
            searchable_field("flag", FieldType::Boolean),
            searchable_field("created", FieldType::DateTimeOffset),
            searchable_field("guid", FieldType::Guid),
            searchable_field("small", FieldType::Int32),
            searchable_field("big", FieldType::Int64),
            searchable_field("scores", FieldType::CollectionInt32),
        ];
        let engine = SearchEngine::new();
        engine
            .create_index("items", &fields)
            .unwrap_or_else(|e| panic!("create_index failed: {e}"));
        let mut fields1 = Map::new();
        fields1.insert("id".to_owned(), Value::String("1".to_owned()));
        fields1.insert("flag".to_owned(), Value::Bool(true));
        fields1.insert(
            "created".to_owned(),
            Value::String("2024-01-15T10:30:00Z".to_owned()),
        );
        fields1.insert(
            "guid".to_owned(),
            Value::String("a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_owned()),
        );
        fields1.insert("small".to_owned(), Value::from(42));
        fields1.insert("big".to_owned(), Value::from(9_999_999_999_i64));
        fields1.insert(
            "scores".to_owned(),
            Value::Array(vec![Value::from(10), Value::from(20), Value::from(30)]),
        );
        let mut fields2 = Map::new();
        fields2.insert("id".to_owned(), Value::String("2".to_owned()));
        fields2.insert("flag".to_owned(), Value::Bool(false));
        fields2.insert(
            "created".to_owned(),
            Value::String("2025-06-20T14:45:00Z".to_owned()),
        );
        fields2.insert(
            "guid".to_owned(),
            Value::String("f1e2d3c4-b5a6-7890-1234-567890abcdef".to_owned()),
        );
        fields2.insert("small".to_owned(), Value::from(99));
        fields2.insert("big".to_owned(), Value::from(12_345_678_901_234_i64));
        fields2.insert(
            "scores".to_owned(),
            Value::Array(vec![Value::from(40), Value::from(50)]),
        );
        let docs = vec![
            Document {
                key: "1".to_owned(),
                fields: fields1,
            },
            Document {
                key: "2".to_owned(),
                fields: fields2,
            },
        ];
        engine
            .index_documents("items", &docs)
            .unwrap_or_else(|e| panic!("index_documents failed: {e}"));
        // Boolean: "true" matches doc 1, "false" matches doc 2.
        assert_eq!(keys(&engine, "true"), vec!["1"]);
        assert_eq!(keys(&engine, "false"), vec!["2"]);
        // DateTimeOffset: the ISO 8601 string is tokenized; "2024" matches doc 1.
        assert_eq!(keys(&engine, "2024"), vec!["1"]);
        assert_eq!(keys(&engine, "2025"), vec!["2"]);
        // Guid: the string representation is indexed; a unique fragment matches.
        assert_eq!(keys(&engine, "a1b2c3d4"), vec!["1"]);
        assert_eq!(keys(&engine, "f1e2d3c4"), vec!["2"]);
        // Int32: "42" matches doc 1, "99" matches doc 2.
        assert_eq!(keys(&engine, "42"), vec!["1"]);
        assert_eq!(keys(&engine, "99"), vec!["2"]);
        // Int64: large values are indexed as their string form.
        assert_eq!(keys(&engine, "9999999999"), vec!["1"]);
        assert_eq!(keys(&engine, "12345678901234"), vec!["2"]);
        // Collection(Edm.Int32): each element is indexed individually.
        assert_eq!(keys(&engine, "10"), vec!["1"]);
        assert_eq!(keys(&engine, "50"), vec!["2"]);
    }

    #[test]
    fn upsert_replaces_existing_key() {
        let engine = engine_with_docs();
        engine
            .index_documents(
                "items",
                &[document(
                    "1",
                    "Completely Different",
                    "no shared tokens",
                    100,
                )],
            )
            .unwrap_or_else(|e| panic!("index_documents failed: {e}"));
        assert!(keys(&engine, "azure").contains(&"2".to_owned()));
        assert!(!keys(&engine, "azure").contains(&"1".to_owned()));
        // The document count for a match-all is unchanged (still 3 docs).
        assert_eq!(keys(&engine, "*").len(), 3);
    }

    #[test]
    fn delete_index_removes_search_state() {
        let engine = engine_with_docs();
        engine.delete_index("items");
        let query = FullTextQuery::default();
        assert!(matches!(
            engine.search("items", &query),
            Err(QueryError::IndexNotFound(_))
        ));
    }

    #[test]
    fn reset_clears_all_indexes() {
        let engine = engine_with_docs();
        engine.reset();
        let query = FullTextQuery::default();
        assert!(matches!(
            engine.search("items", &query),
            Err(QueryError::IndexNotFound(_))
        ));
    }

    #[test]
    fn analyze_uses_english_analyzer_with_stemming_and_stopwords() {
        assert_eq!(analyze("Hello  WORLD"), vec!["hello", "world"]);
        assert!(analyze("").is_empty());
        // Punctuation splits: the analyzer (not whitespace) defines tokens.
        assert_eq!(analyze("hello-world"), vec!["hello", "world"]);
        // English stemming: inflected forms reduce to their stem.
        assert_eq!(analyze("running"), vec!["run"]);
        assert_eq!(analyze("searches"), vec!["search"]);
        // English stopwords are removed.
        assert!(analyze("the").is_empty());
        assert_eq!(analyze("the quick fox"), vec!["quick", "fox"]);
    }

    #[test]
    fn hyphenated_query_matches_split_tokens() {
        let engine = engine_with_docs();
        // "brown-fox" analyzes to ["brown", "fox"], matching document 1.
        // A whitespace-only split would look up "brown-fox" and miss.
        assert_eq!(keys(&engine, "brown-fox"), vec!["1"]);
    }

    #[test]
    fn excluded_terms_remove_matches() {
        let engine = engine_with_docs();
        // "azure -emulators" matches doc 1 (azure search) but not doc 2.
        assert_eq!(keys(&engine, "azure -emulators"), vec!["1"]);
        // Exclusion only: everything except docs containing "azure".
        assert_eq!(keys(&engine, "-azure"), vec!["3"]);
        // A term excluded and required cancels to no matches.
        assert!(keys(&engine, "azure +azure -azure").is_empty());
    }

    #[test]
    fn explicit_plus_is_the_default() {
        let engine = engine_with_docs();
        // `+term` is treated like a plain term (both go to the required list),
        // so under the OR default both match either document.
        assert_eq!(keys(&engine, "+azure +fox"), vec!["1", "2"]);
        assert_eq!(keys(&engine, "azure fox"), vec!["1", "2"]);
    }

    #[test]
    fn quoted_phrases_require_adjacent_tokens() {
        let engine = engine_with_docs();
        assert_eq!(keys(&engine, r#""quick brown""#), vec!["1"]);
        // Tokens present but not adjacent.
        assert!(keys(&engine, r#""brown quick""#).is_empty());
        // Phrase combined with a term: OR (the default) matches either.
        assert_eq!(keys(&engine, r#"azure "lazy dogs""#), vec!["1", "2"]);
    }

    #[test]
    fn field_specific_search_restricts_scope() {
        let engine = engine_with_docs();
        let query = FullTextQuery {
            required: vec![Clause::Term("azure".to_owned())],
            ..Default::default()
        };
        // Across all fields: docs 1 and 2 (title).
        assert_eq!(
            engine
                .search("items", &query)
                .map(|k| k.into_keys().collect::<Vec<_>>()),
            Ok(vec!["1".to_owned(), "2".to_owned()])
        );
        // Restricted to `body`: no match ("azure" only appears in titles).
        let scoped = FullTextQuery {
            required: vec![Clause::Term("azure".to_owned())],
            fields: Some(vec!["body".to_owned()]),
            ..Default::default()
        };
        assert!(engine.search("items", &scoped).is_ok_and(|k| k.is_empty()));
        // Restricted to `title`: both docs.
        let scoped = FullTextQuery {
            required: vec![Clause::Term("azure".to_owned())],
            fields: Some(vec!["title".to_owned()]),
            ..Default::default()
        };
        assert_eq!(
            engine
                .search("items", &scoped)
                .map(|k| k.into_keys().collect::<Vec<_>>()),
            Ok(vec!["1".to_owned(), "2".to_owned()])
        );
    }

    #[test]
    fn parse_search_text_rejects_dangling_modifiers() {
        assert!(parse_search_text("+").is_err());
        assert!(parse_search_text("-").is_err());
        assert!(parse_search_text("azure +").is_err());
        assert!(parse_search_text(r#""unterminated"#).is_err());
        let query =
            parse_search_text(r#"a "b""c" -d"#).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(
            query.required,
            vec![
                Clause::Term("a".to_owned()),
                Clause::Phrase("b\"c".to_owned())
            ]
        );
        assert_eq!(query.excluded, vec![Clause::Term("d".to_owned())]);
    }

    #[test]
    fn search_mode_any_matches_union() {
        let engine = engine_with_docs();
        // Default (any/OR, matching Azure): either term matches.
        assert_eq!(keys(&engine, "azure fox"), vec!["1", "2"]);
        // All (AND): both terms must match.
        let mut all =
            parse_search_text("azure fox").unwrap_or_else(|e| panic!("parse failed: {e}"));
        all.mode = SearchMode::All;
        let mut keys: Vec<String> = engine
            .search("items", &all)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_keys()
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["1"]);
        // SearchMode parsing accepts both values case-insensitively.
        assert_eq!(SearchMode::parse("all"), Ok(SearchMode::All));
        assert_eq!(SearchMode::parse("ANY"), Ok(SearchMode::Any));
        assert!(SearchMode::parse("both").is_err());
    }

    #[test]
    fn stemming_matches_inflected_forms() {
        let engine = engine_with_docs();
        // "dogs" stems to "dog", matching the indexed "dogs".
        assert_eq!(keys(&engine, "dog"), vec!["2"]);
        assert_eq!(keys(&engine, "dogs"), vec!["2"]);
        // "search" stems the same as indexed "Search".
        assert_eq!(keys(&engine, "searches"), vec!["1"]);
    }

    #[test]
    fn stopwords_do_not_match() {
        let engine = engine_with_docs();
        // "the" is an English stopword: removed at index and query time.
        assert!(keys(&engine, "the").is_empty());
        // A stopword-only query matches nothing (not everything).
        let query = parse_search_text("the").unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert!(engine.search("items", &query).is_ok_and(|m| m.is_empty()));
    }

    #[test]
    fn fuzzy_terms_match_within_edit_distance() {
        let engine = engine_with_docs();
        // Fuzzy terms are lowercased only (no stemming): "emul" matches the
        // indexed stemmed "emul" (from "Emulators") exactly.
        assert_eq!(keys(&engine, "emul~"), vec!["2"]);
        // "emulator" is NOT stemmed for fuzzy matching, so it is 4 edits from
        // the indexed "emul" and matches nothing (Azure lowercases fuzzy
        // terms only; a stemmed query would have matched).
        assert!(keys(&engine, "emulator~").is_empty());
        // Fuzzy terms are case-insensitive: "FOX~1" matches indexed "fox".
        assert_eq!(keys(&engine, "FOX~1"), vec!["1"]);
        // One substitution away from indexed "fox" ("box" is also two away
        // from indexed "dog", so the default distance matches both).
        assert_eq!(keys(&engine, "box~1"), vec!["1"]);
        assert_eq!(keys(&engine, "box~"), vec!["1", "2"]);
        // One insertion away from indexed "fox" ("fo" is also two away from
        // indexed "dog", so the default distance matches both).
        assert_eq!(keys(&engine, "fo~1"), vec!["1"]);
        assert_eq!(keys(&engine, "fo~"), vec!["1", "2"]);
        // A bare `~` uses the default edit distance 2 (matching Azure): "qik"
        // is two deletions away from indexed "quick".
        assert_eq!(keys(&engine, "qik~"), vec!["1"]);
        assert!(keys(&engine, "qik~1").is_empty());
        // An unrelated term still matches nothing, even fuzzy.
        assert!(keys(&engine, "zzz~").is_empty());
        // Explicit distance suffixes parse ("~" defaults to 2).
        let query = parse_search_text("emulator~").unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(
            query.required,
            vec![Clause::FuzzyTerm {
                term: "emulator".to_owned(),
                distance: 2
            }]
        );
        let query = parse_search_text("emulator~1").unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(
            query.required,
            vec![Clause::FuzzyTerm {
                term: "emulator".to_owned(),
                distance: 1
            }]
        );
        // Distance above 2 is rejected explicitly.
        assert!(parse_search_text("emulator~3").is_err());
        // A bare `~` with no term is rejected.
        assert!(parse_search_text("~").is_err());
    }

    #[test]
    fn analyze_endpoint_analyzers_tokenize() {
        // The default (English) analyzer stems and drops stopwords.
        assert_eq!(
            analyze_with_offsets_and_analyzer("Running foxes", None)
                .iter()
                .map(|t| t.token.clone())
                .collect::<Vec<_>>(),
            vec!["run", "fox"]
        );
        // `keyword` emits the whole input as one verbatim token.
        let tokens = analyze_with_offsets_and_analyzer("Running foxes", Some("keyword"));
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token, "Running foxes");
        assert_eq!(tokens[0].start_offset, 0);
        assert_eq!(tokens[0].end_offset, "Running foxes".len());
        assert!(analyze_with_offsets_and_analyzer("", Some("keyword")).is_empty());
        // `whitespace` splits without lowercasing or stemming.
        let tokens = analyze_with_offsets_and_analyzer("Running  foxes", Some("whitespace"));
        assert_eq!(
            tokens.iter().map(|t| t.token.clone()).collect::<Vec<_>>(),
            vec!["Running", "foxes"]
        );
        assert_eq!(tokens[0].start_offset, 0);
        assert_eq!(tokens[0].end_offset, "Running".len());
        assert_eq!(tokens[1].start_offset, "Running  ".len());
    }

    #[test]
    fn search_returns_positive_bm25_scores() {
        let engine = engine_with_docs();
        let pairs = scored(&engine, "azure");
        assert_eq!(pairs.len(), 2);
        for (_, score) in &pairs {
            assert!(*score > 0.0, "BM25 scores must be positive, got {score}");
        }
    }

    #[test]
    fn field_boosts_change_scores_not_matches() {
        let engine = engine_with_docs();
        let plain = FullTextQuery {
            required: vec![Clause::Term("azure".to_owned())],
            ..Default::default()
        };
        let plain_scores = engine
            .search("items", &plain)
            .unwrap_or_else(|e| panic!("search failed: {e}"));
        let boosted = FullTextQuery {
            required: vec![Clause::Term("azure".to_owned())],
            boosts: BTreeMap::from([("title".to_owned(), 2.0)]),
            ..Default::default()
        };
        let boosted_scores = engine
            .search("items", &boosted)
            .unwrap_or_else(|e| panic!("search failed: {e}"));
        // Same matches...
        assert_eq!(
            plain_scores.keys().collect::<BTreeSet<_>>(),
            boosted_scores.keys().collect::<BTreeSet<_>>()
        );
        // ...but boosted scores are higher.
        for key in plain_scores.keys() {
            assert!(
                boosted_scores[key] > plain_scores[key],
                "boosted score for {key} should exceed plain score"
            );
        }
    }

    // ------------------------------------------------------------------
    // Per-field analyzers
    // ------------------------------------------------------------------

    fn field(
        name: &str,
        field_type: &str,
        searchable: bool,
        analyzer: Option<&str>,
    ) -> FieldDefinition {
        FieldDefinition {
            name: name.to_owned(),
            field_type: FieldType::from_normalized(field_type),
            is_key: false,
            searchable,
            filterable: false,
            sortable: false,
            facetable: false,
            retrievable: true,
            stored: true,
            vector_dimensions: None,
            vector_search_profile: None,
            vectorizer: None,
            subfields: Vec::new(),
            analyzer: analyzer.map(str::to_owned),
            synonym_maps: Vec::new(),
            raw: Value::Null,
        }
    }

    fn key_field() -> FieldDefinition {
        FieldDefinition {
            name: "id".to_owned(),
            field_type: FieldType::String,
            is_key: true,
            searchable: false,
            filterable: false,
            sortable: false,
            facetable: false,
            retrievable: true,
            stored: true,
            vector_dimensions: None,
            vector_search_profile: None,
            vectorizer: None,
            subfields: Vec::new(),
            analyzer: None,
            synonym_maps: Vec::new(),
            raw: Value::Null,
        }
    }

    fn analyzer_engine(fields: &[FieldDefinition], docs: &[Document]) -> SearchEngine {
        let engine = SearchEngine::new();
        engine
            .create_index("items", fields)
            .unwrap_or_else(|e| panic!("create_index failed: {e}"));
        engine
            .index_documents("items", docs)
            .unwrap_or_else(|e| panic!("index_documents failed: {e}"));
        engine
    }

    fn doc_with(key: &str, pairs: &[(&str, Value)]) -> Document {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(key.to_owned()));
        for (k, v) in pairs {
            map.insert(k.to_string(), v.clone());
        }
        Document {
            key: key.to_owned(),
            fields: map,
        }
    }

    fn search_keys(engine: &SearchEngine, query: &FullTextQuery) -> Vec<String> {
        let mut keys: Vec<String> = engine
            .search("items", query)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_keys()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn analyzer_tokenizer_name_maps_known_names() {
        assert_eq!(analyzer_tokenizer_name(None), ANALYZER_ENGLISH);
        assert_eq!(analyzer_tokenizer_name(Some("standard")), ANALYZER_ENGLISH);
        assert_eq!(
            analyzer_tokenizer_name(Some("en.microsoft")),
            ANALYZER_ENGLISH
        );
        assert_eq!(analyzer_tokenizer_name(Some("keyword")), ANALYZER_KEYWORD);
        assert_eq!(
            analyzer_tokenizer_name(Some("whitespace")),
            ANALYZER_WHITESPACE
        );
        assert_eq!(analyzer_tokenizer_name(Some("alphanum")), ANALYZER_ALPHANUM);
        assert_eq!(analyzer_tokenizer_name(Some("latin")), ANALYZER_LATIN);
        assert_eq!(analyzer_tokenizer_name(Some("ngram")), ANALYZER_NGRAM);
        assert_eq!(
            analyzer_tokenizer_name(Some("edgeNgram")),
            ANALYZER_EDGE_NGRAM
        );
        assert_eq!(analyzer_tokenizer_name(Some("chinese")), ANALYZER_CJK);
        assert_eq!(
            analyzer_tokenizer_name(Some("fr.microsoft")),
            ANALYZER_FRENCH
        );
        // Unknown names fall back to English.
        assert_eq!(
            analyzer_tokenizer_name(Some("custom.analyzer")),
            ANALYZER_ENGLISH
        );
    }

    #[test]
    fn keyword_analyzer_matches_verbatim_only() {
        let engine = analyzer_engine(
            &[
                key_field(),
                field("code", "Edm.String", true, Some("keyword")),
            ],
            &[
                doc_with("1", &[("code", Value::String("ABC-123".to_owned()))]),
                doc_with("2", &[("code", Value::String("abc 123".to_owned()))]),
            ],
        );
        // The whole value is a single token: the exact value matches.
        let exact = FullTextQuery {
            required: vec![Clause::Term("ABC-123".to_owned())],
            ..Default::default()
        };
        assert_eq!(search_keys(&engine, &exact), vec!["1"]);
        // A sub-token does not match (no splitting, no lowercasing).
        let part = FullTextQuery {
            required: vec![Clause::Term("abc".to_owned())],
            ..Default::default()
        };
        assert!(search_keys(&engine, &part).is_empty());
        // Case is preserved: lowercase does not match the uppercase value.
        let lower = FullTextQuery {
            required: vec![Clause::Term("abc-123".to_owned())],
            ..Default::default()
        };
        assert!(search_keys(&engine, &lower).is_empty());
    }

    #[test]
    fn whitespace_analyzer_splits_without_lowercasing() {
        assert_eq!(
            analyze_with("Hello  WORLD", Some("whitespace")),
            vec!["Hello", "WORLD"]
        );
        // No stemming or stopword removal.
        assert_eq!(
            analyze_with("running the", Some("whitespace")),
            vec!["running", "the"]
        );
    }

    #[test]
    fn latin_analyzer_lowercases_without_stemming() {
        assert_eq!(analyze_with("Running", Some("latin")), vec!["running"]);
        // No stemming (unlike the English analyzer).
        assert_eq!(analyze_with("running", Some("latin")), vec!["running"]);
        assert_eq!(analyze_with("searches", Some("latin")), vec!["searches"]);
    }

    #[test]
    fn cjk_analyzer_emits_bigrams() {
        // A 3-character Han run yields two overlapping bigrams.
        assert_eq!(
            analyze_with("北京天", Some("chinese")),
            vec!["北京", "京天"]
        );
        // A single Han character yields a unigram.
        assert_eq!(analyze_with("中", Some("chinese")), vec!["中"]);
        // Mixed CJK + Latin: the Latin run is a verbatim token.
        assert_eq!(
            analyze_with("北京abc", Some("chinese")),
            vec!["北京", "abc"]
        );
    }

    #[test]
    fn cjk_field_search_matches_bigram() {
        let engine = analyzer_engine(
            &[
                key_field(),
                field("name", "Edm.String", true, Some("chinese")),
            ],
            &[
                doc_with("1", &[("name", Value::String("北京大学".to_owned()))]),
                doc_with("2", &[("name", Value::String("清华大学".to_owned()))]),
            ],
        );
        // "清华" is a bigram in document 2 only.
        let query = FullTextQuery {
            required: vec![Clause::Term("清华".to_owned())],
            ..Default::default()
        };
        assert_eq!(search_keys(&engine, &query), vec!["2"]);
        // "大学" is a bigram in both documents.
        let query = FullTextQuery {
            required: vec![Clause::Term("大学".to_owned())],
            ..Default::default()
        };
        assert_eq!(search_keys(&engine, &query), vec!["1", "2"]);
    }

    // ------------------------------------------------------------------
    // Lucene (queryType=full) queries
    // ------------------------------------------------------------------

    fn lucene_query(text: &str) -> FullTextQuery {
        FullTextQuery {
            lucene: Some(text.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn lucene_and_or_not_operators() {
        let engine = engine_with_docs();
        // `azure AND fox`: both terms must match (document 1).
        assert_eq!(
            search_keys(&engine, &lucene_query("azure AND fox")),
            vec!["1"]
        );
        // `azure OR fox`: either term (documents 1 and 2).
        assert_eq!(
            search_keys(&engine, &lucene_query("azure OR fox")),
            vec!["1", "2"]
        );
        // `azure NOT emulators`: azure minus document 2.
        assert_eq!(
            search_keys(&engine, &lucene_query("azure NOT emulators")),
            vec!["1"]
        );
        // `+azure -emulators`: modifier form.
        assert_eq!(
            search_keys(&engine, &lucene_query("+azure -emulators")),
            vec!["1"]
        );
    }

    #[test]
    fn lucene_field_scoped_query() {
        let engine = engine_with_docs();
        // `title:azure` matches both titles; `body:azure` matches nothing.
        assert_eq!(
            search_keys(&engine, &lucene_query("title:azure")),
            vec!["1", "2"]
        );
        assert!(search_keys(&engine, &lucene_query("body:azure")).is_empty());
        // `body:fox` matches document 1 only.
        assert_eq!(search_keys(&engine, &lucene_query("body:fox")), vec!["1"]);
    }

    #[test]
    fn lucene_fuzzy_and_wildcard() {
        let engine = engine_with_docs();
        // `fo*` wildcard prefix matches "fox" (document 1).
        assert_eq!(search_keys(&engine, &lucene_query("fo*")), vec!["1"]);
        // `emul~` fuzzy (default distance 2) matches "emul" (document 2).
        assert_eq!(search_keys(&engine, &lucene_query("emul~")), vec!["2"]);
    }

    #[test]
    fn lucene_phrase_query() {
        let engine = engine_with_docs();
        assert_eq!(
            search_keys(&engine, &lucene_query(r#""quick brown""#)),
            vec!["1"]
        );
        assert!(search_keys(&engine, &lucene_query(r#""brown quick""#)).is_empty());
    }

    #[test]
    fn lucene_empty_and_wildcard_match_all() {
        let engine = engine_with_docs();
        assert_eq!(search_keys(&engine, &lucene_query("")), vec!["1", "2", "3"]);
        assert_eq!(
            search_keys(&engine, &lucene_query("*")),
            vec!["1", "2", "3"]
        );
    }

    #[test]
    fn lucene_invalid_query_is_rejected() {
        let engine = engine_with_docs();
        // Unknown field.
        let result = engine.search("items", &lucene_query("missing:azure"));
        assert!(matches!(result, Err(QueryError::InvalidQuery(_))));
        // Unbalanced quote.
        let result = engine.search("items", &lucene_query(r#""unterminated"#));
        assert!(matches!(result, Err(QueryError::InvalidQuery(_))));
    }

    #[test]
    fn lucene_query_terms_extraction() {
        assert_eq!(
            lucene_query_terms("title:azure -fox AND \"quick brown\"~1 bar^2 baz*"),
            vec!["azure", "fox", "quick", "brown", "bar", "baz"]
        );
        // Operators and wildcards-only tokens are dropped.
        assert_eq!(lucene_query_terms("AND OR NOT *"), Vec::<String>::new());
    }
}
