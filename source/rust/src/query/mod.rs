//! Query engine: full-text search backed by [Tantivy].
//!
//! Tantivy (<https://github.com/quickwit-oss/tantivy>) is a Rust full-text
//! search library modelled on Apache Lucene. It is used as an embedded, in-process
//! dependency rather than rolling our own full-text search (see
//! `docs/decisions/0003-search-engine.md`).
//!
//! Semantics (see `docs/supported_operations.md`):
//! - Token-based full-text match across `searchable: true` string fields,
//!   using Tantivy's default (English) analyzer.
//! - `*` or an empty search term matches all documents.
//! - A multi-term search term matches a document when every required term
//!   matches at least one searchable string field (Azure simple-query AND
//!   semantics).
//! - Simple-query boolean operators: `+term` (required, the default),
//!   `-term` (excluded), and `"quoted phrases"`.
//! - Field-specific search via [`FullTextQuery::fields`].
//!
//! The engine is responsible only for full-text matching. It returns the keys of
//! the matching documents; the service layer resolves those keys back to the full
//! stored documents, applies filters, ordering, projection, and paging.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::Value as _;
use tantivy::schema::{Field, IndexRecordOption, Schema, STORED, STRING, TEXT};
use tantivy::tokenizer::{TokenStream, TokenizerManager};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::storage::{Document, FieldDefinition};

/// Reserved Tantivy field name used to store each document's key.
const KEY_FIELD_NAME: &str = "__aisearch_key";

/// Heap budget (bytes) for each per-index Tantivy writer.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Upper bound on the number of matching documents collected per search. The
/// emulator targets local development and testing, where index sizes are small.
const MAX_MATCHES: usize = 1_000_000;

/// Errors produced by the query engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The requested search index does not exist.
    IndexNotFound(String),
    /// An underlying Tantivy operation failed.
    Engine(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::IndexNotFound(name) => write!(f, "index {name:?} not found"),
            QueryError::Engine(message) => write!(f, "search engine error: {message}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Tokenizes search text with Tantivy's default analyzer, so query terms match
/// the analyzed terms stored in the index (same lowercasing and punctuation
/// splitting applied at index time). A hand-rolled whitespace split would
/// diverge — e.g. `hello-world` would not match indexed `hello` + `world`.
#[must_use]
pub fn analyze(text: &str) -> Vec<String> {
    let manager = TokenizerManager::default();
    let Some(mut analyzer) = manager.get("default") else {
        return Vec::new();
    };
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    tokens
}

/// A single token from the analyze-text operation, with position and offset
/// information matching the Azure `POST /indexes('{name}')/search.analyze`
/// response shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeToken {
    pub token: String,
    pub start_offset: usize,
    pub end_offset: usize,
    pub position: usize,
}

/// Tokenizes text with Tantivy's default analyzer, returning structured tokens
/// with offsets and positions (for the analyze-text endpoint).
#[must_use]
pub fn analyze_with_offsets(text: &str) -> Vec<AnalyzeToken> {
    let manager = TokenizerManager::default();
    let Some(mut analyzer) = manager.get("default") else {
        return Vec::new();
    };
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        let t = stream.token();
        tokens.push(AnalyzeToken {
            token: t.text.clone(),
            start_offset: t.offset_from,
            end_offset: t.offset_to,
            position: t.position,
        });
    }
    tokens
}

/// One clause of a parsed simple query: a single term or a quoted phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Term(String),
    Phrase(String),
}

/// A parsed full-text query: required and excluded clauses, optionally
/// restricted to a set of Azure field names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullTextQuery {
    pub required: Vec<Clause>,
    pub excluded: Vec<Clause>,
    /// Azure field names to restrict the search to; `None` means all
    /// `searchable` string fields.
    pub fields: Option<Vec<String>>,
}

impl FullTextQuery {
    /// Returns `true` when the query matches every document.
    #[must_use]
    pub fn is_match_all(&self) -> bool {
        self.required.is_empty() && self.excluded.is_empty()
    }
}

