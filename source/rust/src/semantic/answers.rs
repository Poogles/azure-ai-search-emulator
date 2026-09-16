//! Extractive answer selection: sentence splitting, BM25 sentence scoring,
//! and global top-N answer selection across the full filtered result set.

use std::collections::{BTreeMap, BTreeSet};

use super::{SemanticAnswer, SemanticConfiguration, SemanticQuery};
use crate::service::highlight::{matched_word_spans, wrap_spans};
use crate::storage::Document;

/// Minimum sentence length: shorter fragments are merged with the next
/// sentence.
pub const MIN_SENTENCE_LENGTH: usize = 20;

/// Maximum sentence length: longer sentences are split at the last word
/// boundary before this length.
pub const MAX_SENTENCE_LENGTH: usize = 500;

/// BM25 saturation parameter.
const BM25_K1: f32 = 1.2;
/// BM25 length-normalization parameter.
const BM25_B: f32 = 0.75;

/// Splits text into sentences. A sentence ends at `.`, `!`, `?`, `;`, or a
/// newline followed by whitespace or the end of the text (the terminator
/// stays with its sentence). Fragments shorter than [`MIN_SENTENCE_LENGTH`]
/// are merged with the following fragment; fragments longer than
/// [`MAX_SENTENCE_LENGTH`] are split at the last word boundary before the
/// limit. Abbreviations are not special-cased.
#[must_use]
pub fn split_sentences(text: &str) -> Vec<String> {
    // Raw split at delimiter boundaries.
    let mut fragments: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let is_terminator = matches!(c, '.' | '!' | '?' | ';' | '\n' | '\r');
        let next_is_boundary = chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        if is_terminator && next_is_boundary {
            let end = offset + c.len_utf8();
            fragments.push(text[start..end].to_owned());
            start = end;
        }
    }
    if start < text.len() {
        fragments.push(text[start..].to_owned());
    }
    // Merge short fragments with the following fragment.
    let mut merged: Vec<String> = Vec::new();
    let mut pending = String::new();
    for fragment in fragments {
        let fragment = fragment.trim();
        if fragment.is_empty() {
            continue;
        }
        if pending.is_empty() {
            pending.push_str(fragment);
        } else {
            pending.push(' ');
            pending.push_str(fragment);
        }
        if pending.trim().chars().count() >= MIN_SENTENCE_LENGTH {
            merged.push(std::mem::take(&mut pending));
        }
    }
    if !pending.trim().is_empty() {
        merged.push(pending);
    }
    // Split over-long fragments at word boundaries.
    let mut sentences = Vec::new();
    for fragment in merged {
        sentences.extend(split_long_sentence(&fragment));
    }
    sentences
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Splits a single fragment longer than [`MAX_SENTENCE_LENGTH`] at the last
/// word boundary before the limit (repeatedly, until every piece fits). A
/// fragment without a word boundary is hard-split at the limit.
fn split_long_sentence(fragment: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = fragment.trim();
    while rest.chars().count() > MAX_SENTENCE_LENGTH {
        let word_boundary = rest
            .char_indices()
            .take_while(|(i, _)| *i <= MAX_SENTENCE_LENGTH)
            .filter(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .last();
        if let Some(byte_idx) = word_boundary {
            pieces.push(rest[..byte_idx].to_owned());
            rest = rest[byte_idx..].trim_start();
        } else {
            // No word boundary: hard-split at the character limit.
            let byte_idx = rest
                .char_indices()
                .nth(MAX_SENTENCE_LENGTH)
                .map_or(rest.len(), |(i, _)| i);
            pieces.push(rest[..byte_idx].to_owned());
            rest = rest[byte_idx..].trim_start();
        }
    }
    if !rest.is_empty() {
        pieces.push(rest.to_owned());
    }
    pieces
}

/// Builds the analyzed query terms for answer/caption scoring: the search
/// text analyzed with the full-text (English) analyzer, with the
/// `queryContext` question text appended (biasing selection toward sentences
/// that address the questions).
#[must_use]
pub fn query_terms(search_text: &str, questions: &[String]) -> Vec<String> {
    let mut terms = crate::query::analyze(search_text);
    for question in questions {
        terms.extend(crate::query::analyze(question));
    }
    terms
}

/// Scores sentences against analyzed query terms with a local BM25: each
/// sentence is a document, analyzed with the full-text (English) analyzer.
/// Returns one score per sentence. A sentence scores above zero exactly when
/// at least one query term matches it.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bm25_score_sentences(sentences: &[String], terms: &[String]) -> Vec<f32> {
    if sentences.is_empty() || terms.is_empty() {
        return vec![0.0; sentences.len()];
    }
    let analyzed: Vec<Vec<String>> = sentences.iter().map(|s| crate::query::analyze(s)).collect();
    let total = sentences.len() as f32;
    let avg_len: f32 = (analyzed
        .iter()
        .map(|tokens| tokens.len() as f32)
        .sum::<f32>()
        / total)
        .max(1.0);
    // Document frequency per query term (sentences containing the term).
    let doc_freqs: BTreeMap<&str, usize> = terms
        .iter()
        .map(|term| {
            let df = analyzed
                .iter()
                .filter(|tokens| tokens.iter().any(|t| t == term))
                .count();
            (term.as_str(), df)
        })
        .collect();
    analyzed
        .iter()
        .map(|tokens| {
            let doc_len = tokens.len().max(1) as f32;
            let mut score = 0.0f32;
            for term in terms {
                let tf = tokens.iter().filter(|t| *t == term).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                let df = *doc_freqs.get(term.as_str()).unwrap_or(&0) as f32;
                let idf = ((total - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf_norm = (tf * (BM25_K1 + 1.0))
                    / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / avg_len));
                score += idf * tf_norm;
            }
            score
        })
        .collect()
}

