//! Tantivy analyzer registration and text analysis helpers.

use tantivy::schema::{IndexRecordOption, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{
    Language, LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter, SimpleTokenizer, Stemmer,
    StopWordFilter, TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager,
    WhitespaceTokenizer,
};

/// Registered analyzer (tokenizer) names. Every supported analyzer is
/// registered on every Tantivy index; the schema builder picks the name per
/// field from the field's declared Azure analyzer.
pub(crate) const ANALYZER_ENGLISH: &str = "aisearch_en";
pub(crate) const ANALYZER_KEYWORD: &str = "aisearch_keyword";
pub(crate) const ANALYZER_WHITESPACE: &str = "aisearch_whitespace";
pub(crate) const ANALYZER_ALPHANUM: &str = "aisearch_alphanum";
pub(crate) const ANALYZER_LATIN: &str = "aisearch_latin";
pub(crate) const ANALYZER_NGRAM: &str = "aisearch_ngram";
pub(crate) const ANALYZER_EDGE_NGRAM: &str = "aisearch_edge_ngram";
pub(crate) const ANALYZER_CJK: &str = "aisearch_cjk";
pub(crate) const ANALYZER_THAI: &str = "aisearch_thai";
pub(crate) const ANALYZER_VIETNAMESE: &str = "aisearch_vietnamese";
pub(crate) const ANALYZER_ARABIC: &str = "aisearch_ar";
pub(crate) const ANALYZER_DANISH: &str = "aisearch_da";
pub(crate) const ANALYZER_GERMAN: &str = "aisearch_de";
pub(crate) const ANALYZER_GREEK: &str = "aisearch_el";
pub(crate) const ANALYZER_SPANISH: &str = "aisearch_es";
pub(crate) const ANALYZER_FINNISH: &str = "aisearch_fi";
pub(crate) const ANALYZER_FRENCH: &str = "aisearch_fr";
pub(crate) const ANALYZER_HUNGARIAN: &str = "aisearch_hu";
pub(crate) const ANALYZER_ITALIAN: &str = "aisearch_it";
pub(crate) const ANALYZER_DUTCH: &str = "aisearch_nl";
pub(crate) const ANALYZER_NORWEGIAN: &str = "aisearch_no";
pub(crate) const ANALYZER_PORTUGUESE: &str = "aisearch_pt";
pub(crate) const ANALYZER_ROMANIAN: &str = "aisearch_ro";
pub(crate) const ANALYZER_RUSSIAN: &str = "aisearch_ru";
pub(crate) const ANALYZER_SWEDISH: &str = "aisearch_sv";
pub(crate) const ANALYZER_TURKISH: &str = "aisearch_tr";

/// The analyzer names the emulator recognizes: the names with explicit arms
/// in [`analyzer_tokenizer_name`] plus the standard/English aliases that fall
/// through to the default. The analyze-text endpoint uses this to reject
/// unknown names explicitly rather than silently mapping them.
pub const KNOWN_ANALYZERS: &[&str] = &[
    "standard",
    "standard.lucene",
    "standard.asciiFolding",
    "keyword",
    "whitespace",
    "alphanum",
    "latin",
    "ngram",
    "ngram.microsoft",
    "ngram.lucene",
    "edgeNgram",
    "edgeNgram.microsoft",
    "edgeNgram.lucene",
    "simple",
    "classic",
    "stop",
    "en.microsoft",
    "en.lucene",
    "chinese",
    "japanese",
    "korean",
    "thai",
    "vietnamese",
    "ar.microsoft",
    "da.microsoft",
    "de.microsoft",
    "el.microsoft",
    "es.microsoft",
    "fi.microsoft",
    "fr.microsoft",
    "hu.microsoft",
    "it.microsoft",
    "nl.microsoft",
    "no.microsoft",
    "pt.microsoft",
    "ro.microsoft",
    "ru.microsoft",
    "sv.microsoft",
    "tr.microsoft",
];

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
pub(crate) fn text_options_for(analyzer: Option<&str>) -> TextOptions {
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
