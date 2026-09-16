//! Caption extraction: leading-sentence selection with 200-character
//! word-boundary truncation, highlights, and nested caption answers.

use std::collections::BTreeSet;

use super::answers::{bm25_score_sentences, highlight_text, split_sentences};
use super::{SemanticCaption, SemanticCaptionAnswer, SemanticConfiguration, SemanticQuery};
use crate::storage::Document;

/// The maximum character length of a caption before truncation.
pub const CAPTION_MAX_LENGTH: usize = 200;

/// Suffix appended to truncated captions.
const TRUNCATION_SUFFIX: &str = "...";

/// Truncates text to at most `max_len` characters: when the text fits it is
/// returned unchanged, otherwise it is cut at the last word boundary before
/// `max_len - suffix` characters and the suffix is appended (hard-split when
/// there is no word boundary). The result never exceeds `max_len`
/// characters. Splits on character (not byte) boundaries.
#[must_use]
pub fn truncate(text: &str, max_len: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_len {
        return text.to_owned();
    }
    let keep = max_len.saturating_sub(TRUNCATION_SUFFIX.chars().count());
    let prefix: String = text.chars().take(keep).collect();
    let cut = prefix.rfind(char::is_whitespace).unwrap_or(prefix.len());
    let mut out = prefix[..cut].trim_end().to_owned();
    out.push_str(TRUNCATION_SUFFIX);
    out
}

/// Selects captions for a document: the first `count` sentences from the
/// caption fields in priority order (falling back to the next field when the
/// current one is empty or exhausted), each truncated to
/// [`CAPTION_MAX_LENGTH`] characters. Nested `captions.answers` are extracted
/// from each caption's text with the answer sentence scorer scoped to that
/// text only.
#[must_use]
pub fn select_captions(
    doc: &Document,
    config: &SemanticConfiguration,
    semantic_query: &SemanticQuery,
    terms: &[String],
    pre_tag: &str,
    post_tag: &str,
) -> Vec<SemanticCaption> {
    let Some(requested) = &semantic_query.captions else {
        return Vec::new();
    };
    if config.captions_fields.is_empty() {
        return Vec::new();
    }
    let term_set: BTreeSet<String> = terms.iter().cloned().collect();
    let mut captions = Vec::new();
    for field in &config.captions_fields {
        if captions.len() >= requested.count {
            break;
        }
        let text = super::answers::field_text(doc, field);
        if text.trim().is_empty() {
            continue;
        }
        for sentence in split_sentences(&text) {
            if captions.len() >= requested.count {
                break;
            }
            let caption_text = truncate(&sentence, CAPTION_MAX_LENGTH);
            let nested = match &requested.answers {
                Some(nested) => select_nested_answers(
                    &caption_text,
                    terms,
                    nested.count,
                    &term_set,
                    pre_tag,
                    post_tag,
                ),
                None => Vec::new(),
            };
            captions.push(SemanticCaption {
                highlights: highlight_text(&caption_text, &term_set, pre_tag, post_tag),
                text: caption_text,
                answers: nested,
            });
        }
    }
    captions
}

/// Extracts nested answers from a caption's text: the caption's sentences
/// scored against the query terms, top-N with a positive score, normalized
/// to 0.0–1.0 within the caption scope.
fn select_nested_answers(
    caption_text: &str,
    terms: &[String],
    count: usize,
    term_set: &BTreeSet<String>,
    pre_tag: &str,
    post_tag: &str,
) -> Vec<SemanticCaptionAnswer> {
    if terms.is_empty() || count == 0 {
        return Vec::new();
    }
    let sentences = split_sentences(caption_text);
    if sentences.is_empty() {
        return Vec::new();
    }
    let scores = bm25_score_sentences(&sentences, terms);
    let mut candidates: Vec<(String, f32)> = sentences
        .into_iter()
        .zip(scores)
        .filter(|(_, score)| *score > 0.0)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    candidates.truncate(count);
    let max_score = candidates
        .iter()
        .map(|(_, score)| *score)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(1.0)
        .max(f32::EPSILON);
    candidates
        .into_iter()
        .map(|(text, score)| SemanticCaptionAnswer {
            highlights: highlight_text(&text, term_set, pre_tag, post_tag),
            text,
            score: score / max_score,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate("hello world", 200), "hello world");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        let text = "a".repeat(200);
        assert_eq!(truncate(&text, 200), text);
    }

    #[test]
    fn truncate_cuts_at_word_boundary_with_ellipsis() {
        let text = format!("{} tail", "word ".repeat(60));
        let result = truncate(&text, 200);
        assert!(result.chars().count() <= 200);
        assert!(result.ends_with("..."));
        assert!(!result.contains("tail"));
        // No partial word before the ellipsis.
        assert!(result[..result.len() - 3].ends_with("word"));
    }

    #[test]
    fn truncate_hard_splits_without_word_boundary() {
        let text = "a".repeat(250);
        let result = truncate(&text, 200);
        assert_eq!(result.chars().count(), 200);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_multibyte_safe() {
        let text = "\u{00e9}".repeat(250);
        let result = truncate(&text, 200);
        assert_eq!(result.chars().count(), 200);
    }

    #[test]
    fn truncate_trims_surrounding_whitespace() {
        assert_eq!(truncate("  hello world  ", 200), "hello world");
    }
}
