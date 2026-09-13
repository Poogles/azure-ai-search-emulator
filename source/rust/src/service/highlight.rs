//! Highlight fragment computation.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::query::{Clause, FullTextQuery};
use crate::storage::{Document, IndexDefinition};

use super::types::SearchQuery;

/// Computes a page's `@search.highlights` value: one entry per document with
/// a query-term match in a requested highlight field. Empty when no
/// `highlight` fields were requested.
pub(crate) fn page_highlights(
    query: &SearchQuery,
    full_text: &FullTextQuery,
    definition: &IndexDefinition,
    page: &[(Document, f32)],
) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    if query.highlight_fields.is_empty() {
        return BTreeMap::new();
    }
    let raw_terms = highlight_raw_terms(full_text);
    page.iter()
        .filter_map(|(doc, _)| {
            let fields = highlight_document(
                doc,
                &query.highlight_fields,
                definition,
                &raw_terms,
                &query.highlight_pre_tag,
                &query.highlight_post_tag,
            );
            (!fields.is_empty()).then(|| (doc.key.clone(), fields))
        })
        .collect()
}

/// Extracts the whitespace-separated words of a string (or collection of
/// string) field value, for suggester matching.
pub(crate) fn field_words(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => text.split_whitespace().map(str::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|text| text.split_whitespace().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Collects the raw (unanalyzed) query terms from a full-text query, for
/// highlight matching. For simple queries this is the required clauses'
/// terms, fuzzy terms, and phrase texts (excluded clauses never highlight).
/// For Lucene queries it is the candidate terms extracted from the query
/// text. Each term is analyzed with the highlight field's own analyzer at
/// highlight time (see [`highlight_document`]).
fn highlight_raw_terms(query: &FullTextQuery) -> Vec<String> {
    if let Some(text) = &query.lucene {
        return crate::query::lucene_query_terms(text);
    }
    let mut terms = Vec::new();
    for clause in &query.required {
        match clause {
            Clause::Term(term) | Clause::FuzzyTerm { term, .. } => {
                terms.push(term.clone());
            }
            Clause::Phrase(phrase) => {
                terms.push(phrase.clone());
            }
        }
    }
    terms
}

/// Upper bound on the number of highlight fragments returned per field,
/// approximating Azure's excerpt count.
const MAX_HIGHLIGHT_FRAGMENTS: usize = 3;

/// Splits `text` into sentences: a sentence ends at a sentence terminator
/// (`.`, `!`, `?`, or a newline) followed by whitespace or the end of the
/// text. Text without terminators is a single sentence. The terminator stays
/// with its sentence.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let is_terminator = matches!(c, '.' | '!' | '?' | '\n' | '\r');
        let next_is_boundary = chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if is_terminator && next_is_boundary {
            let end = offset + c.len_utf8();
            sentences.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        sentences.push(&text[start..]);
    }
    sentences
}

/// Wraps the words of `text` whose analyzed form (under `analyzer`) is in
/// `terms` with the highlight tags, preserving the original text (including
/// spacing and casing). Matching is analyzer-aware, so inflected forms
/// highlight. Returns the wrapped text and whether any word matched.
fn wrap_matched_words(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Option<&str>,
    pre_tag: &str,
    post_tag: &str,
) -> (String, bool) {
    let mut out = String::new();
    let mut matched = false;
    // `split_inclusive` keeps each word glued to its trailing whitespace so
    // the original spacing is preserved.
    for segment in text.split_inclusive(|c: char| c.is_whitespace()) {
        let split = segment
            .find(|c: char| c.is_whitespace())
            .unwrap_or(segment.len());
        let (word, rest) = segment.split_at(split);
        if !word.is_empty()
            && crate::query::analyze_with(word, analyzer)
                .iter()
                .any(|token| terms.contains(token))
        {
            out.push_str(pre_tag);
            out.push_str(word);
            out.push_str(post_tag);
            matched = true;
        } else {
            out.push_str(word);
        }
        out.push_str(rest);
    }
    (out, matched)
}

/// Computes the highlighted sentence-window fragments of `text` for the
/// query terms analyzed with `analyzer`: each sentence containing a match is
/// one fragment (matched words wrapped in tags), in order of appearance, up
/// to [`MAX_HIGHLIGHT_FRAGMENTS`]. Returns `None` when no sentence matches.
fn highlight_fragments(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Option<&str>,
    pre_tag: &str,
    post_tag: &str,
) -> Option<Vec<String>> {
    if terms.is_empty() {
        return None;
    }
    let mut fragments = Vec::new();
    for sentence in split_sentences(text) {
        let (highlighted, matched) =
            wrap_matched_words(sentence, terms, analyzer, pre_tag, post_tag);
        if matched {
            fragments.push(highlighted);
            if fragments.len() >= MAX_HIGHLIGHT_FRAGMENTS {
                break;
            }
        }
    }
    (!fragments.is_empty()).then_some(fragments)
}

/// Computes a document's `@search.highlights` value: one entry per highlight
/// field that contains a query term, each with its highlighted sentence-window
/// fragments. Query terms are analyzed with the field's own analyzer, so a
/// `keyword`-analyzed field highlights its verbatim value and an
/// English-analyzed field highlights stemmed forms.
fn highlight_document(
    doc: &Document,
    fields: &[String],
    definition: &IndexDefinition,
    raw_terms: &[String],
    pre_tag: &str,
    post_tag: &str,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for field in fields {
        let analyzer = definition
            .field_path(field)
            .and_then(|f| f.analyzer.as_deref());
        let terms: BTreeSet<String> = raw_terms
            .iter()
            .flat_map(|term| crate::query::analyze_with(term, analyzer))
            .collect();
        if terms.is_empty() {
            continue;
        }
        // A highlight path may resolve to several values (a collection field
        // or a path through a collection-of-complex field); every string
        // value with a query-term match contributes fragments.
        let mut fragments = Vec::new();
        for value in doc.resolve_path(field) {
            let texts: Vec<&str> = match value {
                Value::String(text) => vec![text.as_str()],
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for text in texts {
                if let Some(sentence_fragments) =
                    highlight_fragments(text, &terms, analyzer, pre_tag, post_tag)
                {
                    fragments.extend(sentence_fragments);
                }
            }
            if fragments.len() >= MAX_HIGHLIGHT_FRAGMENTS {
                fragments.truncate(MAX_HIGHLIGHT_FRAGMENTS);
                break;
            }
        }
        if !fragments.is_empty() {
            out.insert(field.clone(), fragments);
        }
    }
    out
}
