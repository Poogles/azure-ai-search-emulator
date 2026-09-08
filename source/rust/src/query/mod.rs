//! Phase 1 query engine: basic full-text matching.
//!
//! Semantics (see `docs/phase_1_scaffold_and_e2e.md`):
//! - Case-insensitive substring match across all `searchable: true` string fields.
//! - `*` or an empty search term matches all documents.
//! - A multi-term search term matches a document when every whitespace-separated
//!   term matches at least one searchable string field (Azure simple-query AND
//!   semantics, approximated with substring matching).
//! - No boolean operators, field-specific search, or filters (Phase 2).

use crate::storage::{Document, FieldDefinition};

/// Returns `true` when the search term matches no documents selectively, i.e.
/// it is empty or the match-all wildcard.
#[must_use]
pub fn is_match_all(term: &str) -> bool {
    let trimmed = term.trim();
    trimmed.is_empty() || trimmed == "*"
}

/// Splits a search term into lowercase whitespace-separated tokens.
#[must_use]
pub fn tokenize(term: &str) -> Vec<String> {
    term.split_whitespace().map(str::to_lowercase).collect()
}

/// Case-insensitive substring match of `term` against the searchable string
/// fields of `document`.
#[must_use]
pub fn document_matches(document: &Document, fields: &[FieldDefinition], term: &str) -> bool {
    if is_match_all(term) {
        return true;
    }
    let terms = tokenize(term);
    if terms.is_empty() {
        return true;
    }
    let searchable: Vec<&FieldDefinition> = fields.iter().filter(|f| f.searchable).collect();
    terms.iter().all(|term| {
        searchable.iter().any(|field| {
            document
                .fields
                .get(&field.name)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.to_lowercase().contains(term.as_str()))
        })
    })
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

    fn document(title: &str, body: &str) -> Document {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String("1".to_owned()));
        map.insert("title".to_owned(), Value::String(title.to_owned()));
        map.insert("body".to_owned(), Value::String(body.to_owned()));
        map.insert("count".to_owned(), Value::Number(3.into()));
        Document {
            key: "1".to_owned(),
            fields: map,
        }
    }

    #[test]
    fn empty_and_wildcard_match_all() {
        let doc = document("alpha", "beta");
        let f = fields();
        assert!(document_matches(&doc, &f, ""));
        assert!(document_matches(&doc, &f, "  "));
        assert!(document_matches(&doc, &f, "*"));
    }

    #[test]
    fn case_insensitive_substring_match() {
        let doc = document("Azure Search", "the quick brown fox");
        let f = fields();
        assert!(document_matches(&doc, &f, "azure"));
        assert!(document_matches(&doc, &f, "SEARCH"));
        assert!(document_matches(&doc, &f, "quick"));
        assert!(document_matches(&doc, &f, "brown fox"));
        assert!(!document_matches(&doc, &f, "horse"));
    }

    #[test]
    fn multi_term_requires_all_terms() {
        let doc = document("Azure Search", "the quick brown fox");
        let f = fields();
        assert!(document_matches(&doc, &f, "azure fox"));
        assert!(!document_matches(&doc, &f, "azure horse"));
    }

    #[test]
    fn non_string_fields_are_ignored() {
        let doc = document("nothing", "else");
        let f = fields();
        // "3" only appears in the numeric `count` field, which is not a string.
        assert!(!document_matches(&doc, &f, "3"));
    }

    #[test]
    fn missing_field_values_do_not_match() {
        let mut doc = document("alpha", "beta");
        doc.fields.remove("title");
        let f = fields();
        assert!(!document_matches(&doc, &f, "alpha"));
        assert!(document_matches(&doc, &f, "beta"));
    }

    #[test]
    fn tokenize_lowercases_and_splits() {
        assert_eq!(tokenize("Hello  WORLD"), vec!["hello", "world"]);
        assert!(tokenize("").is_empty());
    }
}
