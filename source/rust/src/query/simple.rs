//! Simple-query parsing: `Clause`, `SearchMode`, `QueryType`, `FullTextQuery`.

use std::collections::BTreeMap;

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
/// simple-query parser), `full` (Lucene syntax, parsed by Tantivy's query
/// parser), or `semantic` (semantic search with extractive answers/captions).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueryType {
    /// Simple-query semantics (the default, matching Azure).
    #[default]
    Simple,
    /// Lucene query syntax (`AND`/`OR`/`NOT`, `field:term`, `term~N`,
    /// `term*`, ranges, `^N` boosts).
    Full,
    /// Semantic search (extractive answers, captions, reranker).
    Semantic,
}

impl QueryType {
    /// Parses an Azure `queryType` value.
    ///
    /// # Errors
    ///
    /// Returns an error string for anything other than `simple` / `full` /
    /// `semantic` (case-insensitive).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "simple" => Ok(QueryType::Simple),
            "full" => Ok(QueryType::Full),
            "semantic" => Ok(QueryType::Semantic),
            other => Err(format!(
                "Invalid queryType {other:?}; supported values: 'simple', 'full', 'semantic'."
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
