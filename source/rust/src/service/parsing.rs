//! Search-option parsers.

use serde_json::{Map, Value};

use crate::error::{ApiError, ErrorCode};
use crate::filter::{self, FilterExpr};
use crate::query::SearchMode;
use crate::storage::{IndexDefinition, Suggester};

use super::types::{Facet, OrderBy, SearchField, VectorFilterMode, VectorQuery};
use super::validation::{finite_f32, parse_finite_f32_array, FiniteF32ArrayError};

pub(crate) fn parse_filter_option(raw: &str) -> Result<FilterExpr, ApiError> {
    filter::parse_filter(raw).map_err(|e| ApiError::invalid_query(format!("Invalid filter: {e}")))
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
                        return Err(ApiError::invalid_query(format!(
                            "{name} must be a string or an array of strings."
                        )))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(ApiError::invalid_query(format!(
            "{name} must be a string or an array of strings."
        ))),
    }
}

/// Maps each comma-separated item from a search option value through
/// `per_item`, collecting the results. `string_items` already trims and drops
/// empties, so `per_item` sees only non-empty items and callers must not
/// re-trim or re-filter.
pub(crate) fn string_items_or<T>(
    value: &Value,
    name: &str,
    per_item: impl FnMut(String) -> Result<T, ApiError>,
) -> Result<Vec<T>, ApiError> {
    string_items(value, name)?
        .into_iter()
        .map(per_item)
        .collect()
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
    // `string_items` already trims and drops empties; `part` needs no
    // re-trim or empty check here.
    let parts = string_items(value, "orderby")?;
    let mut clauses = Vec::new();
    for part in &parts {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        let (field, direction) = match tokens.as_slice() {
            [field] => (*field, "asc"),
            [field, direction] => (*field, *direction),
            _ => {
                return Err(ApiError::invalid_query(format!(
                    "Invalid orderby clause {part:?}: expected 'field' or 'field asc|desc'."
                )))
            }
        };
        if field.is_empty() {
            return Err(ApiError::invalid_query(format!(
                "Invalid orderby clause {part:?}: missing field name."
            )));
        }
        let descending = match direction.to_ascii_lowercase().as_str() {
            "asc" => false,
            "desc" => true,
            other => {
                return Err(ApiError::invalid_query(format!(
                    "Invalid orderby clause {part:?}: expected direction 'asc' or 'desc', \
                         found {other:?}."
                )))
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
        let field_def = definition.field_path(field).ok_or_else(|| {
            ApiError::invalid_query(format!("orderby references unknown field {field:?}."))
        })?;
        if !field_def.sortable {
            return Err(ApiError::invalid_query(format!(
                "Field {field:?} is not sortable; mark it \"sortable\": true in the index schema."
            )));
        }
        clauses.push(OrderBy {
            field: field.to_owned(),
            descending,
        });
    }
    if clauses.is_empty() {
        return Err(ApiError::invalid_query("orderby is empty."));
    }
    Ok((clauses, parts.join(", ")))
}

/// Parses a `select` value: comma-separated field names or paths (or a JSON
/// array), each of which must resolve in the schema (nested complex-type
/// paths such as `Address/City` are supported). The special `*` selects
/// every field, exactly like omitting `select`.
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
        if definition.field_path(&part).is_none() {
            return Err(ApiError::invalid_query(format!(
                "select references unknown field {part:?}."
            )));
        }
        fields.push(part);
    }
    if fields.is_empty() {
        return Err(ApiError::invalid_query("select is empty."));
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
        return Err(ApiError::invalid_query("facets is empty."));
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
            return Err(ApiError::invalid_query(format!(
                "Invalid facet option {option:?} in {part:?}; expected 'count:N' or 'top:N'."
            )));
        };
        let count: u64 = arg.trim().parse().map_err(|_| {
            ApiError::invalid_query(
                format!(
                    "Invalid facet option {option:?} in {part:?}; the count must be a non-negative integer."
                ),
            )
        })?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        match key {
            "count" | "top" => limit = Some(n),
            other => {
                return Err(ApiError::invalid_query(
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
            return Err(ApiError::invalid_query(format!(
                "The $count facet does not take options (got {part:?})."
            )));
        }
        facets.push(Facet {
            field: "$count".to_owned(),
            limit: None,
        });
    } else {
        let field_def = definition.field_path(name).ok_or_else(|| {
            ApiError::invalid_query(format!("facets references unknown field {name:?}."))
        })?;
        if !field_def.facetable {
            return Err(ApiError::invalid_query(format!(
                "Field {name:?} is not facetable; mark it \"facetable\": true in the index schema."
            )));
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
        Some(Value::String(mode)) => SearchMode::parse(mode).map_err(ApiError::invalid_query),
        Some(_) => Err(ApiError::invalid_query(
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
    let top =
        match obj.get("top") {
            None => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                ApiError::invalid_query("\"top\" must be a non-negative integer.")
            })?),
        };
    let skip = match obj.get("skip") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| ApiError::invalid_query("\"skip\" must be a non-negative integer."))?,
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
        Some(Value::String(mode)) => VectorFilterMode::parse(mode).ok_or_else(|| {
            ApiError::invalid_query(format!(
                "Invalid vectorFilterMode {mode:?}; supported values: 'preFilter', 'postFilter'."
            ))
        }),
        Some(_) => Err(ApiError::invalid_query(
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
        .ok_or_else(|| ApiError::invalid_query("vectorQueries must be an array."))?;
    if entries.is_empty() {
        return Ok((Vec::new(), None));
    }
    if entries.len() > MAX_VECTOR_QUERIES {
        return Err(ApiError::invalid_query(format!(
            "At most {MAX_VECTOR_QUERIES} vector queries are supported."
        )));
    }
    let mut queries = Vec::with_capacity(entries.len());
    for entry in entries {
        queries.push(parse_vector_query(entry, definition)?);
    }
    Ok((queries, Some(raw.clone())))
}

/// Parses one `vectorQueries[]` entry: `kind`, `vector` (or `text` for
/// `kind: "text"` vectorizer queries), `fields` (string or array), `k`
/// (`k_nearest_neighbors` SDK alias accepted), `exhaustive`. `weight` is
/// accepted but inert. A missing `kind` defaults to `"vector"` (emulator-only
/// leniency, documented in `known_differences.md`).
///
/// For `kind: "text"` the query's `text` is vectorized with the target
/// field's vectorizer (a deterministic hash-based embedding, Phase 2.4); the
/// generated vector is stored on the [`VectorQuery`] and executed like a raw
/// vector. `text` must be present and non-empty, `vector` must be absent, and
/// every target field must reference a vectorizer.
pub(crate) fn parse_vector_query(
    entry: &Value,
    definition: &IndexDefinition,
) -> Result<VectorQuery, ApiError> {
    let obj = entry.as_object().ok_or_else(|| {
        ApiError::invalid_query("Each vectorQueries entry must be a JSON object.")
    })?;
    let is_text = matches!(obj.get("kind").and_then(Value::as_str), Some("text"));
    match obj.get("kind").and_then(Value::as_str) {
        None | Some("vector" | "text") => {}
        Some(other) => {
            return Err(ApiError::invalid_query(format!(
                "Invalid vector query kind {other:?}; supported kinds: 'vector', 'text'."
            )));
        }
    }
    let fields = VectorFieldSet::parse(obj, definition, is_text)?;
    let vector = if is_text {
        parse_vectorizer_query_vector(obj, &fields)?
    } else {
        fields.parse_vector(obj)?
    };
    let k = parse_vector_k(obj)?;
    let exhaustive = obj
        .get("exhaustive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `weight` scales this query's contribution to the hybrid RRF fusion; it
    // must be a finite positive number (default `1.0` when absent).
    let weight = parse_vector_weight(obj)?;
    Ok(VectorQuery {
        fields: fields.names,
        vector,
        k,
        exhaustive,
        weight,
    })
}

/// Parses a `kind: "text"` vectorizer query's vector: rejects a present
/// `vector` property, requires a non-empty `text`, and vectorizes it with the
/// target fields' shared dimension.
fn parse_vectorizer_query_vector(
    obj: &Map<String, Value>,
    fields: &VectorFieldSet,
) -> Result<Vec<f32>, ApiError> {
    if obj.contains_key("vector") {
        return Err(ApiError::invalid_query(
            "Vectorizer query (kind 'text') must not include a 'vector' property.",
        ));
    }
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::invalid_query("Vectorizer query requires a non-empty 'text' property.")
        })?;
    Ok(crate::vector::vectorizer::text_to_vector(text, fields.dim))
}

/// Parses a vector query's `k` (`k_nearest_neighbors` / `kNearestNeighbors`
/// SDK aliases accepted): a positive integer up to [`MAX_VECTOR_K`] (default
/// [`DEFAULT_VECTOR_K`] when absent).
fn parse_vector_k(obj: &Map<String, Value>) -> Result<usize, ApiError> {
    match obj
        .get("k")
        .or_else(|| obj.get("k_nearest_neighbors"))
        .or_else(|| obj.get("kNearestNeighbors"))
    {
        None | Some(Value::Null) => Ok(DEFAULT_VECTOR_K),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n >= 1 && *n <= MAX_VECTOR_K)
            .ok_or_else(|| {
                ApiError::invalid_query("Vector query 'k' must be a positive integer (max 1000).")
            }),
    }
}

/// Parses a vector query's `weight`: a finite positive number (default
/// `1.0` when absent).
fn parse_vector_weight(obj: &Map<String, Value>) -> Result<f32, ApiError> {
    let invalid =
        || ApiError::invalid_query("Vector query 'weight' must be a finite positive number.");
    match obj.get("weight") {
        None | Some(Value::Null) => Ok(1.0),
        Some(value) => {
            let as_f64 = value.as_f64().ok_or_else(invalid)?;
            let weight = finite_f32(as_f64).ok_or_else(invalid)?;
            if !is_finite_positive(weight) {
                return Err(invalid());
            }
            Ok(weight)
        }
    }
}

/// Whether a weight is usable: a finite positive number.
fn is_finite_positive(weight: f32) -> bool {
    weight.is_finite() && weight > 0.0
}

/// A vector query's resolved field set: the field names and the shared
/// dimension every query vector must match (fields with different
/// dimensions cannot share one query vector).
struct VectorFieldSet {
    names: Vec<String>,
    dim: usize,
}

impl VectorFieldSet {
    /// Parses a vector query's `fields`: every entry must be a vector field
    /// in the schema. When `require_vectorizer` is set (a `kind: "text"`
    /// query), each field must also reference a vectorizer.
    fn parse(
        obj: &Map<String, Value>,
        definition: &IndexDefinition,
        require_vectorizer: bool,
    ) -> Result<Self, ApiError> {
        let fields_value = obj
            .get("fields")
            .ok_or_else(|| ApiError::invalid_query("Each vector query must define \"fields\"."))?;
        let names = string_items(fields_value, "vector query fields")?;
        if names.is_empty() {
            return Err(ApiError::invalid_query("Vector query \"fields\" is empty."));
        }
        let mut dim: Option<usize> = None;
        for name in &names {
            let field_def = definition
                .field(name)
                .filter(|f| f.is_vector_field())
                .ok_or_else(|| {
                    ApiError::invalid_query(format!(
                        "Vector query field {name:?} is not a vector field in index {:?}.",
                        definition.name
                    ))
                })?;
            if require_vectorizer
                && crate::vector::vectorizer::field_vectorizer(definition, field_def).is_none()
            {
                return Err(ApiError::invalid_query(format!(
                    "Vector field {name:?} does not have a vectorizer; \
                     use kind 'vector' with an explicit vector.",
                )));
            }
            let dimensions = field_def.vector_dimensions.unwrap_or(0);
            match dim {
                None => dim = Some(dimensions),
                Some(d) if d == dimensions => {}
                Some(d) => {
                    return Err(ApiError::invalid_query(
                        format!(
                            "Vector query targets fields with different dimensions ({d} vs {dimensions})."
                        ),
                    ));
                }
            }
        }
        Ok(Self {
            names,
            dim: dim.unwrap_or(0),
        })
    }

    /// Parses the query's `vector` against this field set: an array of
    /// finite numbers whose length matches the shared dimension.
    fn parse_vector(&self, obj: &Map<String, Value>) -> Result<Vec<f32>, ApiError> {
        let raw_vector = obj
            .get("vector")
            .ok_or_else(|| ApiError::invalid_query("Each vector query must define \"vector\"."))?;
        let items = raw_vector.as_array().ok_or_else(|| {
            ApiError::invalid_query("Vector query \"vector\" must be an array of numbers.")
        })?;
        self.check_vector_len(items.len())?;
        parse_finite_f32_array(items).map_err(|error| {
            let message = match error {
                FiniteF32ArrayError::NonFinite => format!(
                    "Vector query for {:?} contains non-finite values.",
                    self.first_field()
                ),
                FiniteF32ArrayError::NonNumeric => format!(
                    "Vector query for {:?} must contain only numeric values.",
                    self.first_field()
                ),
            };
            ApiError::invalid_query(message)
        })
    }

    /// Checks a vector's length against the shared dimension.
    fn check_vector_len(&self, len: usize) -> Result<(), ApiError> {
        if len != self.dim {
            return Err(ApiError::invalid_query(format!(
                "Vector query for {:?} has dimension {}, expected {}.",
                self.first_field(),
                len,
                self.dim
            )));
        }
        Ok(())
    }

    /// The first field name, for error messages.
    fn first_field(&self) -> &str {
        self.names.first().map_or("", String::as_str)
    }
}

/// Parses a `searchFields` value: comma-separated field names (or a JSON
/// array), optionally weighted (`field^2`). Fields must exist and be marked
/// `searchable`. Weights must be finite positive numbers and scale the
/// field's BM25 contribution to `@search.score`.
pub(crate) fn parse_search_fields(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<SearchField>, ApiError> {
    let fields = string_items_or(value, "searchFields", |part| {
        let (name_part, weight) = match part.split_once('^') {
            None => (part.as_str(), 1.0),
            Some((name, raw_weight)) => {
                let weight: f32 = raw_weight.trim().parse().map_err(|_| {
                    ApiError::invalid_query(format!(
                        "Invalid searchFields weight in {part:?}; \
                             expected a positive number (e.g. 'field^2')."
                    ))
                })?;
                if !is_finite_positive(weight) {
                    return Err(ApiError::invalid_query(format!(
                        "Invalid searchFields weight in {part:?}; \
                             expected a finite positive number."
                    )));
                }
                (name, weight)
            }
        };
        let name = name_part.trim();
        let field_def = definition.field_path(name).ok_or_else(|| {
            ApiError::invalid_query(format!("searchFields references unknown field {name:?}."))
        })?;
        if !field_def.searchable {
            return Err(ApiError::invalid_query(
                format!(
                    "Field {name:?} is not searchable; mark it \"searchable\": true in the index schema."
                ),
            ));
        }
        if field_def.is_vector_field() {
            return Err(ApiError::invalid_query(format!(
                "Field {name:?} is a vector field and cannot be used in searchFields."
            )));
        }
        Ok(SearchField {
            name: name.to_owned(),
            boost: weight,
        })
    })?;
    if fields.is_empty() {
        return Err(ApiError::invalid_query("searchFields is empty."));
    }
    Ok(fields)
}

/// Parses `searchFields` for the suggest/autocomplete routes: each entry must
/// be a searchable field of the index (weights are not supported here; a
/// `field^N` entry is rejected) and must belong to the suggester's configured
/// fields. Returns `None` when absent (all of the suggester's fields apply).
pub(crate) fn parse_suggester_search_fields(
    value: Option<&Value>,
    definition: &IndexDefinition,
    suggester: &Suggester,
) -> Result<Option<Vec<String>>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut fields = Vec::new();
    for part in string_items(value, "searchFields")? {
        if part.contains('^') {
            return Err(ApiError::invalid_query(format!(
                "Invalid searchFields entry {part:?} on suggest/autocomplete; \
                 weights ('field^N') are only supported on the search route."
            )));
        }
        let field_def = definition.field_path(&part).ok_or_else(|| {
            ApiError::invalid_query(format!("searchFields references unknown field {part:?}."))
        })?;
        if !field_def.searchable {
            return Err(ApiError::invalid_query(format!(
                "Field {part:?} is not searchable; only searchable fields can be used in searchFields."
            )));
        }
        if !suggester.search_fields.iter().any(|f| f == &part) {
            return Err(ApiError::invalid_query(format!(
                "Field {part:?} is not part of the suggester; searchFields must be \
                 a subset of the suggester's search fields."
            )));
        }
        fields.push(part);
    }
    if fields.is_empty() {
        return Err(ApiError::invalid_query("searchFields is empty."));
    }
    Ok(Some(fields))
}

/// Parses `orderby` for the suggest/autocomplete routes: same validation as
/// the search route (fields must exist and be `sortable`), except the
/// pseudo-field `@search.score` is rejected (these routes compute no score).
pub(crate) fn parse_suggester_orderby(
    value: Option<&Value>,
    definition: &IndexDefinition,
) -> Result<Vec<OrderBy>, ApiError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let (clauses, _) = parse_orderby(value, definition)?;
    if clauses.iter().any(|c| c.field == "@search.score") {
        return Err(ApiError::invalid_query(
            "orderby references \"@search.score\", which is not available on \
             the suggest/autocomplete routes.",
        ));
    }
    Ok(clauses)
}

/// Parses the `fuzzy` flag for the suggest/autocomplete routes: a boolean
/// enabling 1-edit typo-tolerant matching. Absent means exact matching.
pub(crate) fn parse_suggester_fuzzy(value: Option<&Value>) -> Result<bool, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ApiError::invalid_query("fuzzy must be a boolean.")),
    }
}

