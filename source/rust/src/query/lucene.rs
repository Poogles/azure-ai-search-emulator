//! Lucene (`queryType=full`) query rewriting and term extraction.

use std::collections::BTreeMap;

use tantivy::query::{AllQuery, Query, QueryParser};
use tantivy::Index;

use super::SearchableField;
use super::{QueryError, SearchMode};

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
pub(crate) fn build_lucene_query(
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
