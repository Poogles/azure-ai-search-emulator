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

use serde_json::{Map, Value};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    QueryParser, TermQuery,
};
use tantivy::schema::Value as _;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, STRING,
};
use tantivy::tokenizer::{
    Language, LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter, SimpleTokenizer, Stemmer,
    StopWordFilter, TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager,
    WhitespaceTokenizer,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::storage::{Document, FieldDefinition};

/// Reserved Tantivy field name used to store each document's key.
const KEY_FIELD_NAME: &str = "__aisearch_key";

/// Registered analyzer (tokenizer) names. Every supported analyzer is
/// registered on every Tantivy index; the schema builder picks the name per
/// field from the field's declared Azure analyzer.
const ANALYZER_ENGLISH: &str = "aisearch_en";
const ANALYZER_KEYWORD: &str = "aisearch_keyword";
const ANALYZER_WHITESPACE: &str = "aisearch_whitespace";
const ANALYZER_ALPHANUM: &str = "aisearch_alphanum";
const ANALYZER_LATIN: &str = "aisearch_latin";
const ANALYZER_NGRAM: &str = "aisearch_ngram";
const ANALYZER_EDGE_NGRAM: &str = "aisearch_edge_ngram";
const ANALYZER_CJK: &str = "aisearch_cjk";
const ANALYZER_THAI: &str = "aisearch_thai";
const ANALYZER_VIETNAMESE: &str = "aisearch_vietnamese";
const ANALYZER_ARABIC: &str = "aisearch_ar";
const ANALYZER_DANISH: &str = "aisearch_da";
const ANALYZER_GERMAN: &str = "aisearch_de";
const ANALYZER_GREEK: &str = "aisearch_el";
const ANALYZER_SPANISH: &str = "aisearch_es";
const ANALYZER_FINNISH: &str = "aisearch_fi";
const ANALYZER_FRENCH: &str = "aisearch_fr";
const ANALYZER_HUNGARIAN: &str = "aisearch_hu";
const ANALYZER_ITALIAN: &str = "aisearch_it";
const ANALYZER_DUTCH: &str = "aisearch_nl";
const ANALYZER_NORWEGIAN: &str = "aisearch_no";
const ANALYZER_PORTUGUESE: &str = "aisearch_pt";
const ANALYZER_ROMANIAN: &str = "aisearch_ro";
const ANALYZER_RUSSIAN: &str = "aisearch_ru";
const ANALYZER_SWEDISH: &str = "aisearch_sv";
const ANALYZER_TURKISH: &str = "aisearch_tr";

/// A searchable field: the Azure field path, its Tantivy field, and the
/// field's declared analyzer name (`None` for the English default).
type SearchableField = (String, Field, Option<String>);

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
    /// The search text is malformed (e.g. an unparseable Lucene query).
    InvalidQuery(String),
    /// An underlying Tantivy operation failed.
    Engine(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::IndexNotFound(name) => write!(f, "index {name:?} not found"),
            QueryError::InvalidQuery(message) => write!(f, "invalid query: {message}"),
            QueryError::Engine(message) => write!(f, "search engine error: {message}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Maps an Azure analyzer name (or `None` for the index default) to the
/// registered tokenizer name used for both indexing and querying. Unknown
/// names fall back to the English analyzer (see
/// `docs/known_differences.md`).
#[must_use]
pub fn analyzer_tokenizer_name(analyzer: Option<&str>) -> &'static str {
    match analyzer {
        Some("keyword") => ANALYZER_KEYWORD,
        Some("whitespace") => ANALYZER_WHITESPACE,
        Some("alphanum") => ANALYZER_ALPHANUM,
        Some("latin") => ANALYZER_LATIN,
        Some("ngram" | "ngram.microsoft" | "ngram.lucene") => ANALYZER_NGRAM,
        Some("edgeNgram" | "edgeNgram.microsoft" | "edgeNgram.lucene") => ANALYZER_EDGE_NGRAM,
        Some("chinese" | "japanese" | "korean") => ANALYZER_CJK,
        Some("thai") => ANALYZER_THAI,
        Some("vietnamese") => ANALYZER_VIETNAMESE,
        Some("ar.microsoft") => ANALYZER_ARABIC,
        Some("da.microsoft") => ANALYZER_DANISH,
        Some("de.microsoft") => ANALYZER_GERMAN,
        Some("el.microsoft") => ANALYZER_GREEK,
        Some("es.microsoft") => ANALYZER_SPANISH,
        Some("fi.microsoft") => ANALYZER_FINNISH,
        Some("fr.microsoft") => ANALYZER_FRENCH,
        Some("hu.microsoft") => ANALYZER_HUNGARIAN,
        Some("it.microsoft") => ANALYZER_ITALIAN,
        Some("nl.microsoft") => ANALYZER_DUTCH,
        Some("no.microsoft") => ANALYZER_NORWEGIAN,
        Some("pt.microsoft") => ANALYZER_PORTUGUESE,
        Some("ro.microsoft") => ANALYZER_ROMANIAN,
        Some("ru.microsoft") => ANALYZER_RUSSIAN,
        Some("sv.microsoft") => ANALYZER_SWEDISH,
        Some("tr.microsoft") => ANALYZER_TURKISH,
        // The default, the standard aliases, and anything unrecognized.
        None | Some(_) => ANALYZER_ENGLISH,
    }
}

/// Builds a language analyzer: simple tokenization with lowercasing,
/// stopword removal, and stemming for `language`. The English variant is the
/// index default (see [`analyzer_tokenizer_name`]).
fn language_analyzer(language: Language) -> TextAnalyzer {
    let stopwords = StopWordFilter::new(language).unwrap_or_else(|| {
        // The stopword lists are compiled in; this fallback is defensive and
        // unreachable in practice.
        StopWordFilter::remove(Vec::<String>::new())
    });
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(stopwords)
        .filter(Stemmer::new(language))
        .build()
}

/// Builds the analyzer for a registered tokenizer name.
fn build_analyzer(name: &str) -> TextAnalyzer {
    match name {
        ANALYZER_KEYWORD => TextAnalyzer::builder(RawTokenizer::default()).build(),
        ANALYZER_WHITESPACE => TextAnalyzer::builder(WhitespaceTokenizer::default()).build(),
        ANALYZER_ALPHANUM => TextAnalyzer::builder(SimpleTokenizer::default()).build(),
        ANALYZER_LATIN | ANALYZER_THAI | ANALYZER_VIETNAMESE => {
            TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .build()
        }
        ANALYZER_NGRAM => {
            let tokenizer = NgramTokenizer::new(1, 15, false)
                .unwrap_or_else(|e| panic!("ngram tokenizer is always valid: {e}"));
            TextAnalyzer::builder(tokenizer)
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .build()
        }
        ANALYZER_EDGE_NGRAM => {
            let tokenizer = NgramTokenizer::new(1, 15, true)
                .unwrap_or_else(|e| panic!("edge-ngram tokenizer is always valid: {e}"));
            TextAnalyzer::builder(tokenizer)
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .build()
        }
        ANALYZER_CJK => TextAnalyzer::builder(CjkTokenizer::default())
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .build(),
        ANALYZER_ARABIC => language_analyzer(Language::Arabic),
        ANALYZER_DANISH => language_analyzer(Language::Danish),
        ANALYZER_GERMAN => language_analyzer(Language::German),
        ANALYZER_GREEK => language_analyzer(Language::Greek),
        ANALYZER_SPANISH => language_analyzer(Language::Spanish),
        ANALYZER_FINNISH => language_analyzer(Language::Finnish),
        ANALYZER_FRENCH => language_analyzer(Language::French),
        ANALYZER_HUNGARIAN => language_analyzer(Language::Hungarian),
        ANALYZER_ITALIAN => language_analyzer(Language::Italian),
        ANALYZER_DUTCH => language_analyzer(Language::Dutch),
        ANALYZER_NORWEGIAN => language_analyzer(Language::Norwegian),
        ANALYZER_PORTUGUESE => language_analyzer(Language::Portuguese),
        ANALYZER_ROMANIAN => language_analyzer(Language::Romanian),
        ANALYZER_RUSSIAN => language_analyzer(Language::Russian),
        ANALYZER_SWEDISH => language_analyzer(Language::Swedish),
        ANALYZER_TURKISH => language_analyzer(Language::Turkish),
        _ => language_analyzer(Language::English),
    }
}

/// Registers every supported analyzer on a tokenizer manager, so any field's
/// declared analyzer resolves at index time.
pub fn register_analyzers(manager: &TokenizerManager) {
    for name in [
        ANALYZER_ENGLISH,
        ANALYZER_KEYWORD,
        ANALYZER_WHITESPACE,
        ANALYZER_ALPHANUM,
        ANALYZER_LATIN,
        ANALYZER_NGRAM,
        ANALYZER_EDGE_NGRAM,
        ANALYZER_CJK,
        ANALYZER_THAI,
        ANALYZER_VIETNAMESE,
        ANALYZER_ARABIC,
        ANALYZER_DANISH,
        ANALYZER_GERMAN,
        ANALYZER_GREEK,
        ANALYZER_SPANISH,
        ANALYZER_FINNISH,
        ANALYZER_FRENCH,
        ANALYZER_HUNGARIAN,
        ANALYZER_ITALIAN,
        ANALYZER_DUTCH,
        ANALYZER_NORWEGIAN,
        ANALYZER_PORTUGUESE,
        ANALYZER_ROMANIAN,
        ANALYZER_RUSSIAN,
        ANALYZER_SWEDISH,
        ANALYZER_TURKISH,
    ] {
        manager.register(name, build_analyzer(name));
    }
}

/// A tokenizer approximating Azure's CJK analyzers (chinese, japanese,
/// korean): runs of CJK characters are emitted as overlapping bigrams (a
/// single character for a one-character run), and runs of other characters
/// are emitted verbatim as single tokens. Bigram matching is the standard
/// approximation for unsegmented CJK text (cf. Lucene's `CJKBigramFilter`).
#[derive(Clone, Default)]
struct CjkTokenizer {
    token: Token,
}

struct CjkTokenStream<'a> {
    text: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    pending: std::collections::VecDeque<(usize, usize, String)>,
    token: &'a mut Token,
}

impl Tokenizer for CjkTokenizer {
    type TokenStream<'a> = CjkTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> CjkTokenStream<'a> {
        self.token.reset();
        CjkTokenStream {
            text,
            chars: text.char_indices().peekable(),
            pending: std::collections::VecDeque::new(),
            token: &mut self.token,
        }
    }
}

impl TokenStream for CjkTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        if let Some((from, to, text)) = self.pending.pop_front() {
            self.token.offset_from = from;
            self.token.offset_to = to;
            self.token.position = self.token.position.wrapping_add(1);
            self.token.text = text;
            return true;
        }
        while let Some(&(_, c)) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
        let Some(&(start, first)) = self.chars.peek() else {
            return false;
        };
        if is_cjk_char(first) {
            let mut run: Vec<char> = Vec::new();
            let mut end = start;
            while let Some(&(offset, c)) = self.chars.peek() {
                if is_cjk_char(c) {
                    run.push(c);
                    end = offset + c.len_utf8();
                    self.chars.next();
                } else {
                    break;
                }
            }
            if run.len() == 1 {
                self.token.offset_from = start;
                self.token.offset_to = end;
                self.token.position = self.token.position.wrapping_add(1);
                self.token.text.push(run[0]);
                return true;
            }
            let mut offset = start;
            for window in run.windows(2) {
                let gram: String = window.iter().collect();
                let gram_len = gram.len();
                self.pending.push_back((offset, offset + gram_len, gram));
                offset += gram_len;
            }
            return self.advance();
        }
        let mut end = start + first.len_utf8();
        while let Some(&(offset, c)) = self.chars.peek() {
            if c.is_whitespace() || is_cjk_char(c) {
                break;
            }
            end = offset + c.len_utf8();
            self.chars.next();
        }
        self.token.offset_from = start;
        self.token.offset_to = end;
        self.token.position = self.token.position.wrapping_add(1);
        self.token.text.push_str(&self.text[start..end]);
        true
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

/// Whether a character belongs to a CJK script (Han, Kana, or Hangul), for
/// the CJK bigram tokenizer.
fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
            | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
            | '\u{3040}'..='\u{30FF}' // Hiragana + Katakana
            | '\u{31F0}'..='\u{31FF}' // Katakana Phonetic Extensions
            | '\u{FF66}'..='\u{FF9D}' // Halfwidth Katakana
            | '\u{1100}'..='\u{11FF}' // Hangul Jamo
            | '\u{3130}'..='\u{318F}' // Hangul Compatibility Jamo
            | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

/// Text options for a searchable field: tokenized with the field's analyzer,
/// with positions (for phrase queries) and frequencies (for BM25 scoring).
fn text_options_for(analyzer: Option<&str>) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(analyzer_tokenizer_name(analyzer))
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Tokenizes search text with the analyzer for `analyzer` (the English
/// analyzer when `None`), so query terms match the analyzed terms stored in
/// the index (same lowercasing, punctuation splitting, stopword removal, and
/// stemming applied at index time). A hand-rolled whitespace split would
/// diverge — e.g. `hello-world` would not match indexed `hello` + `world`.
#[must_use]
pub fn analyze_with(text: &str, analyzer: Option<&str>) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut analyzer = build_analyzer(analyzer_tokenizer_name(analyzer));
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    tokens
}

/// Tokenizes search text with the default (English) analyzer.
#[must_use]
pub fn analyze(text: &str) -> Vec<String> {
    analyze_with(text, None)
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

/// Tokenizes text with the default (English) analyzer, returning structured
/// tokens with offsets and positions (for the analyze-text endpoint).
#[must_use]
pub fn analyze_with_offsets(text: &str) -> Vec<AnalyzeToken> {
    analyze_with_offsets_and_analyzer(text, None)
}

/// Tokenizes text for the analyze-text endpoint under the requested analyzer
/// name (already validated against the known-analyzer list): `keyword`
/// (whole input as a single verbatim token), `whitespace` (whitespace split,
/// no lowercasing), `alphanum` (punctuation split, no lowercasing), `latin`
/// (lowercasing, no stemming or stopword removal), `ngram` / `edgeNgram`
/// (character n-grams), CJK (bigrams), the `*.microsoft` language analyzers,
/// and the English analyzer for `None` and the standard aliases.
#[must_use]
pub fn analyze_with_offsets_and_analyzer(text: &str, analyzer: Option<&str>) -> Vec<AnalyzeToken> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut analyzer = build_analyzer(analyzer_tokenizer_name(analyzer));
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

/// The tokenizer manager pre-populated with every supported analyzer, for
/// contexts that resolve tokenizers by name.
#[must_use]
pub fn emulator_tokenizer_manager() -> TokenizerManager {
    let manager = TokenizerManager::new();
    register_analyzers(&manager);
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

/// The `queryType` of a search: `simple` (the default; the emulator's
/// simple-query parser) or `full` (Lucene syntax, parsed by Tantivy's query
/// parser).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueryType {
    /// Simple-query semantics (the default, matching Azure).
    #[default]
    Simple,
    /// Lucene query syntax (`AND`/`OR`/`NOT`, `field:term`, `term~N`,
    /// `term*`, ranges, `^N` boosts).
    Full,
}

impl QueryType {
    /// Parses an Azure `queryType` value.
    ///
    /// # Errors
    ///
    /// Returns an error string for anything other than `simple` / `full`
    /// (case-insensitive).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "simple" => Ok(QueryType::Simple),
            "full" => Ok(QueryType::Full),
            other => Err(format!(
                "Invalid queryType {other:?}; supported values: 'simple', 'full'."
            )),
        }
    }
}

