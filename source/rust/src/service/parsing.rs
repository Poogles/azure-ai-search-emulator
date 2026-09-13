//! Search-option parsers.

use serde_json::{Map, Value};

use crate::error::ApiError;
use crate::filter::{self, FilterExpr};
use crate::query::SearchMode;
use crate::storage::IndexDefinition;

use super::types::{Facet, OrderBy, SearchField, VectorFilterMode, VectorQuery};
use super::validation::finite_f32;

pub(crate) fn parse_filter_option(raw: &str) -> Result<FilterExpr, ApiError> {
    filter::parse_filter(raw)
        .map_err(|e| ApiError::bad_request("InvalidQuery", format!("Invalid filter: {e}")))
}

/// Extracts a list of comma-separated items from a search option value. The
/// Azure REST API documents these as comma-separated strings, but SDKs may
/// send JSON arrays; both shapes are accepted.
pub(crate) fn string_items(value: &Value, name: &str) -> Result<Vec<String>, ApiError> {
    match value {
        Value::String(text) => Ok(text
            .split(',')
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(text) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_owned());
                        }
                    }
                    None => {
                        return Err(ApiError::bad_request(
                            "InvalidQuery",
                            format!("{name} must be a string or an array of strings."),
                        ))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(ApiError::bad_request(
            "InvalidQuery",
            format!("{name} must be a string or an array of strings."),
        )),
    }
}

/// Parses an `orderby` value: comma-separated `field [asc|desc]` clauses (or
/// a JSON array of clauses). Every field must exist and be marked `sortable`,
/// except the pseudo-field `@search.score`, which orders by relevance score.
/// Returns the parsed clauses and a canonical raw string for continuation
/// tokens.
pub(crate) fn parse_orderby(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<(Vec<OrderBy>, String), ApiError> {
    let parts = string_items(value, "orderby")?;
    let mut clauses = Vec::new();
    for part in &parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = part.split_whitespace().collect();
        let (field, direction) = match tokens.as_slice() {
            [field] => (*field, "asc"),
            [field, direction] => (*field, *direction),
            _ => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Invalid orderby clause {part:?}: expected 'field' or 'field asc|desc'."
                    ),
                ))
            }
        };
        if field.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Invalid orderby clause {part:?}: missing field name."),
            ));
        }
        let descending = match direction.to_ascii_lowercase().as_str() {
            "asc" => false,
            "desc" => true,
            other => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Invalid orderby clause {part:?}: expected direction 'asc' or 'desc', \
                         found {other:?}."
                    ),
                ))
            }
        };
        // `@search.score` is not a schema field: it orders by relevance score
        // (the default ranking when no `orderby` is given).
        if field == "@search.score" {
            clauses.push(OrderBy {
                field: field.to_owned(),
                descending,
            });
            continue;
        }
        let field_def = definition.field(field).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("orderby references unknown field {field:?}."),
            )
        })?;
        if !field_def.sortable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {field:?} is not sortable; mark it \"sortable\": true in the index schema."
                ),
            ));
        }
        clauses.push(OrderBy {
            field: field.to_owned(),
            descending,
        });
    }
    if clauses.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "orderby is empty."));
    }
    Ok((clauses, parts.join(", ")))
}

/// Parses a `select` value: comma-separated field names (or a JSON array),
/// each of which must exist in the schema. The special `*` selects every
/// field, exactly like omitting `select`.
pub(crate) fn parse_select(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<String>, ApiError> {
    let items = string_items(value, "select")?;
    if items.iter().any(|item| item == "*") {
        return Ok(Vec::new());
    }
    let mut fields = Vec::new();
    for part in items {
        if definition.field(&part).is_none() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("select references unknown field {part:?}."),
            ));
        }
        fields.push(part);
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "select is empty."));
    }
    Ok(fields)
}