/// Validates `minimumCoverage` on the suggest/autocomplete routes: it must be
/// a number when present, and is otherwise ignored (prefix/infix matching is
/// not coverage-based; see `docs/known_differences.md`).
pub(crate) fn validate_suggester_minimum_coverage(value: Option<&Value>) -> Result<(), ApiError> {
    match value {
        None | Some(Value::Null | Value::Number(_)) => Ok(()),
        Some(_) => Err(ApiError::invalid_query("minimumCoverage must be a number.")),
    }
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
            let fields = string_items_or(value, "highlight", |part| {
                let field_def = definition.field_path(&part).ok_or_else(|| {
                    ApiError::invalid_query(format!("highlight references unknown field {part:?}."))
                })?;
                if !field_def.searchable {
                    return Err(ApiError::invalid_query(
                        format!(
                            "Field {part:?} is not searchable; only searchable fields can be highlighted."
                        ),
                    ));
                }
                Ok(part)
            })?;
            if fields.is_empty() {
                return Err(ApiError::invalid_query("highlight is empty."));
            }
            fields
        }
    };
    let tag = |key: &str, default: &str| -> Result<String, ApiError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(default.to_owned()),
            Some(Value::String(tag)) => Ok(tag.clone()),
            Some(_) => Err(ApiError::invalid_query(format!("{key} must be a string."))),
        }
    };
    let pre_tag = tag("highlightPreTag", "<em>")?;
    let post_tag = tag("highlightPostTag", "</em>")?;
    Ok((fields, pre_tag, post_tag))
}

