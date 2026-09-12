//! Query engine: full-text search backed by [Tantivy].
//!
//! Tantivy (<https://github.com/quickwit-oss/tantivy>) is a Rust full-text
//! search library modelled on Apache Lucene. It is used as an embedded, in-process
//! dependency rather than rolling our own full-text search (see
//! `docs/decisions/0003-search-engine.md`).
//!
//! Semantics (see `docs/supported_operations.md`):
//! - Token-based full-text match across `searchable: true` string fields,
//!   using an English analyzer (lowercasing, punctuation splitting, English
//!   stopword removal, English stemming) approximating Azure's basic English
//!   analyzer.
//! - `*` or an empty search term matches all documents.
//! - A multi-term search combines required clauses with OR (`searchMode=any`,
//!   the default, matching Azure) or AND (`searchMode=all`).
//! - Simple-query boolean operators: `+term` (required, the default),
//!   `-term` (excluded), `"quoted phrases"`, and Lucene-style fuzzy terms
//!   (`term~` for the default edit distance 2, `term~1` for distance 1).
//! - Field-specific search via [`FullTextQuery::fields`], with per-field
//!   boosts from `searchFields` weights (`field^N`).
//! - Results carry Tantivy BM25 relevance scores for ranking.
//!
//! The engine returns matching keys with scores; the service layer resolves
//! those keys back to the full stored documents, applies filters, ordering,
//! projection, and paging.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    TermQuery,
};
use tantivy::schema::Value as _;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, STRING,
};
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer,
    TokenStream, TokenizerManager,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::storage::{Document, FieldDefinition};

/// Reserved Tantivy field name used to store each document's key.
const KEY_FIELD_NAME: &str = "__aisearch_key";

/// Name of the custom analyzer used for both indexing and querying: lowercase,
/// punctuation splitting, English stopword removal, and English stemming. This
/// approximates Azure's basic English analyzer (see
/// `docs/known_differences.md`).
const EMULATOR_TOKENIZER: &str = "aisearch_en";

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

/// Builds the emulator's English analyzer: simple tokenization with
/// lowercasing, English stopword removal, and English stemming. The same
/// analyzer is used at index time (registered on every Tantivy index) and at
/// query time (see [`analyze`]), so indexed and query terms always agree.
fn emulator_analyzer() -> TextAnalyzer {
    let stopwords = StopWordFilter::new(Language::English).unwrap_or_else(|| {
        // The English stopword list is compiled in; this fallback is
        // defensive and unreachable in practice.
        StopWordFilter::remove(Vec::<String>::new())
    });
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(stopwords)
        .filter(Stemmer::new(Language::English))
        .build()
}

/// Text options for searchable fields: tokenized with the emulator analyzer,
/// with positions (for phrase queries) and frequencies (for BM25 scoring).
fn emulator_text_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(EMULATOR_TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Tokenizes search text with the emulator analyzer, so query terms match the
/// analyzed terms stored in the index (same lowercasing, punctuation
/// splitting, stopword removal, and stemming applied at index time).
/// A hand-rolled whitespace split would diverge — e.g. `hello-world` would
/// not match indexed `hello` + `world`.
#[must_use]
pub fn analyze(text: &str) -> Vec<String> {
    let mut analyzer = emulator_analyzer();
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

/// Tokenizes text with the emulator analyzer, returning structured tokens
/// with offsets and positions (for the analyze-text endpoint).
#[must_use]
pub fn analyze_with_offsets(text: &str) -> Vec<AnalyzeToken> {
    let mut analyzer = emulator_analyzer();
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

/// Tokenizes text for the analyze-text endpoint under the requested
/// analyzer name (already validated against the known-analyzer list):
/// - `keyword`: the whole input as a single token (no analysis, case
///   preserved), matching Azure's keyword analyzer.
/// - `whitespace`: split on whitespace boundaries (no lowercasing, stemming,
///   or stopword removal), matching Azure's whitespace analyzer.
/// - anything else (`None` included): the emulator English analyzer (see
///   [`analyze_with_offsets`]).
#[must_use]
pub fn analyze_with_offsets_and_analyzer(text: &str, analyzer: Option<&str>) -> Vec<AnalyzeToken> {
    match analyzer {
        Some("keyword") => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![AnalyzeToken {
                    token: text.to_owned(),
                    start_offset: 0,
                    end_offset: text.len(),
                    position: 0,
                }]
            }
        }
        Some("whitespace") => {
            let mut tokens = Vec::new();
            let mut offset = 0usize;
            for (position, word) in text.split_whitespace().enumerate() {
                let start = text[offset..].find(word).map_or(offset, |at| offset + at);
                tokens.push(AnalyzeToken {
                    token: word.to_owned(),
                    start_offset: start,
                    end_offset: start + word.len(),
                    position,
                });
                offset = start + word.len();
            }
            tokens
        }
        _ => analyze_with_offsets(text),
    }
}

/// The tokenizer manager pre-populated with the emulator analyzer, for
/// contexts that resolve tokenizers by name.
#[must_use]
pub fn emulator_tokenizer_manager() -> TokenizerManager {
    let manager = TokenizerManager::new();
    manager.register(EMULATOR_TOKENIZER, emulator_analyzer());
    manager
}

/// One clause of a parsed simple query: a single term, a quoted phrase, or a
/// fuzzy term (`term~` / `term~N`, Lucene-style trailing-tilde syntax).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Term(String),
    Phrase(String),
    FuzzyTerm { term: String, distance: u8 },
}

