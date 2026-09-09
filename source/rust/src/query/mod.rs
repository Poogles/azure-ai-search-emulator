//! Phase 1 query engine: full-text search backed by [Tantivy].
//!
//! Tantivy (<https://github.com/quickwit-oss/tantivy>) is a Rust full-text
//! search library modelled on Apache Lucene. It is used as an embedded, in-process
//! dependency rather than rolling our own full-text search (see
//! `docs/decisions/0003-search-engine.md`).
//!
//! Semantics (see `docs/phase_1_scaffold_and_e2e.md`):
//! - Token-based full-text match across all `searchable: true` string fields,
//!   using Tantivy's default (English) analyzer.
//! - `*` or an empty search term matches all documents.
//! - A multi-term search term matches a document when every whitespace-separated
//!   term matches at least one searchable string field (Azure simple-query AND
//!   semantics).
//! - No boolean operators, field-specific search, or filters (Phase 2).
//!
//! The engine is responsible only for full-text matching. It returns the keys of
//! the matching documents; the service layer resolves those keys back to the full
//! stored documents, applies deterministic key ordering, and pages the results.

use std::collections::{BTreeMap, BTreeSet};

use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, EmptyQuery, Occur, Query, TermQuery};
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

/// Returns `true` when the search term matches no documents selectively, i.e.
/// it is empty or the match-all wildcard.
#[must_use]
pub fn is_match_all(term: &str) -> bool {
    let trimmed = term.trim();
    trimmed.is_empty() || trimmed == "*"
}

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
                if let Some(value) = document
                    .fields
                    .get(field_name)
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

    /// Runs a full-text search, returning the keys of all matching documents as
    /// a set for efficient lookup by the service layer.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::IndexNotFound`] if the index does not exist, or
    /// [`QueryError::Engine`] if a Tantivy operation fails.
    pub fn search(&self, name: &str, term: &str) -> Result<BTreeSet<String>, QueryError> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine = guard
            .get(name)
            .ok_or_else(|| QueryError::IndexNotFound(name.to_owned()))?;
        // A non-match-all term cannot match when there are no searchable fields.
        if !is_match_all(term) && engine.searchable.is_empty() {
            return Ok(BTreeSet::new());
        }
        let query = build_query(term, &engine.searchable);
        let searcher = engine.reader.searcher();
        let top_docs = searcher
            .search(&*query, &TopDocs::with_limit(MAX_MATCHES).order_by_score())
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
/// tokenized text field per `searchable: true` field.
fn build_schema(fields: &[FieldDefinition]) -> (Schema, Field, Vec<(String, Field)>) {
    let mut builder = Schema::builder();
    let key_field = builder.add_text_field(KEY_FIELD_NAME, STRING | STORED);
    let mut searchable = Vec::new();
    for field in fields {
        if field.searchable {
            let tantivy_field = builder.add_text_field(&field.name, TEXT);
            searchable.push((field.name.clone(), tantivy_field));
        }
    }
    (builder.build(), key_field, searchable)
}

/// Builds the Tantivy query for a search term: match-all for `*`/empty,
/// otherwise an AND over analyzer-produced tokens, each token an OR over all
/// searchable fields. A non-match-all term that analyzes to no tokens (e.g.
/// punctuation only) matches nothing.
fn build_query(term: &str, searchable: &[(String, Field)]) -> Box<dyn Query> {
    if is_match_all(term) {
        return Box::new(AllQuery);
    }
    let tokens = analyze(term);
    if tokens.is_empty() {
        return Box::new(EmptyQuery);
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(tokens.len());
    for token in &tokens {
        let field_clauses: Vec<(Occur, Box<dyn Query>)> = searchable
            .iter()
            .map(|(_, field)| {
                let term = Term::from_field_text(*field, token);
                let query: Box<dyn Query> =
                    Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                (Occur::Should, query)
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(field_clauses))));
    }
    Box::new(BooleanQuery::new(clauses))
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
        engine
            .search("items", term)
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
        assert!(matches!(
            engine.search("items", "*"),
            Err(QueryError::IndexNotFound(_))
        ));
    }

    #[test]
    fn reset_clears_all_indexes() {
        let engine = engine_with_docs();
        engine.reset();
        assert!(matches!(
            engine.search("items", "*"),
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
}