/// Search request options that are not implemented and must be rejected with
/// an explicit error.
pub(crate) const UNSUPPORTED_SEARCH_OPTIONS: &[&str] =
    &["scoringProfile", "scoringParameters", "scoringStatistics"];

/// Parses the `minimumCoverage` option: a float in [0.0, 1.0] (default 0.0).
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) for non-numeric values or
/// values outside [0.0, 1.0].
pub(crate) fn parse_minimum_coverage(obj: &Map<String, Value>) -> Result<f64, ApiError> {
    match obj.get("minimumCoverage") {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Number(n)) => {
            let v = n
                .as_f64()
                .ok_or_else(|| ApiError::invalid_query("minimumCoverage must be a number."))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(ApiError::invalid_query(
                    "minimumCoverage must be between 0.0 and 1.0.",
                ));
            }
            Ok(v)
        }
        Some(_) => Err(ApiError::invalid_query("minimumCoverage must be a number.")),
    }
}

/// Parses the `debug` option. The SDK wire format is a string
/// (`QueryDebugMode`: `disabled`, `semantic`, `vector`, `queryRewrites`,
/// `innerHits`, `all`, or pipe-combinations such as `semantic|queryRewrites`);
/// a boolean is also accepted. Debug is enabled for `true` or any string other
/// than `disabled`.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) for values that are neither a
/// boolean nor a string.
pub(crate) fn parse_debug(obj: &Map<String, Value>) -> Result<bool, ApiError> {
    match obj.get("debug") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(Value::String(mode)) => Ok(mode != "disabled"),
        Some(_) => Err(ApiError::invalid_query(
            "debug must be a boolean or a string.",
        )),
    }
}