/// How multi-term required clauses combine: `All` (AND, every clause must
/// match) or `Any` (OR, at least one clause must match). Mirrors Azure's
/// `searchMode` (`all` / `any`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    /// Every required clause must match (AND).
    All,
    /// At least one required clause must match (OR). This is the default when
    /// `searchMode` is omitted, matching Azure.
    #[default]
    Any,
}

impl SearchMode {
    /// Parses an Azure `searchMode` value.
    ///
    /// # Errors
    ///
    /// Returns an error string for anything other than `all` / `any`
    /// (case-insensitive).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "all" => Ok(SearchMode::All),
            "any" => Ok(SearchMode::Any),
            other => Err(format!(
                "Invalid searchMode {other:?}; supported values: 'all', 'any'."
            )),
        }
    }
}

/// A parsed full-text query: required and excluded clauses, optionally
/// restricted to a set of Azure field names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FullTextQuery {
    pub required: Vec<Clause>,
    pub excluded: Vec<Clause>,
    /// Azure field names to restrict the search to; `None` means all
    /// `searchable` string fields.
    pub fields: Option<Vec<String>>,
    /// Per-field score boosts from `searchFields` weights (`field^N`);
    /// absent entries mean boost `1.0`.
    pub boosts: BTreeMap<String, f32>,
    /// How required clauses combine.
    pub mode: SearchMode,
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
/// Supports `+term` (required; the default), `-term` (excluded), `"quoted
/// phrases"`, and Lucene-style fuzzy terms (`term~` for the default edit
/// distance 2, `term~N` for an explicit distance 0-2). An empty text or `*`
/// produces a match-all query. Fuzzy terms are lowercased only (no stemming,
/// stopword removal, or punctuation splitting), matching Azure.
///
/// # Errors
///
/// Returns an error string for an unterminated phrase, a `+`/`-` modifier
/// with no term, or an invalid fuzzy distance.
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
    let raw: String = chars[start..*pos].iter().collect();
    split_fuzzy_suffix(&raw)
}

/// Splits a raw term into a plain or fuzzy clause: a trailing `~` (the
/// default edit distance 2, matching Azure) or `~N` (explicit distance 0-2)
/// marks a fuzzy term.
///
/// # Errors
///
/// Returns an error string when the fuzzy distance exceeds 2 or no term
/// precedes the `~` marker.
fn split_fuzzy_suffix(raw: &str) -> Result<Clause, String> {
    let Some(tilde) = raw.rfind('~') else {
        return Ok(Clause::Term(raw.to_owned()));
    };
    let (stem, suffix) = raw.split_at(tilde);
    // A `~` that is not a trailing marker (text follows that is not a
    // 0-2 digit suffix) is part of the term itself.
    let distance: u8 = match &suffix[1..] {
        "" | "2" => 2,
        "1" => 1,
        "0" => 0,
        other => {
            if other.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "Invalid fuzzy distance {other:?} in search term {raw:?}; \
                     supported distances are 0-2 (e.g. `term~`, `term~2`)."
                ));
            }
            return Ok(Clause::Term(raw.to_owned()));
        }
    };
    if stem.is_empty() {
        return Err(format!(
            "Search term {raw:?} has a fuzzy marker with no term."
        ));
    }
    if distance == 0 {
        return Ok(Clause::Term(stem.to_owned()));
    }
    Ok(Clause::FuzzyTerm {
        term: stem.to_owned(),
        distance,
    })
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
        // Register the emulator analyzer so the schema's tokenizer name
        // resolves at index time.
        index
            .tokenizers()
            .register(EMULATOR_TOKENIZER, emulator_analyzer());
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
                for value in resolve_doc_paths(&document.fields, field_name) {
                    // A plain collection field (e.g. `Edm.Collection(Edm.String)`)
                    // resolves to a single JSON array; index each string element.
                    if let Value::Array(items) = value {
                        for item in items {
                            if let Some(text) = item.as_str() {
                                tantivy_doc.add_text(*field, text);
                            }
                        }
                    } else if let Some(text) = value.as_str() {
                        tantivy_doc.add_text(*field, text);
                    }
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
                .map(|(name, field)| (name.clone(), *field))
                .collect::<Vec<(String, Field)>>(),
            None => engine
                .searchable
                .iter()
                .map(|(name, field)| (name.clone(), *field))
                .collect::<Vec<(String, Field)>>(),
        };
        // A non-match-all query cannot match when there are no searchable
        // fields in scope.
        if !query.is_match_all() && fields.is_empty() {
            return Ok(BTreeMap::new());
        }
        let tantivy_query = build_query(query, &fields);
        let searcher = engine.reader.searcher();
        let top_docs = searcher
            .search(
                &*tantivy_query,
                &TopDocs::with_limit(MAX_MATCHES).order_by_score(),
            )
            .map_err(|e| QueryError::Engine(e.to_string()))?;
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
        if field.field_type == "Edm.ComplexType"
            || field.field_type == "Edm.Collection(Edm.ComplexType)"
        {
            // Complex types (single or collection) index their searchable
            // subfields under the field path; a collection contributes one
            // value per element at index time.
            collect_searchable(builder, searchable, &path, &field.subfields);
        } else if field.searchable && !field.is_vector_field() {
            // Vector fields require `searchable: true` per Azure but are not
            // full-text indexed (their values are numeric arrays).
            let tantivy_field = builder.add_text_field(&path, emulator_text_options());
            searchable.push((path, tantivy_field));
        }
    }
}

