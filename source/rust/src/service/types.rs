//! Request/response types for the service layer.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::error::{ApiError, ErrorCode};
use crate::filter::FilterExpr;
use crate::query::{QueryType, SearchMode};
use crate::storage::Document;

/// Per-document result of an indexing operation, in the response shape
/// expected by the pinned Python SDK (`azure-search-documents==11.6.0`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexingResultItem {
    pub key: String,
    #[serde(rename = "status")]
    pub succeeded: bool,
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    #[serde(rename = "errorMessage")]
    pub error_message: Option<String>,
}

impl IndexingResultItem {
    #[must_use]
    pub fn to_value(&self) -> Value {
        crate::error::to_value_or_null(self)
    }
}

/// A document batch action, parsed from the wire format by the API layer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentAction {
    pub kind: ActionKind,
    /// The document fields (for `delete`, only the key field is meaningful).
    pub document: Value,
}

impl DocumentAction {
    /// Parses one upload-batch action from the wire format. Two shapes are
    /// accepted:
    ///   - Documented Azure format: `{"@search.action": "...", "document": {...}}`
    ///   - Python SDK format: `{"@search.action": "...", ...fields}`
    ///     (the SDK spreads the document fields at the top level of the action)
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] when the action is not a JSON object, its
    /// `@search.action` is unsupported, or it has no `document` member.
    /// Takes the action by value so the document moves (not clones) into the
    /// result.
    pub fn from_value(value: Value) -> Result<Self, ApiError> {
        let Value::Object(mut map) = value else {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidDocuments,
                "Each batch action must be a JSON object.",
            ));
        };
        let kind = match map
            .get("@search.action")
            .and_then(Value::as_str)
            .unwrap_or("upload")
        {
            "upload" => ActionKind::Upload,
            "merge" => ActionKind::Merge,
            "mergeOrUpload" => ActionKind::MergeOrUpload,
            "delete" => ActionKind::Delete,
            other => {
                return Err(ApiError::unsupported(
                    ErrorCode::UnsupportedAction,
                    format!("Document action {other:?} is not supported by the emulator."),
                ))
            }
        };
        let document = if let Some(document) = map.remove("document") {
            document
        } else {
            map.remove("@search.action");
            Value::Object(map)
        };
        Ok(Self { kind, document })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Upload,
    Merge,
    MergeOrUpload,
    Delete,
}

/// A parsed search request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    pub search: Option<String>,
    /// The `queryType`: `simple` (the default) or `full` (Lucene).
    pub query_type: QueryType,
    /// How multi-term required clauses combine (`searchMode`).
    pub search_mode: SearchMode,
    pub count: bool,
    pub top: Option<u64>,
    pub skip: u64,
    pub filter: Option<FilterExpr>,
    pub orderby: Vec<OrderBy>,
    pub select: Vec<String>,
    pub facets: Vec<Facet>,
    pub search_fields: Vec<SearchField>,
    /// Fields to highlight (`highlight`); empty means no highlighting.
    pub highlight_fields: Vec<String>,
    /// Tags wrapping highlighted terms (`highlightPreTag` / `highlightPostTag`).
    pub highlight_pre_tag: String,
    pub highlight_post_tag: String,
    pub continuation: Option<String>,
    /// The raw request state bound into continuation tokens.
    pub paging: PagingState,
    /// Parsed `vectorQueries` entries (wire shape; SDK key aliases already
    /// resolved).
    pub vector_queries: Vec<VectorQuery>,
    pub vector_filter_mode: VectorFilterMode,
}

/// The raw request state bound into continuation tokens: the raw `filter`,
/// `orderby`, and `vectorQueries` values that must survive a token round-trip
/// so later pages stay on the same result set and vector-query identity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PagingState {
    /// The raw `filter` string.
    pub filter_raw: Option<String>,
    /// The raw `orderby` string.
    pub orderby_raw: Option<String>,
    /// The raw `vectorQueries` array, binding tokens to the vector identity.
    pub vector_queries_raw: Option<Value>,
}

/// One parsed `searchFields` entry: a field name with its score boost from an
/// optional `field^N` weight (default `1.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchField {
    pub name: String,
    pub boost: f32,
}

/// One parsed `vectorQueries[]` entry: a raw-vector kNN query over one or
/// more vector fields. `weight` scales this query's contribution to the
/// hybrid RRF fusion (default `1.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct VectorQuery {
    pub fields: Vec<String>,
    pub vector: Vec<f32>,
    pub k: usize,
    pub exhaustive: bool,
    pub weight: f32,
}

/// The top-level `vectorFilterMode`: `postFilter` (default) retrieves top-k
/// by similarity then applies `filter`; `preFilter` constrains candidates to
/// `filter` matches before top-k.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VectorFilterMode {
    #[default]
    PostFilter,
    PreFilter,
}