/// Parses a simple-query search text into required and excluded clauses.
///
/// Supports `+term` (required; the default), `-term` (excluded), and
/// `"quoted phrases"`. An empty text or `*` produces a match-all query.
///
/// # Errors
///
/// Returns an error string for an unterminated phrase or a `+`/`-` modifier
/// with no term.
pub fn parse_search_text(text: &str) -> Result<FullTextQuery, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Ok(FullTextQuery::default());
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut pos = 0usize;
    let mut query = FullTextQuery::default();
    while pos < chars.len() {
        if chars[pos].is_whitespace() {
            pos += 1;
            continue;
        }
        let mut sign: Option<char> = None;
        if chars[pos] == '+' || chars[pos] == '-' {
            sign = Some(chars[pos]);
            pos += 1;
            if pos >= chars.len() || chars[pos].is_whitespace() {
                return Err(
                    "Search modifier '+' or '-' must be followed by a search term.".to_owned(),
                );
            }
        }
        let clause = read_clause(&chars, &mut pos)?;
        match sign {
            Some('-') => query.excluded.push(clause),
            _ => query.required.push(clause),
        }
    }
    Ok(query)
}

/// Reads one clause starting at `chars[*pos]`, advancing `pos` past it.
fn read_clause(chars: &[char], pos: &mut usize) -> Result<Clause, String> {
    if chars[*pos] == '"' {
        *pos += 1;
        let mut text = String::new();
        loop {
            if *pos >= chars.len() {
                return Err("Unterminated quoted phrase in search text.".to_owned());
            }
            match chars[*pos] {
                '"' => {
                    // `""` escapes a literal quote inside a phrase.
                    if chars.get(*pos + 1) == Some(&'"') {
                        text.push('"');
                        *pos += 2;
                        continue;
                    }
                    *pos += 1;
                    return Ok(Clause::Phrase(text));
                }
                other => {
                    text.push(other);
                    *pos += 1;
                }
            }
        }
    }
    let start = *pos;
    while *pos < chars.len() && !chars[*pos].is_whitespace() {
        *pos += 1;
    }
    Ok(Clause::Term(chars[start..*pos].iter().collect()))
}

/// A single Tantivy-backed search index.
struct EngineIndex {
    reader: IndexReader,
    writer: IndexWriter,
    key_field: Field,
    /// Searchable fields as `(azure field name, tantivy field)` pairs.
    searchable: Vec<(String, Field)>,
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
        let reader = index
            .reader()
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.contains_key(name) {
            return Err(QueryError::Engine(format!(
                "search index {name:?} already exists"
            )));
        }
        guard.insert(
            name.to_owned(),
            EngineIndex {
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
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine = guard
            .get_mut(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        for document in documents {
            engine
                .writer
                .delete_term(Term::from_field_text(engine.key_field, &document.key));
            let mut tantivy_doc = TantivyDocument::new();
            tantivy_doc.add_text(engine.key_field, &document.key);
            for (field_name, field) in &engine.searchable {
                if let Some(value) = resolve_doc_path(&document.fields, field_name)
                    .and_then(serde_json::Value::as_str)
                {
                    tantivy_doc.add_text(*field, value);
                }
            }
            engine
                .writer
                .add_document(tantivy_doc)
                .map_err(|e| QueryError::Engine(e.to_string()))?;
        }
        engine
            .writer
            .commit()
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        // Reload synchronously so newly indexed documents are immediately
        // searchable (the emulator guarantees immediate consistency).
        engine
            .reader
            .reload()
            .map_err(|e| QueryError::Engine(e.to_string()))?;
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
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine = guard
            .get_mut(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        for key in keys {
            engine
                .writer
                .delete_term(Term::from_field_text(engine.key_field, key));
        }
        engine
            .writer
            .commit()
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        engine
            .reader
            .reload()
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        Ok(())
    }

    /// Runs a full-text search, returning the keys of all matching documents as
    /// a set for efficient lookup by the service layer.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::IndexNotFound`] if the index does not exist, or
    /// [`QueryError::Engine`] if a Tantivy operation fails.
    pub fn search(
        &self,
        name: &str,
        query: &FullTextQuery,
    ) -> Result<BTreeSet<String>, QueryError> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine = guard
            .get(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        let fields = match &query.fields {
            Some(names) => engine
                .searchable
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(_, field)| *field)
                .collect::<Vec<Field>>(),
            None => engine
                .searchable
                .iter()
                .map(|(_, field)| *field)
                .collect::<Vec<Field>>(),
        };
        // A non-match-all query cannot match when there are no searchable
        // fields in scope.
        if !query.is_match_all() && fields.is_empty() {
            return Ok(BTreeSet::new());
        }
        let tantivy_query = build_query(query, &fields);
        let searcher = engine.reader.searcher();
        let top_docs = searcher
            .search(
                &*tantivy_query,
                &TopDocs::with_limit(MAX_MATCHES).order_by_score(),
            )
            .map_err(|e| QueryError::Engine(e.to_string()))?;
        let mut keys = BTreeSet::new();
        for (_score, doc_address) in top_docs {
            if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                if let Some(key) = doc
                    .get_first(engine.key_field)
                    .and_then(|value| value.as_str())
                {
                    keys.insert(key.to_owned());
                }
            }
        }
        Ok(keys)
    }

    /// Clears all Tantivy indexes.
    pub fn reset(&self) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }
}