/// Resolves a field path (`Address/City`, or a plain field name) against a
/// document's field map, walking into complex-type objects. When a segment
/// resolves to a JSON array (a collection field), the remaining path is
/// resolved against every element, so a collection-of-complex path yields one
/// value per element. A plain (non-collection) path yields at most one value.
fn resolve_doc_paths<'a>(fields: &'a Map<String, Value>, path: &str) -> Vec<&'a Value> {
    let mut segments = path.split('/');
    let Some(first) = segments.next() else {
        return Vec::new();
    };
    let mut current = match fields.get(first) {
        Some(value) => vec![value],
        None => return Vec::new(),
    };
    for segment in segments {
        let mut next = Vec::new();
        for value in current {
            match value {
                Value::Array(items) => {
                    for item in items {
                        if let Some(sub) = item.as_object().and_then(|o| o.get(segment)) {
                            next.push(sub);
                        }
                    }
                }
                _ => {
                    if let Some(sub) = value.as_object().and_then(|o| o.get(segment)) {
                        next.push(sub);
                    }
                }
            }
        }
        current = next;
    }
    current
}

/// Builds the Tantivy query for a [`FullTextQuery`]: match-all when the query
/// has no clauses, otherwise a boolean combination where each required clause
/// is `Should` (`searchMode=any`, the default) or `Must` (`searchMode=all`)
/// and each excluded clause is `MustNot`. Each clause is an OR over the
/// in-scope searchable fields (a term query per analyzer token, or a phrase
/// query for quoted phrases). A clause that analyzes to no tokens (e.g. a
/// stopword-only term, or punctuation only) matches nothing.
fn build_query(query: &FullTextQuery, fields: &[(String, Field)]) -> Box<dyn Query> {
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
        clauses.push((required_occur, clause_query(clause, fields, &query.boosts)));
    }
    for clause in &query.excluded {
        clauses.push((Occur::MustNot, clause_query(clause, fields, &query.boosts)));
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

/// Builds the per-field OR query for a single clause.
fn clause_query(
    clause: &Clause,
    fields: &[(String, Field)],
    boosts: &BTreeMap<String, f32>,
) -> Box<dyn Query> {
    match clause {
        Clause::Term(term) => {
            let tokens = analyze(term);
            if tokens.is_empty() {
                return Box::new(EmptyQuery);
            }
            let mut term_clauses: Vec<(Occur, Box<dyn Query>)> =
                Vec::with_capacity(tokens.len() * fields.len());
            for token in &tokens {
                for (name, field) in fields {
                    let term = Term::from_field_text(*field, token);
                    let query: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                    term_clauses
                        .push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
                }
            }
            Box::new(BooleanQuery::new(term_clauses))
        }
        Clause::FuzzyTerm { term, distance } => {
            // Azure lowercases fuzzy terms but bypasses analysis (no stemming,
            // no stopword removal, no punctuation splitting), so the raw
            // lowercased term is the single fuzzy token.
            let token = term.to_lowercase();
            if token.is_empty() {
                return Box::new(EmptyQuery);
            }
            let mut fuzzy_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fields.len());
            for (name, field) in fields {
                let term = Term::from_field_text(*field, &token);
                let query: Box<dyn Query> = Box::new(FuzzyTermQuery::new(term, *distance, true));
                fuzzy_clauses.push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
            }
            Box::new(BooleanQuery::new(fuzzy_clauses))
        }
        Clause::Phrase(phrase) => {
            let tokens = analyze(phrase);
            if tokens.is_empty() {
                return Box::new(EmptyQuery);
            }
            let mut phrase_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fields.len());
            for (name, field) in fields {
                let terms = tokens
                    .iter()
                    .map(|token| Term::from_field_text(*field, token))
                    .collect();
                let query: Box<dyn Query> = Box::new(PhraseQuery::new(terms));
                phrase_clauses.push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
            }
            Box::new(BooleanQuery::new(phrase_clauses))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};
    use std::collections::BTreeSet;

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
}