crate::string_enum!(VectorFilterMode both {
    PostFilter => "postFilter",
    PreFilter => "preFilter",
});

/// A document-key predicate for `preFilter` vector search: whether the
/// document with the given key matches the top-level filter.
pub(crate) type KeyPredicate<'a> = Box<dyn Fn(&str) -> bool + 'a>;

/// One `facets` entry: a field (or the special `$count`) with an optional
/// limit on the number of returned facet values (`count:N` / `top:N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facet {
    pub field: String,
    pub limit: Option<usize>,
}

/// One `orderby` clause: a field and its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub field: String,
    pub descending: bool,
}

/// The result of a search operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub total: u64,
    pub documents: Vec<Document>,
    /// Per-document `@search.score` values keyed by document key: BM25
    /// relevance scores for full-text matches (higher is more relevant),
    /// emulator-defined similarity scores for vector matches, best-score-wins
    /// for hybrid matches. Absent keys default to `1.0` at serialization.
    pub scores: BTreeMap<String, f32>,
    /// Per-document `@search.highlights` values keyed by document key: each
    /// maps a highlight field to its highlighted fragments. Empty when no
    /// `highlight` fields were requested.
    pub highlights: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// The `@search.facets` object, when facets were requested.
    pub facets: Option<Value>,
    /// Whether more results exist beyond the returned page.
    pub has_more: bool,
    /// The `skip` value for the next page.
    pub next_skip: u64,
}

/// A single autocomplete completion: the completed term and the query with
/// the completed term appended.
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteCompletion {
    pub text: String,
    pub query_plus_text: String,
}

/// A single suggestion: a matched document plus the word that matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub document: Document,
    pub text: String,
}

/// The `autocompleteMode` for autocomplete: how the search text is completed.
/// Mirrors the Azure wire values (`oneTerm` / `twoTerms` /
/// `oneTermWithContext`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AutocompleteMode {
    /// Complete the last whitespace-separated term (the default).
    #[default]
    OneTerm,
    /// Suggest matching two-term phrases from the index.
    TwoTerms,
    /// Complete the last term, requiring the preceding terms to appear in
    /// the candidate document.
    OneTermWithContext,
}

impl AutocompleteMode {
    /// Parses an `autocompleteMode` value (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns an error string for anything other than `oneTerm` /
    /// `twoTerms` / `oneTermWithContext`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "oneterm" => Ok(Self::OneTerm),
            "twoterms" => Ok(Self::TwoTerms),
            "onetermwithcontext" => Ok(Self::OneTermWithContext),
            _ => Err(format!(
                "Invalid autocompleteMode {value:?}; supported values: \
                 'oneTerm', 'twoTerms', 'oneTermWithContext'."
            )),
        }
    }
}

/// Validated suggest options: the subset of the suggest request affecting
/// matching, ordering, and projection. `minimumCoverage` is accepted but
/// inert (prefix/infix matching is not coverage-based); `select` is empty
/// when all fields are returned (`*` or absent).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SuggestOptions {
    /// Fields to match against (`None` means the suggester's fields).
    pub search_fields: Option<Vec<String>>,
    /// Projected fields (empty means all fields).
    pub select: Vec<String>,
    /// Result ordering (empty means key order).
    pub orderby: Vec<OrderBy>,
    /// Whether typo-tolerant (1-edit) matching applies.
    pub fuzzy: bool,
    /// Highlight tags wrapping the matched portion of `@search.text`
    /// (`None` means no highlighting; both tags are required for effect).
    pub highlight_tags: Option<(String, String)>,
}

/// Validated autocomplete options. `select` does not apply (completions are
/// `text`/`queryPlusText` only); `minimumCoverage` and lone highlight tags
/// are accepted but inert.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutocompleteOptions {
    /// Fields to match against (`None` means the suggester's fields).
    pub search_fields: Option<Vec<String>>,
    /// Candidate ordering (empty means key order).
    pub orderby: Vec<OrderBy>,
    /// Whether typo-tolerant (1-edit) matching applies.
    pub fuzzy: bool,
    /// How the search text is completed.
    pub mode: AutocompleteMode,
}

/// Raw (unvalidated) suggest/autocomplete options, merged from the request
/// body and query string by the API layer. The service layer validates them
/// against the index definition when executing the request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawSuggesterOptions {
    pub search_fields: Option<Value>,
    pub select: Option<Value>,
    pub orderby: Option<Value>,
    pub fuzzy: Option<Value>,
    pub autocomplete_mode: Option<String>,
    pub highlight_pre_tag: Option<String>,
    pub highlight_post_tag: Option<String>,
    pub minimum_coverage: Option<Value>,
}