/// Parses the semantic search options from a search request body. Supports
/// two wire formats:
/// 1. The nested `semantic` object: `{"semantic": {"semanticConfiguration":
///    "...", "answers": {"count": N, "type": "extractive"}, ...}}`.
/// 2. The flat SDK format: `{"queryType": "semantic", "semanticConfiguration":
///    "...", "answers": "extractive|count-N", "captions":
///    "extractive|highlight-true", "semanticErrorHandling": "fail", ...}`.
///    (`semanticQuery`, `semanticMaxWaitInMilliseconds`,
///    `queryAnswerThreshold`, and `queryCaptionHighlightEnabled` are accepted
///    but inert.)
///
/// Returns `None` when neither format is present.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the options are present
/// but malformed.
pub(crate) fn parse_semantic(
    obj: &Map<String, Value>,
    limits: &crate::semantic::SemanticLimits,
) -> Result<Option<crate::semantic::SemanticQuery>, ApiError> {
    // Format 1: nested `semantic` object.
    if let Some(raw) = obj.get("semantic") {
        if !raw.is_null() {
            return crate::semantic::SemanticQuery::from_json(raw, limits);
        }
    }
    // Format 2: flat SDK properties, triggered by a configuration name (or
    // `queryType: "semantic"`, whose missing configuration is reported by
    // validation).
    let config_name = obj
        .get("semanticConfiguration")
        .or_else(|| obj.get("semanticConfigurationName"));
    let Some(config_name) = config_name.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let configuration = config_name
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "The 'semanticConfiguration' property requires a non-empty string.",
            )
        })?;
    let answers = parse_flat_answers(obj, limits);
    let captions = parse_flat_captions(obj);
    let error_handling = crate::semantic::SemanticQuery::parse_error_handling(
        obj.get("semanticErrorHandling")
            .or_else(|| obj.get("semanticErrorMode")),
    )?;
    Ok(Some(crate::semantic::SemanticQuery {
        configuration: configuration.to_owned(),
        questions: Vec::new(),
        answers,
        captions,
        error_handling,
    }))
}