/// Wraps the query-term matches of `text` in the highlight tags
/// (analyzer-aware matching, same as `@search.highlights`). With no matches
/// the text is returned unchanged.
#[must_use]
pub fn highlight_text(
    text: &str,
    terms: &BTreeSet<String>,
    pre_tag: &str,
    post_tag: &str,
) -> String {
    let spans = matched_word_spans(text, terms, None);
    wrap_spans(text, &spans, pre_tag, post_tag)
}

/// Extracts the text of a document's field values as a single string (for
/// sentence splitting): every string value the path resolves to, joined.
#[must_use]
pub fn field_text(document: &Document, field: &str) -> String {
    document
        .resolve_path(field)
        .into_iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Selects the global top-N answers for a semantic search: candidate
/// sentences are gathered from the configuration's answer fields across the
/// full scored result set (before paging), scored with BM25, and the top-N
/// (N = `answers.count`) with a positive score are returned in score order
/// with scores normalized to 0.0–1.0 (`score / max_score`).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn select_answers(
    scored: &[(Document, f32)],
    terms: &[String],
    config: &SemanticConfiguration,
    semantic_query: &SemanticQuery,
    pre_tag: &str,
    post_tag: &str,
) -> Vec<SemanticAnswer> {
    let Some(requested) = &semantic_query.answers else {
        return Vec::new();
    };
    if terms.is_empty() || config.answers_fields.is_empty() {
        return Vec::new();
    }
    // Gather (doc_key, field_order, sentence_idx, sentence) candidates.
    let mut gathered: Vec<(String, usize, usize, String)> = Vec::new();
    for (doc, _) in scored {
        for (order, field) in config.answers_fields.iter().enumerate() {
            let text = field_text(doc, field);
            if text.trim().is_empty() {
                continue;
            }
            for (idx, sentence) in split_sentences(&text).into_iter().enumerate() {
                gathered.push((doc.key.clone(), order, idx, sentence));
            }
        }
    }
    if gathered.is_empty() {
        return Vec::new();
    }
    let sentences: Vec<String> = gathered.iter().map(|(_, _, _, s)| s.clone()).collect();
    let sentence_scores = bm25_score_sentences(&sentences, terms);
    let mut candidates: Vec<(String, usize, usize, String, f32)> = gathered
        .into_iter()
        .zip(sentence_scores)
        .filter(|(_, score)| *score > 0.0)
        .map(|((key, order, idx, sentence), score)| (key, order, idx, sentence, score))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    candidates.sort_by(|a, b| {
        b.4.partial_cmp(&a.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    candidates.truncate(requested.count);
    let max_score = candidates
        .iter()
        .map(|(_, _, _, _, score)| *score)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(1.0)
        .max(f32::EPSILON);
    let term_set: BTreeSet<String> = terms.iter().cloned().collect();
    candidates
        .into_iter()
        .map(|(key, _, _, text, score)| SemanticAnswer {
            key,
            score: score / max_score,
            highlights: highlight_text(&text, &term_set, pre_tag, post_tag),
            text,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn split_sentences_basic() {
        let sentences = split_sentences(
            "Hello world, this is a test. How are you doing today? I am fine, thanks!",
        );
        assert_eq!(
            sentences,
            vec![
                "Hello world, this is a test.",
                "How are you doing today?",
                "I am fine, thanks!"
            ]
        );
    }

    #[test]
    fn split_sentences_semicolon_and_newline() {
        let sentences = split_sentences(
            "First part of the text here; second part of the text here.\nThird part of the text here now.",
        );
        assert_eq!(
            sentences,
            vec![
                "First part of the text here;",
                "second part of the text here.",
                "Third part of the text here now."
            ]
        );
    }

    #[test]
    fn split_sentences_short_fragments_merge() {
        // "Hi." is shorter than 20 chars, so it merges with the next sentence.
        let sentences = split_sentences("Hi. This is a much longer sentence following it.");
        assert_eq!(
            sentences,
            vec!["Hi. This is a much longer sentence following it."]
        );
    }

    #[test]
    fn split_sentences_long_sentence_splits_at_word_boundary() {
        let long = format!("{} end.", "word ".repeat(150));
        assert!(long.chars().count() > MAX_SENTENCE_LENGTH);
        let sentences = split_sentences(&long);
        assert!(sentences.len() > 1);
        for sentence in &sentences {
            assert!(sentence.chars().count() <= MAX_SENTENCE_LENGTH);
        }
    }

    #[test]
    fn split_sentences_empty() {
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   ").is_empty());
    }

    #[test]
    fn split_sentences_no_abbreviation_handling() {
        // Abbreviations are not special-cased: "e.g." ends a sentence when
        // followed by whitespace... but the fragment is short, so it merges.
        let sentences = split_sentences("See e.g. the documentation for details here.");
        assert_eq!(
            sentences,
            vec!["See e.g. the documentation for details here."]
        );
    }

    #[test]
    fn bm25_scores_relevant_sentences_higher() {
        let sentences = vec![
            "The cat sat on the mat peacefully.".to_owned(),
            "Azure AI Search is a cloud service.".to_owned(),
            "The dog played in the park daily.".to_owned(),
        ];
        let terms = crate::query::analyze("azure search");
        let scores = bm25_score_sentences(&sentences, &terms);
        assert!(scores[1] > scores[0]);
        assert!(scores[1] > scores[2]);
        assert_eq!(scores[0], 0.0);
        assert_eq!(scores[2], 0.0);
    }

    #[test]
    fn bm25_empty_terms_score_zero() {
        let sentences = vec!["Hello world, this is text.".to_owned()];
        assert_eq!(bm25_score_sentences(&sentences, &[]), vec![0.0]);
        assert!(bm25_score_sentences(&[], &["x".to_owned()]).is_empty());
    }

    #[test]
    fn bm25_stemming_matches_inflected_forms() {
        let sentences = vec!["The networks are converging rapidly now.".to_owned()];
        let terms = crate::query::analyze("network");
        let scores = bm25_score_sentences(&sentences, &terms);
        assert!(scores[0] > 0.0);
    }
}