/// A parsed full-text query: required and excluded clauses (for
/// `queryType=simple`), or raw Lucene text (for `queryType=full`), optionally
/// restricted to a set of Azure field names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FullTextQuery {
    pub required: Vec<Clause>,
    pub excluded: Vec<Clause>,
    /// Raw Lucene query text for `queryType=full`; when `Some`, the clauses
    /// are ignored and the text is parsed by the engine's query parser.
    pub lucene: Option<String>,
    /// Synonym expansions: `(analyzed query term, analyzed expansion terms)`
    /// pairs. Each pair OR-matches alongside its term (see [`clause_query`]).
    /// Expansion terms are analyzed with the querying field's analyzer, so
    /// for multi-analyzer scopes the service supplies per-field pairs.
    pub synonyms: Vec<(String, Vec<String>)>,
    /// Azure field names to restrict the search to; `None` means all
    /// `searchable` fields.
    pub fields: Option<Vec<String>>,
    /// Per-field score boosts from `searchFields` weights (`field^N`);
    /// absent entries mean boost `1.0`.
    pub boosts: BTreeMap<String, f32>,
    /// How required clauses combine (simple queries); for Lucene queries it
    /// selects the default operator between space-separated terms.
    pub mode: SearchMode,
}

impl FullTextQuery {
    /// Returns `true` when the query matches every document.
    #[must_use]
    pub fn is_match_all(&self) -> bool {
        self.lucene.is_none() && self.required.is_empty() && self.excluded.is_empty()
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
    /// The underlying index, kept (cheaply cloneable) so Lucene queries can
    /// be parsed with a [`QueryParser`] bound to the schema and tokenizers.
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    key_field: Field,
    /// Searchable fields as `(azure field name, tantivy field, analyzer)`
    /// triples; `analyzer` is the field's declared Azure analyzer name
    /// (`None` for the English default).
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
            for (field_name, field, _analyzer) in &engine.searchable {
                for value in resolve_doc_paths(&document.fields, field_name) {
                    // A plain collection field (e.g. `Edm.Collection(Edm.String)`)
                    // resolves to a single JSON array; index each element.
                    if let Value::Array(items) = value {
                        for item in items {
                            if let Some(text) = text_representation(item) {
                                tantivy_doc.add_text(*field, text);
                            }
                        }
                    } else if let Some(text) = text_representation(value) {
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
        let fields: Vec<SearchableField> = match &query.fields {
            Some(names) => engine
                .searchable
                .iter()
                .filter(|(name, _, _)| names.contains(name))
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
            let tantivy_field =
                builder.add_text_field(&path, text_options_for(field.analyzer.as_deref()));
            searchable.push((path, tantivy_field, field.analyzer.clone()));
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

/// Builds the Tantivy query for a `queryType=full` (Lucene) search text:
/// parsed by Tantivy's query parser over the in-scope searchable fields, with
/// the request's field boosts applied and the default operator between
/// space-separated terms set by `searchMode` (`any` → OR, the default;
/// `all` → AND). An empty text or `*` matches all documents.
///
/// # Errors
///
/// Returns [`QueryError::InvalidQuery`] when the text is not valid Lucene
/// syntax (unknown field, malformed range, unbalanced quotes, ...).
fn build_lucene_query(
    index: &Index,
    text: &str,
    fields: &[SearchableField],
    boosts: &BTreeMap<String, f32>,
    mode: SearchMode,
) -> Result<Box<dyn Query>, QueryError> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Ok(Box::new(AllQuery));
    }
    // Tantivy's parser auto-closes an unterminated quote; Azure rejects it.
    if !quotes_balanced(trimmed) {
        return Err(QueryError::InvalidQuery(
            "Unterminated quoted phrase in search text.".to_owned(),
        ));
    }
    // Expand `prefix*` wildcards (Azure prefix semantics) before parsing:
    // Tantivy's parser strips a trailing `*` instead of wildcard-matching.
    let field_names: Vec<String> = fields.iter().map(|(name, _, _)| name.clone()).collect();
    let expanded = expand_trailing_wildcards(trimmed, &field_names);
    let mut parser =
        QueryParser::for_index(index, fields.iter().map(|(_, field, _)| *field).collect());
    if mode == SearchMode::All {
        parser.set_conjunction_by_default();
    }
    // Azure's full (Lucene) query type supports `term*` wildcards and regexes.
    parser.allow_regexes();
    for (name, field, _) in fields {
        if let Some(boost) = boosts.get(name) {
            parser.set_field_boost(*field, *boost);
        }
    }
    parser
        .parse_query(&expanded)
        .map_err(|e| QueryError::InvalidQuery(e.to_string()))
}

/// Expands trailing-wildcard terms (`prefix*`) into explicit case-insensitive
/// regex queries. Tantivy's parser strips a trailing `*` and treats the
/// remainder as a plain term, while Azure's full (Lucene) query type treats
/// `prefix*` as a prefix wildcard. A field-scoped `field:prefix*` becomes
/// `field:/(?i)prefix.*/`;
/// a bare `prefix*` becomes a disjunction over all in-scope fields.
/// Quoted phrases are left untouched (a `*` inside quotes is literal), as is
/// a standalone `*` (match-all, handled upstream). Tokens with `*` elsewhere
/// (leading/multiple wildcards, `/.../ ` regex literals) are left for the
/// parser.
fn expand_trailing_wildcards(text: &str, field_names: &[String]) -> String {
    text.split('"')
        .enumerate()
        .map(|(i, segment)| {
            if i % 2 == 0 {
                expand_unquoted_wildcards(segment, field_names)
            } else {
                format!("\"{segment}\"")
            }
        })
        .collect::<String>()
}

/// Expands trailing-wildcard tokens in an unquoted query segment, preserving
/// leading and trailing whitespace.
fn expand_unquoted_wildcards(segment: &str, field_names: &[String]) -> String {
    let leading_len: usize = segment
        .chars()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .sum();
    let trailing_len: usize = segment
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .sum();
    if leading_len + trailing_len >= segment.len() {
        return segment.to_owned();
    }
    let core = &segment[leading_len..segment.len() - trailing_len];
    let expanded = core
        .split_whitespace()
        .map(|token| expand_token_wildcard(token, field_names))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{}{}",
        &segment[..leading_len],
        expanded,
        &segment[segment.len() - trailing_len..]
    )
}

/// Splits a token into leading modifiers/parens, the core, and trailing
/// parens.
fn split_token_affixes(token: &str) -> (&str, &str, &str) {
    let lead_len: usize = token
        .chars()
        .take_while(|c| matches!(c, '+' | '-' | '('))
        .map(char::len_utf8)
        .sum();
    let trail_len: usize = token
        .chars()
        .rev()
        .take_while(|c| matches!(c, ')'))
        .map(char::len_utf8)
        .sum();
    if lead_len + trail_len >= token.len() {
        return (token, "", "");
    }
    let (lead, rest) = token.split_at(lead_len);
    let (core, trail) = rest.split_at(rest.len() - trail_len);
    (lead, core, trail)
}

/// Expands a single unquoted token's trailing wildcard, if present. Returns
/// the token unchanged when it has no trailing `*`, is a bare `*`, an
/// operator, or regex syntax.
fn expand_token_wildcard(token: &str, field_names: &[String]) -> String {
    let (lead, core, trail) = split_token_affixes(token);
    if core.is_empty() {
        return token.to_owned();
    }
    let Some(prefix) = core.strip_suffix('*') else {
        return token.to_owned();
    };
    if prefix.is_empty() || prefix.contains('*') || prefix.starts_with('/') {
        return token.to_owned();
    }
    if matches!(
        prefix,
        "AND" | "OR" | "NOT" | "and" | "or" | "not" | "TO" | "to"
    ) {
        return token.to_owned();
    }
    let (field, term) = match prefix.split_once(':') {
        Some((f, t)) if !f.is_empty() && !t.is_empty() => (Some(f), t),
        _ => (None, prefix),
    };
    if term.contains([':', '/', '"']) {
        return token.to_owned();
    }
    let regex = format!("/(?i){}/", regex_escaped_prefix(term));
    if let Some(f) = field {
        format!("{lead}{f}:{regex}{trail}")
    } else {
        if field_names.is_empty() {
            return token.to_owned();
        }
        let disjunction = field_names
            .iter()
            .map(|name| format!("{name}:{regex}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        format!("{lead}({disjunction}){trail}")
    }
}

/// Whether the Lucene query text has balanced quotes (`""` counts as an
/// escaped literal quote, not two delimiters).
fn quotes_balanced(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < chars.len() {
        if chars[i] == '"' {
            if chars.get(i + 1) == Some(&'"') {
                i += 2;
            } else {
                count += 1;
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    count.is_multiple_of(2)
}

/// Escapes a wildcard prefix for embedding in a `/.../ ` regex, appending
/// `.*` so the prefix matches literally (case-insensitively via the caller's
/// `(?i)` flag).
fn regex_escaped_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 4);
    for c in prefix.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out.push_str(".*");
    out
}

/// Extracts the candidate search terms from a Lucene query text, for
/// highlight matching: whitespace- and quote-separated tokens with field
/// prefixes (`field:`), boolean operators, boosts (`^N`), fuzzy suffixes
/// (`~N`), wildcards, and range brackets stripped. The terms are analyzed
/// per field at highlight time, so this is an approximation (e.g. range
/// bounds and wildcard stems are included).
#[must_use]
pub fn lucene_query_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || c == '"') {
        let mut term = raw.trim();
        // Strip a `field:` prefix (the field name may contain `/` for
        // complex-type paths).
        if let Some((field, rest)) = term.split_once(':') {
            if !field.is_empty() && !field.contains(|c: char| c.is_whitespace()) {
                term = rest;
            }
        }
        // Strip leading boolean modifiers.
        term = term.trim_start_matches(['+', '-']);
        // Strip a trailing boost (`^N`), fuzzy suffix (`~N`), or wildcard.
        if let Some(at) = term.find(['^', '~', '*']) {
            term = &term[..at];
        }
        // Strip range brackets (the `TO` bounds separator is filtered as an
        // operator below).
        term = term.trim_matches(['[', ']', '{', '}']);
        if term.is_empty()
            || matches!(term, "AND" | "OR" | "NOT" | "and" | "or" | "not")
            || term == "*"
        {
            continue;
        }
        terms.push(term.to_owned());
    }
    terms
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
fn clause_query(
    clause: &Clause,
    fields: &[SearchableField],
    boosts: &BTreeMap<String, f32>,
    synonyms: &[(String, Vec<String>)],
) -> Box<dyn Query> {
    match clause {
        Clause::Term(term) => {
            let mut term_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (name, field, analyzer) in fields {
                let tokens = analyze_with(term, analyzer.as_deref());
                for token in &tokens {
                    let term = Term::from_field_text(*field, token);
                    let query: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                    term_clauses
                        .push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
                }
                // Synonym expansions for this term, analyzed with the same
                // field analyzer so `keyword` fields match verbatim forms and
                // English fields match stemmed forms.
                if let Some((_, expansions)) = synonyms.iter().find(|(raw, _)| raw == term) {
                    for expansion in expansions {
                        for token in analyze_with(expansion, analyzer.as_deref()) {
                            let term = Term::from_field_text(*field, &token);
                            let query: Box<dyn Query> =
                                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                            term_clauses.push((
                                Occur::Should,
                                maybe_boost(query, field_boost(boosts, name)),
                            ));
                        }
                    }
                }
            }
            if term_clauses.is_empty() {
                return Box::new(EmptyQuery);
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
            for (name, field, _analyzer) in fields {
                let term = Term::from_field_text(*field, &token);
                let query: Box<dyn Query> = Box::new(FuzzyTermQuery::new(term, *distance, true));
                fuzzy_clauses.push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
            }
            Box::new(BooleanQuery::new(fuzzy_clauses))
        }
        Clause::Phrase(phrase) => {
            let mut phrase_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (name, field, analyzer) in fields {
                let tokens = analyze_with(phrase, analyzer.as_deref());
                if tokens.is_empty() {
                    continue;
                }
                let terms = tokens
                    .iter()
                    .map(|token| Term::from_field_text(*field, token))
                    .collect();
                let query: Box<dyn Query> = Box::new(PhraseQuery::new(terms));
                phrase_clauses.push((Occur::Should, maybe_boost(query, field_boost(boosts, name))));
            }
            if phrase_clauses.is_empty() {
                return Box::new(EmptyQuery);
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
                analyzer: None,
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
                analyzer: None,
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
                analyzer: None,
                raw: Value::Null,
            },
            FieldDefinition {
                name: "count".to_owned(),
                field_type: "Edm.Int32".to_owned(),
                is_key: false,
                searchable: false,
                filterable: true,
                sortable: true,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                analyzer: None,
                raw: Value::Null,
            },
            FieldDefinition {
                name: "price".to_owned(),
                field_type: "Edm.Double".to_owned(),
                is_key: false,
                searchable: true,
                filterable: true,
                sortable: true,
                facetable: false,
                retrievable: true,
                vector_dimensions: None,
                vector_search_profile: None,
                subfields: Vec::new(),
                analyzer: None,
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
            field_type: field_type.to_owned(),
            is_key: false,
            searchable,
            filterable: false,
            sortable: false,
            facetable: false,
            retrievable: true,
            vector_dimensions: None,
            vector_search_profile: None,
            subfields: Vec::new(),
            analyzer: analyzer.map(str::to_owned),
            raw: Value::Null,
        }
    }

    fn key_field() -> FieldDefinition {
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
            analyzer: None,
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