/// Parses the flat `answers` option: the SDK compound string
/// (`"extractive"`, `"extractive|count-N"`, `"extractive|count-N,threshold-T"`,
/// `"none"`), falling back to the `queryAnswer` / `queryAnswerCount`
/// properties. Unknown types are treated as absent.
fn parse_flat_answers(
    obj: &Map<String, Value>,
    limits: &crate::semantic::SemanticLimits,
) -> Option<crate::semantic::SemanticQueryAnswers> {
    if let Some(Value::String(raw)) = obj.get("answers") {
        let mut parts = raw.split('|');
        let kind = parts.next().unwrap_or("").trim();
        if kind != "extractive" {
            return None;
        }
        let mut count = crate::semantic::DEFAULT_ANSWERS_COUNT;
        for option in parts.flat_map(|part| part.split(',')) {
            if let Some(value) = option.trim().strip_prefix("count-") {
                if let Ok(parsed) = value.trim().parse::<usize>() {
                    count = parsed.clamp(1, limits.max_answers);
                }
            }
        }
        return Some(crate::semantic::SemanticQueryAnswers { count });
    }
    if obj
        .get("queryAnswer")
        .and_then(Value::as_str)
        .is_some_and(|v| v == "extractive")
    {
        let count = match obj.get("queryAnswerCount") {
            Some(Value::Number(n)) => n
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .map_or(crate::semantic::DEFAULT_ANSWERS_COUNT, |v| {
                    v.clamp(1, limits.max_answers)
                }),
            _ => crate::semantic::DEFAULT_ANSWERS_COUNT,
        };
        return Some(crate::semantic::SemanticQueryAnswers { count });
    }
    None
}

/// Parses the flat `captions` option: the SDK compound string
/// (`"extractive"`, `"extractive|highlight-true"`, `"none"`), falling back
/// to the `queryCaption` property. The SDK has no caption count, so the
/// default applies. Unknown types are treated as absent.
fn parse_flat_captions(obj: &Map<String, Value>) -> Option<crate::semantic::SemanticQueryCaptions> {
    if let Some(Value::String(raw)) = obj.get("captions") {
        let kind = raw.split('|').next().unwrap_or("").trim();
        if kind != "extractive" {
            return None;
        }
        return Some(crate::semantic::SemanticQueryCaptions {
            count: crate::semantic::DEFAULT_CAPTIONS_COUNT,
            answers: None,
        });
    }
    if obj
        .get("queryCaption")
        .and_then(Value::as_str)
        .is_some_and(|v| v == "extractive")
    {
        return Some(crate::semantic::SemanticQueryCaptions {
            count: crate::semantic::DEFAULT_CAPTIONS_COUNT,
            answers: None,
        });
    }
    None
}