/// Parses a `facets` value: comma-separated entries (or a JSON array). Each
/// entry is a field name, the special `$count`, or `*` (all facetable fields),
/// optionally followed by `,count:N` (or `,top:N`) to limit the number of
/// returned facet values. Named fields must exist and be marked `facetable`.
pub(crate) fn parse_facets(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<Facet>, ApiError> {
    // A single string is split on commas, with `count:N` / `top:N` fragments
    // re-attached to the preceding facet (so `"tags,count:1"` limits the
    // `tags` facet); array entries keep their inner commas intact.
    let entries: Vec<String> = match value {
        Value::String(text) => split_facet_string(text),
        _ => string_items(value, "facets")?,
    };
    let mut facets = Vec::new();
    for part in entries {
        parse_facet_entry(&part, definition, &mut facets)?;
    }
    if facets.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "facets is empty."));
    }
    Ok(facets)
}

/// Splits a single-string `facets` value on commas, re-attaching `count:N` /
/// `top:N` fragments to the preceding facet entry. A leading option with no
/// facet passes through and is rejected as an unknown field downstream.
pub(crate) fn split_facet_string(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let is_option = part
            .split_once(':')
            .is_some_and(|(key, _)| matches!(key.trim(), "count" | "top"));
        if is_option {
            if let Some(last) = entries.last_mut() {
                last.push_str(", ");
                last.push_str(part);
                continue;
            }
        }
        entries.push(part.to_owned());
    }
    entries
}

/// Parses one facet entry (a field name, `$count`, or `*`, optionally with
/// `,count:N` / `,top:N` limits) against the index schema.
pub(crate) fn parse_facet_entry(
    part: &str,
    definition: &IndexDefinition,
    facets: &mut Vec<Facet>,
) -> Result<(), ApiError> {
    let mut pieces = part.split(',');
    let name = pieces.next().unwrap_or("").trim();
    let mut limit = None;
    for option in pieces {
        let option = option.trim();
        let Some((key, arg)) = option.split_once(':') else {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid facet option {option:?} in {part:?}; expected 'count:N' or 'top:N'."
                ),
            ));
        };
        let count: u64 = arg.trim().parse().map_err(|_| {
            ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid facet option {option:?} in {part:?}; the count must be a non-negative integer."
                ),
            )
        })?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        match key {
            "count" | "top" => limit = Some(n),
            other => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Unsupported facet option {other:?} in {part:?}; supported options: count:N, top:N."
                    ),
                ))
            }
        }
    }
    if name == "*" {
        for field in &definition.fields {
            if field.facetable {
                facets.push(Facet {
                    field: field.name.clone(),
                    limit,
                });
            }
        }
    } else if name == "$count" {
        if limit.is_some() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("The $count facet does not take options (got {part:?})."),
            ));
        }
        facets.push(Facet {
            field: "$count".to_owned(),
            limit: None,
        });
    } else {
        let field_def = definition.field(name).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("facets references unknown field {name:?}."),
            )
        })?;
        if !field_def.facetable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {name:?} is not facetable; mark it \"facetable\": true in the index schema."
                ),
            ));
        }
        facets.push(Facet {
            field: name.to_owned(),
            limit,
        });
    }
    Ok(())
}

/// Parses the `searchMode` option (`search_mode` SDK alias accepted): `any`
/// (OR, the default, matching Azure) or `all` (AND).
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the value is present but
/// not one of the two supported modes.
pub(crate) fn parse_search_mode(obj: &Map<String, Value>) -> Result<SearchMode, ApiError> {
    match obj.get("searchMode").or_else(|| obj.get("search_mode")) {
        None | Some(Value::Null) => Ok(SearchMode::default()),
        Some(Value::String(mode)) => {
            SearchMode::parse(mode).map_err(|e| ApiError::bad_request("InvalidQuery", e))
        }
        Some(_) => Err(ApiError::bad_request(
            "InvalidQuery",
            "searchMode must be 'all' or 'any'.",
        )),
    }
}

