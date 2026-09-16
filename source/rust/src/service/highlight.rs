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

/// Sentences longer than this are not returned whole: a character window
/// around each match is used instead (matching Azure's excerpt behaviour).
const MAX_SENTENCE_LENGTH: usize = 200;

/// Half-width, in characters, of the window extracted around a match in an
/// over-long sentence.
const FRAGMENT_WINDOW: usize = 100;

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

/// Returns the byte spans of the maximal non-whitespace runs (words) of
/// `text`, in order of appearance. A word includes any trailing punctuation,
/// so `azure.` is a single word.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start {
                spans.push((s, i));
                start = None;
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

/// Returns the byte spans of the whitespace-delimited words of `text` whose
/// analyzed form (under `analyzer`) is in `terms`, in order of appearance.
/// Matching is analyzer-aware, so inflected forms highlight. Shared with the
/// semantic answer/caption highlighter (same matching semantics).
pub(crate) fn matched_word_spans(
    text: &str,
    terms: &BTreeSet<String>,
    analyzer: Option<&str>,
) -> Vec<(usize, usize)> {
    word_spans(text)
        .into_iter()
        .filter_map(|(start, end)| {
            let matched = crate::query::analyze_with(&text[start..end], analyzer)
                .iter()
                .any(|token| terms.contains(token));
            matched.then_some((start, end))
        })
        .collect()
}