/// Builds the Tantivy schema for an emulator index: a stored key field plus one
/// tokenized text field per `searchable: true` field. Searchable complex-type
/// subfields are indexed under their field path (`Address/City`).
fn build_schema(fields: &[FieldDefinition]) -> (Schema, Field, Vec<(String, Field)>) {
    let mut builder = Schema::builder();
    let key_field = builder.add_text_field(KEY_FIELD_NAME, STRING | STORED);
    let mut searchable = Vec::new();
    collect_searchable(&mut builder, &mut searchable, "", fields);
    (builder.build(), key_field, searchable)
}

fn collect_searchable(
    builder: &mut tantivy::schema::SchemaBuilder,
    searchable: &mut Vec<(String, Field)>,
    prefix: &str,
    fields: &[FieldDefinition],
) {
    for field in fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}/{}", field.name)
        };
        if field.field_type == "Edm.ComplexType" {
            collect_searchable(builder, searchable, &path, &field.subfields);
        } else if field.searchable && !field.is_vector_field() {
            // Vector fields require `searchable: true` per Azure but are not
            // full-text indexed (their values are numeric arrays).
            let tantivy_field = builder.add_text_field(&path, TEXT);
            searchable.push((path, tantivy_field));
        }
    }
}

/// Resolves a field path (`Address/City`, or a plain field name) against a
/// document's field map, walking into complex-type objects.
fn resolve_doc_path<'a>(fields: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('/');
    let first = segments.next()?;
    let mut current = fields.get(first)?;
    for segment in segments {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// Builds the Tantivy query for a [`FullTextQuery`]: match-all when the query
/// has no clauses, otherwise a boolean combination where each required clause
/// is `Must` and each excluded clause is `MustNot`. Each clause is an OR over
/// the in-scope searchable fields (a term query per analyzer token, or a
/// phrase query for quoted phrases). A clause that analyzes to no tokens
/// (e.g. punctuation only) matches nothing.
fn build_query(query: &FullTextQuery, fields: &[Field]) -> Box<dyn Query> {
    if query.is_match_all() {
        return Box::new(AllQuery);
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        Vec::with_capacity(query.required.len() + query.excluded.len() + 1);
    // An exclusion-only query (no required clauses) starts from all documents
    // and removes the excluded matches; a boolean query needs a positive clause.
    if query.required.is_empty() {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }
    for clause in &query.required {
        clauses.push((Occur::Must, clause_query(clause, fields)));
    }
    for clause in &query.excluded {
        clauses.push((Occur::MustNot, clause_query(clause, fields)));
    }
    Box::new(BooleanQuery::new(clauses))
}

/// Builds the per-field OR query for a single clause.
fn clause_query(clause: &Clause, fields: &[Field]) -> Box<dyn Query> {
    match clause {
        Clause::Term(term) => {
            let tokens = analyze(term);
            if tokens.is_empty() {
                return Box::new(EmptyQuery);
            }
            let mut term_clauses: Vec<(Occur, Box<dyn Query>)> =
                Vec::with_capacity(tokens.len() * fields.len());
            for token in &tokens {
                for field in fields {
                    let term = Term::from_field_text(*field, token);
                    let query: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                    term_clauses.push((Occur::Should, query));
                }
            }
            Box::new(BooleanQuery::new(term_clauses))
        }
        Clause::Phrase(phrase) => {
            let tokens = analyze(phrase);
            if tokens.is_empty() {
                return Box::new(EmptyQuery);
            }
            let mut phrase_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fields.len());
            for field in fields {
                let terms = tokens
                    .iter()
                    .map(|token| Term::from_field_text(*field, token))
                    .collect();
                phrase_clauses.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
            }
            Box::new(BooleanQuery::new(phrase_clauses))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};

    fn fields() -> Vec<FieldDefinition> {
        vec![
            FieldDefinition {
                name: "id".to_owned(),
                field_type: "Edm.String".to_owned(),
                is_key: true,
                searchable: false,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "title".to_owned(),
                field_type: "Edm.String".to_owned(),
                is_key: false,
                searchable: true,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "body".to_owned(),
                field_type: "Edm.String".to_owned(),
                is_key: false,
                searchable: true,
                filterable: false,
                sortable: false,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                raw: Value::Null,
            },
            FieldDefinition {
                name: "count".to_owned(),
                field_type: "Edm.Int32".to_owned(),
                is_key: false,
                searchable: true,
                filterable: true,
                sortable: true,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                raw: Value::Null,
            },
        ]
    }

    fn document(key: &str, title: &str, body: &str) -> Document {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(key.to_owned()));
        map.insert("title".to_owned(), Value::String(title.to_owned()));
        map.insert("body".to_owned(), Value::String(body.to_owned()));
        map.insert("count".to_owned(), Value::Number(3.into()));
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
                    document("1", "Azure Search", "the quick brown fox"),
                    document("2", "Azure Emulators", "lazy dogs"),
                    document("3", "Other", "nothing relevant"),
                ],
            )
            .unwrap_or_else(|e| panic!("index_documents failed: {e}"));
        engine
    }

    fn keys(engine: &SearchEngine, term: &str) -> Vec<String> {
        let query =
            parse_search_text(term).unwrap_or_else(|e| panic!("parse failed for {term:?}: {e}"));
        engine
            .search("items", &query)
            .unwrap_or_else(|e| panic!("search failed: {e}"))
            .into_iter()
            .collect()
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
    fn multi_term_requires_all_terms() {
        let engine = engine_with_docs();
        assert_eq!(keys(&engine, "azure fox"), vec!["1"]);
        assert!(keys(&engine, "azure horse").is_empty());
    }

    #[test]
    fn non_string_fields_are_ignored() {
        let engine = engine_with_docs();
        // "3" only appears in the numeric `count` field, which is not indexed
        // as text.
        assert!(keys(&engine, "3").is_empty());
    }

    #[test]
    fn upsert_replaces_existing_key() {
        let engine = engine_with_docs();
        engine
            .index_documents(
                "items",
                &[document("1", "Completely Different", "no shared tokens")],
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
    fn analyze_uses_default_analyzer() {
        assert_eq!(analyze("Hello  WORLD"), vec!["hello", "world"]);
        assert!(analyze("").is_empty());
        // Punctuation splits: the analyzer (not whitespace) defines tokens.
        // The default analyzer does not stem or remove stopwords.
        assert_eq!(analyze("hello-world"), vec!["hello", "world"]);
        assert_eq!(analyze("running"), vec!["running"]);
        assert_eq!(analyze("the"), vec!["the"]);
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
        assert_eq!(keys(&engine, "+azure +fox"), vec!["1"]);
        assert_eq!(keys(&engine, "azure fox"), vec!["1"]);
    }

    #[test]
    fn quoted_phrases_require_adjacent_tokens() {
        let engine = engine_with_docs();
        assert_eq!(keys(&engine, r#""quick brown""#), vec!["1"]);
        // Tokens present but not adjacent.
        assert!(keys(&engine, r#""brown quick""#).is_empty());
        // Phrase combined with a required term.
        assert_eq!(keys(&engine, r#"azure "lazy dogs""#), vec!["2"]);
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
                .map(|k| k.into_iter().collect::<Vec<_>>()),
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
                .map(|k| k.into_iter().collect::<Vec<_>>()),
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
}