/// Parses the `count`/`top`/`skip` paging options: `top`/`skip` must be
/// non-negative integers when present.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when `top`/`skip` is
/// present but not a non-negative integer.
pub(crate) fn parse_paging_options(
    obj: &Map<String, Value>,
) -> Result<(bool, Option<u64>, u64), ApiError> {
    let count = obj.get("count").and_then(Value::as_bool).unwrap_or(false);
    let top = match obj.get("top") {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"top\" must be a non-negative integer.")
        })?),
    };
    let skip = match obj.get("skip") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"skip\" must be a non-negative integer.")
        })?,
    };
    Ok((count, top, skip))
}

/// Parses the top-level vector options: `vectorQueries`
/// (`vector_queries` SDK alias accepted) plus `vectorFilterMode`
/// (`vector_filter_mode` SDK alias accepted).
///
/// # Errors
///
/// Returns an [`ApiError`] when the filter mode or any vector query is
/// malformed (see [`parse_vector_filter_mode`], [`parse_vector_queries`]).
pub(crate) fn parse_vector_options(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<VectorQuery>, Option<Value>, VectorFilterMode), ApiError> {
    let vector_filter_mode = parse_vector_filter_mode(
        obj.get("vectorFilterMode")
            .or_else(|| obj.get("vector_filter_mode")),
    )?;
    let (vector_queries, vector_queries_raw) = parse_vector_queries(
        obj.get("vectorQueries")
            .or_else(|| obj.get("vector_queries")),
        definition,
    )?;
    Ok((vector_queries, vector_queries_raw, vector_filter_mode))
}

/// Maximum `vectorQueries` entries per search (matches Azure).
const MAX_VECTOR_QUERIES: usize = 5;
/// Maximum `k` per vector query (matches Azure).
const MAX_VECTOR_K: usize = 1000;
/// Default `k` when omitted (matches the SDK default).
const DEFAULT_VECTOR_K: usize = 3;

/// Parses the top-level `vectorFilterMode` (`vector_filter_mode` SDK alias
/// accepted): `postFilter` (default) or `preFilter`.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the value is present
/// but not one of the two supported modes.
pub(crate) fn parse_vector_filter_mode(
    value: Option<&Value>,
) -> Result<VectorFilterMode, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(VectorFilterMode::PostFilter),
        Some(Value::String(mode)) => match mode.as_str() {
            "postFilter" => Ok(VectorFilterMode::PostFilter),
            "preFilter" => Ok(VectorFilterMode::PreFilter),
            other => Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid vectorFilterMode {other:?}; supported values: 'preFilter', 'postFilter'."
                ),
            )),
        },
        Some(_) => Err(ApiError::bad_request(
            "InvalidQuery",
            "vectorFilterMode must be 'preFilter' or 'postFilter'.",
        )),
    }
}

/// Parses the top-level `vectorQueries` array against the index schema,
/// returning the parsed queries plus the raw array (bound into continuation
/// tokens). SDK key aliases (`k_nearest_neighbors`, `vector_queries`) are
/// accepted; the service layer otherwise sees the wire shape only.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`, or `400 UnsupportedQuery`
/// for `kind: "text"` vectorizer queries) when the array or any entry is
/// malformed.
pub(crate) fn parse_vector_queries(
    value: Option<&Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<VectorQuery>, Option<Value>), ApiError> {
    let Some(raw) = value else {
        return Ok((Vec::new(), None));
    };
    if raw.is_null() {
        return Ok((Vec::new(), None));
    }
    let entries = raw
        .as_array()
        .ok_or_else(|| ApiError::bad_request("InvalidQuery", "vectorQueries must be an array."))?;
    if entries.is_empty() {
        return Ok((Vec::new(), None));
    }
    if entries.len() > MAX_VECTOR_QUERIES {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            format!("At most {MAX_VECTOR_QUERIES} vector queries are supported."),
        ));
    }
    let mut queries = Vec::with_capacity(entries.len());
    for entry in entries {
        queries.push(parse_vector_query(entry, definition)?);
    }
    Ok((queries, Some(raw.clone())))
}