/// Wraps each span in `spans` (byte ranges into `text`) with the highlight
/// tags, preserving all other text (spacing and casing) verbatim. Shared
/// with the semantic answer/caption highlighter.
pub(crate) fn wrap_spans(
    text: &str,
    spans: &[(usize, usize)],
    pre_tag: &str,
    post_tag: &str,
) -> String {
    let mut out = String::new();
    let mut last = 0usize;
    for &(start, end) in spans {
        out.push_str(&text[last..start]);
        out.push_str(pre_tag);
        out.push_str(&text[start..end]);
        out.push_str(post_tag);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Computes the highlighted sentence-window fragments of `text` for the
/// query terms analyzed with `analyzer`. Each sentence containing a match
/// contributes a fragment (matched words wrapped in tags), in order of
/// appearance, up to [`MAX_HIGHLIGHT_FRAGMENTS`]. A sentence longer than
/// [`MAX_SENTENCE_LENGTH`] instead contributes a [`FRAGMENT_WINDOW`]-character
/// window around each match (matches already inside an emitted window are
/// not repeated). Returns `None` when no sentence matches.
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
        let spans = matched_word_spans(sentence, terms, analyzer);
        if spans.is_empty() {
            continue;
        }
        if sentence.len() <= MAX_SENTENCE_LENGTH {
            fragments.push(wrap_spans(sentence, &spans, pre_tag, post_tag));
        } else {
            let mut covered: Vec<(usize, usize)> = Vec::new();
            for &(start, end) in &spans {
                if covered.iter().any(|&(ws, we)| start >= ws && end <= we) {
                    continue;
                }
                let ws = sentence.floor_char_boundary(start.saturating_sub(FRAGMENT_WINDOW));
                let we = sentence.ceil_char_boundary((end + FRAGMENT_WINDOW).min(sentence.len()));
                let window = &sentence[ws..we];
                let inner: Vec<(usize, usize)> = spans
                    .iter()
                    .filter(|&&(s, e)| s >= ws && e <= we)
                    .map(|&(s, e)| (s - ws, e - ws))
                    .collect();
                fragments.push(wrap_spans(window, &inner, pre_tag, post_tag));
                covered.push((ws, we));
                if fragments.len() >= MAX_HIGHLIGHT_FRAGMENTS {
                    break;
                }
            }
        }
        if fragments.len() >= MAX_HIGHLIGHT_FRAGMENTS {
            break;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Analyzes raw terms with the default analyzer, mirroring
    /// [`highlight_document`], which passes analyzed terms to
    /// [`highlight_fragments`].
    fn terms(list: &[&str]) -> BTreeSet<String> {
        list.iter()
            .flat_map(|s| crate::query::analyze_with(s, None))
            .collect()
    }

    /// Unwraps a [`highlight_fragments`] result, panicking when no fragment was
    /// produced (the tests always assert a match exists).
    fn frags(opt: Option<Vec<String>>) -> Vec<String> {
        match opt {
            Some(f) => f,
            None => panic!("expected highlight fragments"),
        }
    }

    #[test]
    fn split_sentences_on_terminators() {
        let sentences = split_sentences("Azure is great. Nothing here! Find it? Last one");
        assert_eq!(
            sentences,
            vec![
                "Azure is great.",
                " Nothing here!",
                " Find it?",
                " Last one"
            ]
        );
    }

    #[test]
    fn split_sentences_without_terminator_is_single() {
        assert_eq!(split_sentences("one two three"), vec!["one two three"]);
    }

    #[test]
    fn short_field_is_single_fragment() {
        let fragments = frags(highlight_fragments(
            "Azure Search Rocks",
            &terms(&["azure"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments, vec!["<em>Azure</em> Search Rocks"]);
    }

    #[test]
    fn only_matching_sentences_become_fragments_in_order() {
        let text = "Azure is great. Nothing relevant here. Search finds azure twice.";
        let fragments = frags(highlight_fragments(
            text,
            &terms(&["azure"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments.len(), 2);
        assert!(fragments[0].contains("<em>Azure</em>"));
        assert!(fragments[1].contains("<em>azure</em>"));
    }

    #[test]
    fn at_most_three_fragments_per_field() {
        let text = "a azure. b azure. c azure. d azure. e azure.";
        let fragments = frags(highlight_fragments(
            text,
            &terms(&["azure"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments.len(), MAX_HIGHLIGHT_FRAGMENTS);
    }

    #[test]
    fn no_match_yields_none() {
        assert!(
            highlight_fragments("hello world", &terms(&["azure"]), None, "<em>", "</em>").is_none()
        );
    }

    #[test]
    fn phrase_terms_in_one_sentence_yield_single_fragment() {
        // A phrase query analyzes to its constituent terms; all matches in one
        // sentence produce a single fragment.
        let fragments = frags(highlight_fragments(
            "Azure Search Rocks",
            &terms(&["azure", "search"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0], "<em>Azure</em> <em>Search</em> Rocks");
    }

    #[test]
    fn long_sentence_yields_window_not_whole_sentence() {
        let filler = "word ".repeat(50).trim_end().to_owned();
        let text = format!("{filler} azure {filler}");
        assert!(text.len() > MAX_SENTENCE_LENGTH);
        let fragments = frags(highlight_fragments(
            &text,
            &terms(&["azure"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments.len(), 1);
        assert!(fragments[0].contains("<em>azure</em>"));
        // The fragment is a bounded window (match plus ~100 chars either side,
        // plus the tags), not the whole over-long sentence.
        assert!(fragments[0].len() < text.len());
        assert!(fragments[0].len() <= FRAGMENT_WINDOW * 2 + 50);
    }

    #[test]
    fn long_sentence_with_distant_matches_yields_window_per_match() {
        let filler = "word ".repeat(50).trim_end().to_owned();
        let text = format!("{filler} azure {filler} azure {filler}");
        let fragments = frags(highlight_fragments(
            &text,
            &terms(&["azure"]),
            None,
            "<em>",
            "</em>",
        ));
        assert_eq!(fragments.len(), 2);
        assert!(fragments[0].contains("<em>azure</em>"));
        assert!(fragments[1].contains("<em>azure</em>"));
    }

    #[test]
    fn tags_wrap_only_matched_words() {
        let fragments = frags(highlight_fragments(
            "the azure sky",
            &terms(&["azure"]),
            None,
            "<b>",
            "</b>",
        ));
        assert_eq!(fragments, vec!["the <b>azure</b> sky"]);
    }
}