/// Parses one `vectorQueries[]` entry: `kind`, `vector`, `fields` (string or
/// array), `k` (`k_nearest_neighbors` SDK alias accepted), `exhaustive`.
/// `weight` is accepted but inert. A missing `kind` defaults to `"vector"`
/// (emulator-only leniency, documented in `known_differences.md`).
pub(crate) fn parse_vector_query(
    entry: &Value,
    definition: &IndexDefinition,
) -> Result<VectorQuery, ApiError> {
    let obj = entry.as_object().ok_or_else(|| {
        ApiError::bad_request(
            "InvalidQuery",
            "Each vectorQueries entry must be a JSON object.",
        )
    })?;
    match obj.get("kind").and_then(Value::as_str) {
        None | Some("vector") => {}
        Some("text") => {
            return Err(ApiError::unsupported(
                "UnsupportedQuery",
                "Vectorizer queries (kind 'text') are not supported; supply raw vectors with kind 'vector'.",
            ));
        }
        Some(other) => {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Invalid vector query kind {other:?}; supported kinds: 'vector'."),
            ));
        }
    }
    let (fields, expected) = parse_vector_query_fields(obj, definition)?;
    let vector = parse_vector_query_vector(obj, &fields, expected)?;
    let k = match obj
        .get("k")
        .or_else(|| obj.get("k_nearest_neighbors"))
        .or_else(|| obj.get("kNearestNeighbors"))
    {
        None | Some(Value::Null) => DEFAULT_VECTOR_K,
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n >= 1 && *n <= MAX_VECTOR_K)
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    "Vector query 'k' must be a positive integer (max 1000).",
                )
            })?,
    };
    let exhaustive = obj
        .get("exhaustive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `weight` scales this query's contribution to the hybrid RRF fusion; it
    // must be a finite positive number (default `1.0` when absent).
    let weight = match obj.get("weight") {
        None | Some(Value::Null) => 1.0,
        Some(value) => {
            let invalid = || {
                ApiError::bad_request(
                    "InvalidQuery",
                    "Vector query 'weight' must be a finite positive number.",
                )
            };
            let as_f64 = value.as_f64().ok_or_else(invalid)?;
            let weight = finite_f32(as_f64).ok_or_else(invalid)?;
            if weight <= 0.0 {
                return Err(invalid());
            }
            weight
        }
    };
    Ok(VectorQuery {
        fields,
        vector,
        k,
        exhaustive,
        weight,
    })
}

/// Parses a vector query's `fields`: every entry must be a vector field in
/// the schema. Returns the field names plus the shared dimension the query
/// vector must match (fields with different dimensions cannot share one
/// query vector).
pub(crate) fn parse_vector_query_fields(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<String>, usize), ApiError> {
    let fields_value = obj.get("fields").ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "Each vector query must define \"fields\".")
    })?;
    let fields = string_items(fields_value, "vector query fields")?;
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            "Vector query \"fields\" is empty.",
        ));
    }
    let mut expected: Option<usize> = None;
    for name in &fields {
        let field_def = definition
            .field(name)
            .filter(|f| f.is_vector_field())
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Vector query field {name:?} is not a vector field in index {:?}.",
                        definition.name
                    ),
                )
            })?;
        let dimensions = field_def.vector_dimensions.unwrap_or(0);
        match expected {
            None => expected = Some(dimensions),
            Some(d) if d == dimensions => {}
            Some(d) => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Vector query targets fields with different dimensions ({d} vs {dimensions})."
                    ),
                ));
            }
        }
    }
    Ok((fields, expected.unwrap_or(0)))
}

/// Parses a vector query's `vector`: an array of finite numbers whose length
/// matches every listed field's dimensions.
pub(crate) fn parse_vector_query_vector(
    obj: &Map<String, Value>,
    fields: &[String],
    expected: usize,
) -> Result<Vec<f32>, ApiError> {
    let raw_vector = obj.get("vector").ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "Each vector query must define \"vector\".")
    })?;
    let items = raw_vector.as_array().ok_or_else(|| {
        ApiError::bad_request(
            "InvalidQuery",
            "Vector query \"vector\" must be an array of numbers.",
        )
    })?;
    let first_field = fields.first().map_or("", String::as_str);
    if items.len() != expected {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            format!(
                "Vector query for {first_field:?} has dimension {}, expected {expected}.",
                items.len()
            ),
        ));
    }
    let mut vector = Vec::with_capacity(items.len());
    for item in items {
        match item.as_f64().and_then(finite_f32) {
            Some(narrowed) => vector.push(narrowed),
            None if item.is_number() => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!("Vector query for {first_field:?} contains non-finite values."),
                ));
            }
            None => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!("Vector query for {first_field:?} must contain only numeric values."),
                ));
            }
        }
    }
    Ok(vector)
}

/// Parses a `searchFields` value: comma-separated field names (or a JSON
/// array), optionally weighted (`field^2`). Fields must exist and be marked
/// `searchable`. Weights must be finite positive numbers and scale the
/// field's BM25 contribution to `@search.score`.
pub(crate) fn parse_search_fields(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<SearchField>, ApiError> {
    let mut fields = Vec::new();
    for part in string_items(value, "searchFields")? {
        let (name_part, weight) = match part.split_once('^') {
            None => (part.as_str(), 1.0),
            Some((name, raw_weight)) => {
                let weight: f32 = raw_weight.trim().parse().map_err(|_| {
                    ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Invalid searchFields weight in {part:?}; \
                             expected a positive number (e.g. 'field^2')."
                        ),
                    )
                })?;
                if !weight.is_finite() || weight <= 0.0 {
                    return Err(ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Invalid searchFields weight in {part:?}; \
                             expected a finite positive number."
                        ),
                    ));
                }
                (name, weight)
            }
        };
        let name = name_part.trim();
        let field_def = definition.field_path(name).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("searchFields references unknown field {name:?}."),
            )
        })?;
        if !field_def.searchable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {name:?} is not searchable; mark it \"searchable\": true in the index schema."
                ),
            ));
        }
        if field_def.is_vector_field() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Field {name:?} is a vector field and cannot be used in searchFields."),
            ));
        }
        fields.push(SearchField {
            name: name.to_owned(),
            boost: weight,
        });
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            "searchFields is empty.",
        ));
    }
    Ok(fields)
}

/// Parses the `highlight` / `highlightPreTag` / `highlightPostTag` options.
/// Highlight fields must exist and be marked `searchable`. Tags default to
/// `<em>` / `</em>` (the Azure defaults).
pub(crate) fn parse_highlight_options(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<String>, String, String), ApiError> {
    let highlight_value = obj.get("highlight");
    let fields = match highlight_value {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let mut fields = Vec::new();
            for part in string_items(value, "highlight")? {
                let field_def = definition.field_path(&part).ok_or_else(|| {
                    ApiError::bad_request(
                        "InvalidQuery",
                        format!("highlight references unknown field {part:?}."),
                    )
                })?;
                if !field_def.searchable {
                    return Err(ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Field {part:?} is not searchable; only searchable fields can be highlighted."
                        ),
                    ));
                }
                fields.push(part);
            }
            if fields.is_empty() {
                return Err(ApiError::bad_request("InvalidQuery", "highlight is empty."));
            }
            fields
        }
    };
    let tag = |key: &str, default: &str| -> Result<String, ApiError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(default.to_owned()),
            Some(Value::String(tag)) => Ok(tag.clone()),
            Some(_) => Err(ApiError::bad_request(
                "InvalidQuery",
                format!("{key} must be a string."),
            )),
        }
    };
    let pre_tag = tag("highlightPreTag", "<em>")?;
    let post_tag = tag("highlightPostTag", "</em>")?;
    Ok((fields, pre_tag, post_tag))
}

/// Search request options that are not implemented and must be rejected with
/// an explicit error.
pub(crate) const UNSUPPORTED_SEARCH_OPTIONS: &[&str] = &[
    "scoringProfile",
    "scoringParameters",
    "scoringStatistics",
    "minimumCoverage",
    "answers",
    "captions",
    "semantic",
    "semanticConfiguration",
    "semanticQuery",
    "semanticErrorHandling",
    "semanticMaxWaitInMilliseconds",
    "debug",
];
