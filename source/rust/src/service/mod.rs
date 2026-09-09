//! Domain / service layer: index lifecycle, document indexing (upload, merge,
//! merge-or-upload, delete), and search (full-text, filter, ordering,
//! projection, facets, paging with continuation tokens).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{Map, Value};

use crate::error::ApiError;
use crate::filter::{self, FilterExpr};
use crate::query::{parse_search_text, FullTextQuery, QueryError, SearchEngine};
use crate::storage::{Document, IndexDefinition, Storage, StorageError};

/// Field types accepted by the schema validator.
const SUPPORTED_FIELD_TYPES: &[&str] = &[
    "Edm.String",
    "Edm.Int32",
    "Edm.Int64",
    "Edm.Single",
    "Edm.Double",
    "Edm.Boolean",
    "Edm.DateTimeOffset",
    "Edm.Guid",
    "Edm.GeographyPoint",
    "Edm.Collection(Edm.String)",
    "Edm.Collection(Edm.Int32)",
    "Edm.Collection(Edm.Int64)",
    "Edm.Collection(Edm.Single)",
    "Edm.Collection(Edm.Double)",
    "Edm.Collection(Edm.Boolean)",
    "Edm.Collection(Edm.DateTimeOffset)",
    "Edm.Collection(Edm.Guid)",
];

/// Per-document result of an indexing operation, in the response shape
/// expected by the pinned Python SDK (`azure-search-documents==11.6.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexingResultItem {
    pub key: String,
    pub succeeded: bool,
    pub status_code: u16,
    pub error_message: Option<String>,
}

impl IndexingResultItem {
    pub fn to_value(&self) -> Value {
        Value::Object({
            let mut map = Map::new();
            map.insert("key".to_owned(), Value::String(self.key.clone()));
            map.insert("status".to_owned(), Value::Bool(self.succeeded));
            map.insert(
                "statusCode".to_owned(),
                Value::from(u64::from(self.status_code)),
            );
            map.insert(
                "errorMessage".to_owned(),
                self.error_message
                    .clone()
                    .map_or(Value::Null, Value::String),
            );
            map
        })
    }
}

/// A document batch action, parsed from the wire format by the API layer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentAction {
    pub kind: ActionKind,
    /// The document fields (for `delete`, only the key field is meaningful).
    pub document: Value,
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
    pub count: bool,
    pub top: Option<u64>,
    pub skip: u64,
    pub filter: Option<FilterExpr>,
    /// The raw `filter` string, preserved for continuation tokens.
    pub filter_raw: Option<String>,
    pub orderby: Vec<OrderBy>,
    /// The raw `orderby` string, preserved for continuation tokens.
    pub orderby_raw: Option<String>,
    pub select: Vec<String>,
    pub facets: Vec<String>,
    pub search_fields: Vec<String>,
    pub continuation: Option<String>,
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
    /// The `@search.facets` object, when facets were requested.
    pub facets: Option<Value>,
    /// Whether more results exist beyond the returned page.
    pub has_more: bool,
    /// The `skip` value for the next page.
    pub next_skip: u64,
}

/// A continuation token: the opaque `base64(json{filter, orderby, skip,
/// state_version})` value carried in `@odata.nextLink` and returned by the
/// client in the `continuation` request parameter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContinuationToken {
    pub filter: Option<String>,
    pub orderby: Option<String>,
    pub skip: u64,
    pub state_version: u64,
}

impl ContinuationToken {
    #[must_use]
    pub fn encode(&self) -> String {
        // Serialization of this plain-data struct cannot fail; fall back to an
        // empty object (which decodes but carries no paging state) rather than
        // panicking in request handling.
        let json = serde_json::to_string(self).unwrap_or_default();
        BASE64.encode(json)
    }

    /// # Errors
    ///
    /// Returns an error string when the token is not valid base64 JSON with
    /// the expected shape.
    pub fn decode(raw: &str) -> Result<Self, String> {
        let bytes = BASE64
            .decode(raw.as_bytes())
            .map_err(|e| format!("continuation token is not valid base64: {e}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|e| format!("continuation token is not valid UTF-8: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("continuation token is not valid JSON: {e}"))
    }
}

pub struct SearchService {
    storage: Arc<dyn Storage>,
    engine: Arc<SearchEngine>,
    /// Monotonically increasing counter incremented on every document
    /// mutation; embedded in continuation tokens so stale tokens can be
    /// detected.
    state_version: AtomicU64,
}

impl SearchService {
    pub fn new(storage: Arc<dyn Storage>, engine: Arc<SearchEngine>) -> Self {
        Self {
            storage,
            engine,
            state_version: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }

    /// The current document-mutation counter.
    #[must_use]
    pub fn state_version(&self) -> u64 {
        self.state_version.load(Ordering::SeqCst)
    }

    fn bump_state_version(&self) {
        self.state_version.fetch_add(1, Ordering::SeqCst);
    }

    /// Creates an index from a raw Azure `SearchIndex` definition, returning
    /// the echoed definition on success.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is malformed, the schema uses
    /// unsupported field types, or an index with the same name already exists.
    pub fn create_index(&self, raw: &Value) -> Result<Value, ApiError> {
        let definition = parse_index_definition(raw)?;
        validate_schema(&definition)?;
        match self.storage.create_index(&definition) {
            Ok(()) => {
                if let Err(e) = self
                    .engine
                    .create_index(&definition.name, &definition.fields)
                {
                    // Roll back storage: this index did not exist before, so
                    // removing it restores the prior state and avoids
                    // storage/engine divergence.
                    self.storage.delete_index(&definition.name);
                    return Err(engine_error(&definition.name, e));
                }
                Ok(definition.raw.clone())
            }
            Err(StorageError::IndexAlreadyExists(name)) => Err(ApiError::conflict(
                "IndexAlreadyExists",
                format!("An index with name {name:?} already exists."),
            )),
            Err(StorageError::IndexNotFound(name)) => Err(ApiError::not_found(format!(
                "Index {name:?} was not found."
            ))),
        }
    }

    /// Creates or replaces an index from a raw Azure `SearchIndex` definition,
    /// returning the echoed definition. Replacing an index discards its
    /// documents.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is malformed or the schema
    /// uses unsupported field types.
    pub fn create_or_update_index(&self, raw: &Value) -> Result<Value, ApiError> {
        let definition = parse_index_definition(raw)?;
        validate_schema(&definition)?;
        let replaced = self.storage.get_index(&definition.name).is_some();
        self.storage.upsert_index(&definition);
        // Replacing an index discards its documents, so rebuild the search index.
        self.engine.delete_index(&definition.name);
        if let Err(e) = self
            .engine
            .create_index(&definition.name, &definition.fields)
        {
            // Engine rebuild failed; remove the upserted index so storage and
            // engine stay consistent (both absent). The caller can retry.
            self.storage.delete_index(&definition.name);
            return Err(engine_error(&definition.name, e));
        }
        if replaced {
            self.bump_state_version();
        }
        Ok(definition.raw.clone())
    }

    /// Returns the raw stored index definition.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn get_index(&self, name: &str) -> Result<Value, ApiError> {
        Ok(self.require_index(name)?.raw)
    }

    /// Returns the raw stored definitions of all indexes, sorted by name.
    #[must_use]
    pub fn list_indexes(&self) -> Vec<Value> {
        self.storage
            .list_index_names()
            .into_iter()
            .filter_map(|name| self.storage.get_index(&name).map(|def| def.raw))
            .collect()
    }

    /// Deletes an index by name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn delete_index(&self, name: &str) -> Result<(), ApiError> {
        if self.storage.delete_index(name) {
            self.engine.delete_index(name);
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "Index {name:?} was not found."
            )))
        }
    }

    /// Validates and applies a batch of document actions (upload, merge,
    /// merge-or-upload, delete), returning one result per action in request
    /// order. Valid actions in the same batch are applied even when others
    /// fail.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn index_documents(
        &self,
        index: &str,
        actions: Vec<DocumentAction>,
    ) -> Result<Vec<IndexingResultItem>, ApiError> {
        let definition = self.require_index(index)?;
        let key_name = key_field_name(&definition);
        let mut results = Vec::with_capacity(actions.len());
        let mut upserts: Vec<Document> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();

        for action in actions {
            let key = action
                .document
                .get(&key_name)
                .and_then(key_display)
                .unwrap_or_default();
            match action.kind {
                ActionKind::Upload => match validate_document(&definition, &action.document) {
                    Ok(doc) => {
                        upserts.push(doc);
                        results.push(ok_result(key, 201));
                    }
                    Err(message) => results.push(fail_result(key, 400, message)),
                },
                ActionKind::Merge => match self.merge_one(&definition, &key_name, &action.document)
                {
                    MergeOutcome::Applied(doc) => {
                        upserts.push(doc);
                        results.push(ok_result(key, 200));
                    }
                    MergeOutcome::Missing => results.push(fail_result(
                        key.clone(),
                        404,
                        format!("Document with key {key:?} was not found in index {index:?}."),
                    )),
                    MergeOutcome::Invalid(message) => results.push(fail_result(key, 400, message)),
                },
                ActionKind::MergeOrUpload => {
                    match self.merge_one(&definition, &key_name, &action.document) {
                        MergeOutcome::Applied(doc) => {
                            upserts.push(doc);
                            results.push(ok_result(key, 200));
                        }
                        MergeOutcome::Missing => {
                            match validate_document(&definition, &action.document) {
                                Ok(doc) => {
                                    upserts.push(doc);
                                    results.push(ok_result(key, 201));
                                }
                                Err(message) => results.push(fail_result(key, 400, message)),
                            }
                        }
                        MergeOutcome::Invalid(message) => {
                            results.push(fail_result(key, 400, message));
                        }
                    }
                }
                ActionKind::Delete => {
                    if self
                        .storage
                        .get_document(index, &key)
                        .map_err(|e| ApiError::not_found(e.to_string()))?
                        .is_some()
                    {
                        deletes.push(key.clone());
                        results.push(ok_result(key, 200));
                    } else {
                        results.push(fail_result(
                            key.clone(),
                            404,
                            format!("Document with key {key:?} was not found in index {index:?}."),
                        ));
                    }
                }
            }
        }

        if !upserts.is_empty() || !deletes.is_empty() {
            // Apply engine changes first (borrow), then move the documents
            // into storage, so storage is untouched if the engine rejects the
            // batch.
            if let Err(e) = self.apply_engine_changes(index, &upserts, &deletes) {
                return Err(engine_error(index, e));
            }
            if !upserts.is_empty() {
                self.storage
                    .put_documents(index, upserts)
                    .map_err(|e| ApiError::not_found(e.to_string()))?;
            }
            if !deletes.is_empty() {
                self.storage
                    .delete_documents(index, &deletes)
                    .map_err(|e| ApiError::not_found(e.to_string()))?;
            }
            self.bump_state_version();
        }
        Ok(results)
    }

    fn apply_engine_changes(
        &self,
        index: &str,
        upserts: &[Document],
        deletes: &[String],
    ) -> Result<(), QueryError> {
        if upserts.is_empty() && deletes.is_empty() {
            return Ok(());
        }
        // Delete first so a key that is both deleted and re-uploaded in the
        // same batch ends up indexed.
        if !deletes.is_empty() {
            self.engine.delete_documents(index, deletes)?;
        }
        if !upserts.is_empty() {
            self.engine.index_documents(index, upserts)?;
        }
        Ok(())
    }

    fn merge_one(
        &self,
        definition: &IndexDefinition,
        key_name: &str,
        document: &Value,
    ) -> MergeOutcome {
        let Some(key) = document.get(key_name).and_then(key_display) else {
            return MergeOutcome::Invalid(format!(
                "Document is missing the key field {key_name:?}."
            ));
        };
        match self.storage.get_document(&definition.name, &key) {
            Ok(Some(existing)) => match merge_fields(&existing, document) {
                Some(merged) => match validate_document(definition, &merged) {
                    Ok(doc) => MergeOutcome::Applied(doc),
                    Err(message) => MergeOutcome::Invalid(message),
                },
                None => MergeOutcome::Invalid("Merge document must be a JSON object.".to_owned()),
            },
            Ok(None) => MergeOutcome::Missing,
            Err(e) => MergeOutcome::Invalid(e.to_string()),
        }
    }

    /// Parses and validates a search request body against the index schema.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist, the body is not a
    /// JSON object, an option is malformed, or a referenced field is missing
    /// or lacks the required attribute (`filterable`, `sortable`, `facetable`,
    /// `searchable`).
    pub fn parse_search(&self, index: &str, body: &Value) -> Result<SearchQuery, ApiError> {
        let definition = self.require_index(index)?;
        let obj = match body {
            Value::Object(map) => map,
            Value::Null => &Map::new(),
            _ => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    "Search request body must be a JSON object.",
                ))
            }
        };

        for key in UNSUPPORTED_SEARCH_OPTIONS {
            if obj.contains_key(*key) {
                return Err(ApiError::unsupported(
                    "UnsupportedQuery",
                    format!("The {key:?} search option is not supported by the emulator."),
                ));
            }
        }
        if let Some(query_type) = obj.get("queryType").and_then(Value::as_str) {
            if query_type != "simple" {
                return Err(ApiError::unsupported(
                    "UnsupportedQuery",
                    format!("queryType {query_type:?} is not supported by the emulator."),
                ));
            }
        }

        let search = obj.get("search").and_then(Value::as_str).map(str::to_owned);
        if let Some(text) = &search {
            parse_search_text(text).map_err(|e| {
                ApiError::bad_request("InvalidQuery", format!("Invalid search text: {e}"))
            })?;
        }
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

        let filter_raw = obj.get("filter").and_then(Value::as_str).map(str::to_owned);
        let filter = match &filter_raw {
            Some(raw) => Some(parse_filter_option(raw)?),
            None => None,
        };
        if let Some(expr) = &filter {
            filter::validate(expr, &definition).map_err(|e| {
                ApiError::bad_request("InvalidQuery", format!("Invalid filter: {e}"))
            })?;
        }

        let (orderby, orderby_raw) = match obj.get("orderby") {
            Some(value) => {
                let (parsed, raw) = parse_orderby(value, &definition)?;
                (parsed, Some(raw))
            }
            None => (Vec::new(), None),
        };

        let select = match obj.get("select") {
            Some(value) => parse_select(value, &definition)?,
            None => Vec::new(),
        };

        let facets = match obj.get("facets") {
            Some(value) => parse_facets(value, &definition)?,
            None => Vec::new(),
        };

        let search_fields = match obj.get("searchFields") {
            Some(value) => parse_search_fields(value, &definition)?,
            None => Vec::new(),
        };

        let continuation = obj
            .get("continuation")
            .and_then(Value::as_str)
            .map(str::to_owned);

        Ok(SearchQuery {
            search,
            count,
            top,
            skip,
            filter,
            filter_raw,
            orderby,
            orderby_raw,
            select,
            facets,
            search_fields,
            continuation,
        })
    }

    /// Runs a search over an index: full-text match, filter, ordering,
    /// facets, and paging.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist or a continuation
    /// token is invalid or stale.
    pub fn search(&self, index: &str, query: &SearchQuery) -> Result<SearchOutcome, ApiError> {
        let definition = self.require_index(index)?;

        // A continuation token is authoritative for skip/filter/orderby and
        // must reference the current document state.
        let (skip, filter, orderby) = match &query.continuation {
            Some(raw) => {
                let token = ContinuationToken::decode(raw).map_err(|e| {
                    ApiError::bad_request(
                        "InvalidQuery",
                        format!("Invalid continuation token: {e}"),
                    )
                })?;
                if token.state_version != self.state_version() {
                    return Err(ApiError::bad_request(
                        "InvalidQuery",
                        "Stale continuation token: the index changed since the token was issued. \
                         Restart the search.",
                    ));
                }
                let filter = match &query.filter {
                    Some(expr) => Some(expr.clone()),
                    None => match &token.filter {
                        Some(raw) => Some(parse_filter_option(raw)?),
                        None => None,
                    },
                };
                let orderby = if query.orderby.is_empty() {
                    match &token.orderby {
                        Some(raw) => parse_orderby(&Value::String(raw.clone()), &definition)?.0,
                        None => Vec::new(),
                    }
                } else {
                    query.orderby.clone()
                };
                (token.skip, filter, orderby)
            }
            None => (query.skip, query.filter.clone(), query.orderby.clone()),
        };

        let mut full_text = match &query.search {
            Some(text) => parse_search_text(text).map_err(|e| {
                ApiError::bad_request("InvalidQuery", format!("Invalid search text: {e}"))
            })?,
            None => FullTextQuery::default(),
        };
        if !query.search_fields.is_empty() {
            full_text.fields = Some(query.search_fields.clone());
        }

        let matched_keys = self
            .engine
            .search(index, &full_text)
            .map_err(|e| engine_error(index, e))?;
        let documents = self
            .storage
            .get_documents(index)
            .map_err(|e| ApiError::not_found(e.to_string()))?;
        let mut matched: Vec<Document> = documents
            .into_iter()
            .filter(|doc| matched_keys.contains(&doc.key))
            .collect();
        if let Some(expr) = &filter {
            matched.retain(|doc| expr.matches(&doc.fields));
        }
        if !orderby.is_empty() {
            sort_documents(&mut matched, &orderby);
        }

        let total = u64::try_from(matched.len()).unwrap_or(u64::MAX);
        let facets = if query.facets.is_empty() {
            None
        } else {
            Some(compute_facets(&matched, &query.facets))
        };

        let skip_usize = usize::try_from(skip).unwrap_or(usize::MAX);
        let take = query
            .top
            .and_then(|t| usize::try_from(t).ok())
            .unwrap_or(usize::MAX);
        let page: Vec<Document> = matched.into_iter().skip(skip_usize).take(take).collect();
        let has_more =
            u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX) < total;
        let next_skip = u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX);
        Ok(SearchOutcome {
            total,
            documents: page,
            facets,
            has_more,
            next_skip,
        })
    }

    /// Builds the continuation token for the next page, if one exists.
    #[must_use]
    pub fn next_continuation(
        &self,
        query: &SearchQuery,
        outcome: &SearchOutcome,
    ) -> Option<String> {
        if !outcome.has_more {
            return None;
        }
        Some(
            ContinuationToken {
                filter: query.filter_raw.clone(),
                orderby: query.orderby_raw.clone(),
                skip: outcome.next_skip,
                state_version: self.state_version(),
            }
            .encode(),
        )
    }

    pub fn reset(&self) {
        self.storage.reset();
        self.engine.reset();
        self.bump_state_version();
    }

    fn require_index(&self, name: &str) -> Result<IndexDefinition, ApiError> {
        self.storage
            .get_index(name)
            .ok_or_else(|| ApiError::not_found(format!("Index {name:?} was not found.")))
    }
}

/// The outcome of a single merge operation.
enum MergeOutcome {
    Applied(Document),
    Missing,
    Invalid(String),
}

fn ok_result(key: String, status_code: u16) -> IndexingResultItem {
    IndexingResultItem {
        key,
        succeeded: true,
        status_code,
        error_message: None,
    }
}

fn fail_result(key: String, status_code: u16, message: String) -> IndexingResultItem {
    IndexingResultItem {
        key,
        succeeded: false,
        status_code,
        error_message: Some(message),
    }
}

/// Merges `update` into `existing`: scalar fields are overwritten, collection
/// fields are replaced wholesale (Azure merge semantics).
fn merge_fields(existing: &Document, update: &Value) -> Option<Value> {
    let update_obj = update.as_object()?;
    let mut merged = existing.fields.clone();
    for (key, value) in update_obj {
        merged.insert(key.clone(), value.clone());
    }
    Some(Value::Object(merged))
}

/// Sorts documents by the `orderby` clauses, with the key field as the final
/// tie-breaker so ordering is deterministic. Missing values sort last
/// regardless of direction.
fn sort_documents(documents: &mut [Document], orderby: &[OrderBy]) {
    documents.sort_by(|a, b| {
        for clause in orderby {
            let ordering = compare_field(a, b, &clause.field);
            let ordering = if clause.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        a.key.cmp(&b.key)
    });
}

fn compare_field(a: &Document, b: &Document, field: &str) -> std::cmp::Ordering {
    let (av, bv) = (a.fields.get(field), b.fields.get(field));
    match (av, bv) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(av), Some(bv)) => compare_values(av, bv),
    }
}

fn compare_values(av: &Value, bv: &Value) -> std::cmp::Ordering {
    match (av, bv) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        // Mixed types: order by type tag so the result is deterministic.
        _ => type_tag(av).cmp(&type_tag(bv)),
    }
}

fn type_tag(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// Computes facet counts over the full (filtered, ordered) result set: one
/// entry per distinct value, ordered by count descending then value ascending.
fn compute_facets(documents: &[Document], fields: &[String]) -> Value {
    let mut map = Map::new();
    for field in fields {
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for document in documents {
            let Some(value) = document.fields.get(field) else {
                continue;
            };
            let values = if value.is_array() {
                value.as_array().cloned().unwrap_or_default()
            } else {
                vec![value.clone()]
            };
            for value in values {
                if value.is_null() {
                    continue;
                }
                let key = facet_key(&value);
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        let mut entries: Vec<(String, u64)> = counts.into_iter().collect();
        entries.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        let items = entries
            .into_iter()
            .map(|(key, count)| {
                Value::Object({
                    let mut entry = Map::new();
                    entry.insert("value".to_owned(), facet_value(&key));
                    entry.insert("count".to_owned(), Value::from(count));
                    entry
                })
            })
            .collect();
        map.insert(field.clone(), Value::Array(items));
    }
    Value::Object(map)
}

fn facet_key(value: &Value) -> String {
    match value {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        other => format!("o:{other}"),
    }
}

fn facet_value(key: &str) -> Value {
    match key.split_once(':') {
        Some(("s", rest)) => Value::String(rest.to_owned()),
        Some(("n", rest)) => rest
            .parse::<i64>()
            .ok()
            .map(Value::from)
            .or_else(|| rest.parse::<f64>().ok().map(Value::from))
            .unwrap_or_else(|| Value::String(rest.to_owned())),
        Some(("b", rest)) => Value::Bool(rest == "true"),
        _ => Value::String(key.to_owned()),
    }
}

fn parse_filter_option(raw: &str) -> Result<FilterExpr, ApiError> {
    filter::parse_filter(raw)
        .map_err(|e| ApiError::bad_request("InvalidQuery", format!("Invalid filter: {e}")))
}

/// Extracts a list of comma-separated items from a search option value. The
/// Azure REST API documents these as comma-separated strings, but SDKs may
/// send JSON arrays; both shapes are accepted.
fn string_items(value: &Value, name: &str) -> Result<Vec<String>, ApiError> {
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
/// a JSON array of clauses). Every field must exist and be marked `sortable`.
/// Returns the parsed clauses and a canonical raw string for continuation
/// tokens.
fn parse_orderby(
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
/// each of which must exist in the schema.
fn parse_select(value: &Value, definition: &IndexDefinition) -> Result<Vec<String>, ApiError> {
    let mut fields = Vec::new();
    for part in string_items(value, "select")? {
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

/// Parses a `facets` value: comma-separated field names or `*` (all facetable
/// fields), or a JSON array. Named fields must exist and be marked `facetable`.
fn parse_facets(value: &Value, definition: &IndexDefinition) -> Result<Vec<String>, ApiError> {
    let mut fields = Vec::new();
    for part in string_items(value, "facets")? {
        if part == "*" {
            for field in &definition.fields {
                if field.facetable {
                    fields.push(field.name.clone());
                }
            }
        } else {
            let field_def = definition.field(&part).ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    format!("facets references unknown field {part:?}."),
                )
            })?;
            if !field_def.facetable {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Field {part:?} is not facetable; mark it \"facetable\": true in the index schema."
                    ),
                ));
            }
            fields.push(part);
        }
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "facets is empty."));
    }
    Ok(fields)
}

/// Parses a `searchFields` value: comma-separated field names (or a JSON
/// array), optionally weighted (`field^2`). Fields must exist and be marked
/// `searchable`. Weights are accepted but inert (scoring is a constant
/// placeholder).
fn parse_search_fields(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<String>, ApiError> {
    let mut fields = Vec::new();
    for part in string_items(value, "searchFields")? {
        let name = part.split('^').next().unwrap_or("").trim();
        let field_def = definition.field(name).ok_or_else(|| {
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
        fields.push(name.to_owned());
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            "searchFields is empty.",
        ));
    }
    Ok(fields)
}

fn key_field_name(definition: &IndexDefinition) -> String {
    definition
        .key_field()
        .map(|f| f.name.clone())
        .unwrap_or_default()
}

fn key_display(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Maps a query-engine failure onto an Azure-compatible [`ApiError`].
fn engine_error(index: &str, error: QueryError) -> ApiError {
    match error {
        QueryError::IndexNotFound(name) => {
            ApiError::not_found(format!("Index {name:?} was not found."))
        }
        QueryError::Engine(message) => {
            ApiError::internal(format!("Search failed for index {index:?}: {message}"))
        }
    }
}

fn parse_index_definition(raw: &Value) -> Result<IndexDefinition, ApiError> {
    IndexDefinition::from_json(raw.clone())
        .map_err(|message| ApiError::bad_request("InvalidIndex", message))
}

fn validate_schema(definition: &IndexDefinition) -> Result<(), ApiError> {
    let mut key_count = 0;
    let mut seen = std::collections::BTreeSet::new();
    for field in &definition.fields {
        if !seen.insert(field.name.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!("Duplicate field name {:?} in index schema.", field.name),
            ));
        }
        if field.is_key {
            key_count += 1;
        }
        if !SUPPORTED_FIELD_TYPES.contains(&field.field_type.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Unsupported field type {:?} for field {:?}. Supported types: {}.",
                    field.field_type,
                    field.name,
                    SUPPORTED_FIELD_TYPES.join(", ")
                ),
            ));
        }
    }
    if key_count != 1 {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!("Index schema must define exactly one key field; found {key_count}."),
        ));
    }
    Ok(())
}

fn validate_document(definition: &IndexDefinition, document: &Value) -> Result<Document, String> {
    let obj = document
        .as_object()
        .ok_or_else(|| "Document must be a JSON object.".to_owned())?;
    let key_name = definition
        .key_field()
        .map(|f| f.name.clone())
        .ok_or_else(|| "Index has no key field.".to_owned())?;
    let key_value = obj
        .get(&key_name)
        .ok_or_else(|| format!("Document is missing the key field {key_name:?}."))?;
    let key = match key_value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => {
            return Err(format!(
                "Key field {key_name:?} must be a string or number, got {other:?}."
            ))
        }
    };
    for (name, value) in obj {
        let field = definition
            .field(name)
            .ok_or_else(|| format!("Document contains unknown field {name:?}."))?;
        check_field_type(name, &field.field_type, value)?;
    }
    Ok(Document {
        key,
        fields: obj.clone(),
    })
}

fn check_field_type(name: &str, field_type: &str, value: &Value) -> Result<(), String> {
    let ok = if let Some(inner) = field_type
        .strip_prefix("Edm.Collection(")
        .and_then(|s| s.strip_suffix(')'))
    {
        value
            .as_array()
            .is_some_and(|items| items.iter().all(|item| type_ok(inner, item)))
    } else {
        match field_type {
            "Edm.String" | "Edm.DateTimeOffset" | "Edm.Guid" | "Edm.GeographyPoint" => {
                value.is_string()
            }
            "Edm.Int32" | "Edm.Int64" => value.is_i64() || value.is_u64(),
            "Edm.Single" | "Edm.Double" => value.is_number(),
            "Edm.Boolean" => value.is_boolean(),
            _ => true,
        }
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "Value for field {name:?} is not compatible with type {field_type:?}."
        ))
    }
}

fn type_ok(inner: &str, value: &Value) -> bool {
    match inner {
        "Edm.String" | "Edm.DateTimeOffset" | "Edm.Guid" => value.is_string(),
        "Edm.Int32" | "Edm.Int64" => value.is_i64() || value.is_u64(),
        "Edm.Single" | "Edm.Double" => value.is_number(),
        "Edm.Boolean" => value.is_boolean(),
        _ => true,
    }
}

/// Search request options that are not implemented and must be rejected with
/// an explicit error.
const UNSUPPORTED_SEARCH_OPTIONS: &[&str] = &[
    "highlight",
    "highlightPreTag",
    "highlightPostTag",
    "scoringProfile",
    "scoringParameters",
    "scoringStatistics",
    "sessionId",
    "minimumCoverage",
    "answers",
    "captions",
    "semanticConfiguration",
    "semanticQuery",
    "semanticErrorHandling",
    "semanticMaxWaitInMilliseconds",
    "vectorQueries",
    "vectorFilterMode",
    "debug",
    "searchMode",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use serde_json::json;

    fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(err) => panic!("expected Ok, got Err: {err:?}"),
        }
    }

    fn err<T, E: std::fmt::Debug>(result: Result<T, E>) -> E {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(err) => err,
        }
    }

    fn service() -> SearchService {
        SearchService::new(
            Arc::new(InMemoryStorage::new()),
            Arc::new(SearchEngine::new()),
        )
    }

    fn index_body() -> Value {
        json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
                {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true},
                {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true, "facetable": true}
            ]
        })
    }

    fn upload(service: &SearchService, docs: Vec<Value>) {
        let actions = docs
            .into_iter()
            .map(|document| DocumentAction {
                kind: ActionKind::Upload,
                document,
            })
            .collect();
        let results = ok(service.index_documents("items", actions));
        assert!(
            results.iter().all(|r| r.succeeded),
            "upload failed: {results:?}"
        );
    }

    #[test]
    fn create_and_delete_index() {
        let service = service();
        let created = ok(service.create_index(&index_body()));
        assert_eq!(created["name"], "items");
        assert!(service.delete_index("items").is_ok());
        assert!(service.delete_index("items").is_err());
    }

    #[test]
    fn duplicate_index_conflicts() {
        let service = service();
        ok(service.create_index(&index_body()));
        let api_error = err(service.create_index(&index_body()));
        assert_eq!(api_error.status, axum::http::StatusCode::CONFLICT);
    }

    #[test]
    fn schema_validation_rejects_bad_types_and_key_counts() {
        let service = service();
        let no_key = json!({
            "name": "x",
            "fields": [{"name": "id", "type": "Edm.String"}]
        });
        assert!(service.create_index(&no_key).is_err());
        let bad_type = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "v", "type": "Edm.Vector(Edm.Single, 3)"}
            ]
        });
        assert!(service.create_index(&bad_type).is_err());
    }

    #[test]
    fn upload_validates_documents_against_schema() {
        let service = service();
        ok(service.create_index(&index_body()));
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "1", "title": "one", "price": 1.5}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"title": "missing key"}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "3", "unknown": true}),
                },
            ],
        ));
        assert_eq!(results.len(), 3);
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 201);
        assert!(!results[1].succeeded);
        assert_eq!(results[1].status_code, 400);
        assert!(!results[2].succeeded);
    }

    #[test]
    fn merge_updates_existing_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.0, "tags": ["a"]})],
        );
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "1", "price": 2.0}),
            }],
        ));
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 200);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs[0].fields["title"], "one");
        assert_eq!(docs[0].fields["price"], 2.0);
        // Collections are replaced, not appended.
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "1", "tags": ["b"]}),
            }],
        ));
        assert!(results[0].succeeded);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs[0].fields["tags"], json!(["b"]));
    }

    #[test]
    fn merge_missing_document_fails_per_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "missing", "price": 2.0}),
            }],
        ));
        assert!(!results[0].succeeded);
        assert_eq!(results[0].status_code, 404);
    }

    #[test]
    fn merge_or_upload_merges_or_uploads() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.0})],
        );
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::MergeOrUpload,
                    document: json!({"id": "1", "price": 9.0}),
                },
                DocumentAction {
                    kind: ActionKind::MergeOrUpload,
                    document: json!({"id": "2", "title": "two", "price": 3.0}),
                },
            ],
        ));
        assert_eq!(results[0].status_code, 200);
        assert_eq!(results[1].status_code, 201);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].fields["title"], "one");
        assert_eq!(docs[0].fields["price"], 9.0);
    }

    #[test]
    fn delete_removes_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "one"}),
                json!({"id": "2", "title": "two"}),
            ],
        );
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "1"}),
                },
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "missing"}),
                },
            ],
        ));
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 200);
        assert!(!results[1].succeeded);
        assert_eq!(results[1].status_code, 404);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].key, "2");
        // Deleted documents are no longer searchable.
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 1);
    }

    #[test]
    fn upsert_index_rebuilds_search_state() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "hello", "price": 1.0})],
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("hello".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 1);

        // Replacing the index discards its documents from both storage and
        // the search engine.
        let replacement = json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "other", "type": "Edm.String", "searchable": true}
            ]
        });
        ok(service.create_or_update_index(&replacement));
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("hello".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 0);
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 0);
    }

    #[test]
    fn search_matches_and_orders_by_key() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "b", "title": "azure search"}),
                json!({"id": "a", "title": "azure emulators"}),
                json!({"id": "c", "title": "other"}),
            ],
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("azure".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 2);
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn search_count_and_paging() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            (0..5)
                .map(|i| json!({"id": i.to_string(), "title": "same"}))
                .collect(),
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                count: true,
                top: Some(2),
                skip: 1,
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 5);
        assert_eq!(outcome.documents.len(), 2);
        assert_eq!(outcome.documents[0].key, "1");
        assert!(outcome.has_more);
        assert_eq!(outcome.next_skip, 3);
    }

    #[test]
    fn search_filter_narrows_results() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "cheap", "price": 5.0}),
                json!({"id": "2", "title": "mid", "price": 50.0}),
                json!({"id": "3", "title": "expensive", "price": 500.0}),
            ],
        );
        let query =
            ok(service.parse_search("items", &json!({"search": "*", "filter": "price ge 50"})));
        let outcome = ok(service.search("items", &query));
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["2", "3"]);
    }

    #[test]
    fn search_orderby_sorts_results() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "price": 30.0}),
                json!({"id": "2", "title": "b", "price": 10.0}),
                json!({"id": "3", "title": "c", "price": 20.0}),
            ],
        );
        let query =
            ok(service.parse_search("items", &json!({"search": "*", "orderby": "price desc"})));
        let outcome = ok(service.search("items", &query));
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["1", "3", "2"]);
    }

    #[test]
    fn search_facets_count_values() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "tags": ["red", "blue"]}),
                json!({"id": "2", "title": "b", "tags": ["red"]}),
                json!({"id": "3", "title": "c", "tags": ["green"]}),
            ],
        );
        let query = ok(service.parse_search("items", &json!({"search": "*", "facets": "tags"})));
        let outcome = ok(service.search("items", &query));
        let Some(facets) = outcome.facets else {
            panic!("expected facets in search outcome");
        };
        let Some(entries) = facets["tags"].as_array() else {
            panic!("expected tags facet array");
        };
        assert_eq!(entries[0]["value"], "red");
        assert_eq!(entries[0]["count"], 2);
        assert_eq!(entries[1]["value"], "blue");
        assert_eq!(entries[1]["count"], 1);
        assert_eq!(entries[2]["value"], "green");
        assert_eq!(entries[2]["count"], 1);
    }

    #[test]
    fn parse_search_rejects_bad_options() {
        let service = service();
        ok(service.create_index(&index_body()));
        // Unknown filter field.
        let api_error = err(service.parse_search("items", &json!({"filter": "missing eq 1"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-sortable field in orderby.
        let api_error = err(service.parse_search("items", &json!({"orderby": "title"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Unknown field in select.
        let api_error = err(service.parse_search("items", &json!({"select": "nope"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-facetable field in facets.
        let api_error = err(service.parse_search("items", &json!({"facets": "price"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-searchable field in searchFields.
        let api_error = err(service.parse_search("items", &json!({"searchFields": "price"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Unsupported option.
        let api_error = err(service.parse_search("items", &json!({"highlight": "title"})));
        assert_eq!(api_error.code, "UnsupportedQuery");
        // Bad filter syntax.
        let api_error = err(service.parse_search("items", &json!({"filter": "price eq"})));
        assert_eq!(api_error.code, "InvalidQuery");
    }

    #[test]
    fn continuation_token_round_trips_and_detects_staleness() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            (0..5)
                .map(|i| json!({"id": i.to_string(), "title": "same"}))
                .collect(),
        );
        let query = ok(service.parse_search(
            "items",
            &json!({"search": "*", "top": 2, "filter": "title eq 'same'"}),
        ));
        let outcome = ok(service.search("items", &query));
        let token = service
            .next_continuation(&query, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));
        // The token decodes and carries the next skip.
        let decoded =
            ContinuationToken::decode(&token).unwrap_or_else(|e| panic!("token decodes: {e}"));
        assert_eq!(decoded.skip, 2);
        assert_eq!(decoded.state_version, service.state_version());
        assert_eq!(decoded.filter.as_deref(), Some("title eq 'same'"));

        // A mutation invalidates the token.
        upload(&service, vec![json!({"id": "5", "title": "new"})]);
        let stale = SearchQuery {
            continuation: Some(token),
            ..query.clone()
        };
        let api_error = err(service.search("items", &stale));
        assert_eq!(api_error.code, "InvalidQuery");
        assert!(api_error.message.contains("Stale"));

        // A fresh token works and resumes after the first page.
        let outcome = ok(service.search("items", &query));
        let token = service
            .next_continuation(&query, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));
        let next = SearchQuery {
            continuation: Some(token),
            ..query.clone()
        };
        let outcome = ok(service.search("items", &next));
        assert_eq!(outcome.documents.first().map(|d| d.key.as_str()), Some("2"));
    }

    #[test]
    fn search_unknown_index_not_found() {
        let service = service();
        let api_error = err(service.parse_search("missing", &Value::Null));
        assert_eq!(api_error.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn concurrent_writes_and_searches_do_not_corrupt_state() {
        use std::sync::Barrier;

        let service = Arc::new(service());
        ok(service.create_index(&index_body()));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for worker in 0..4 {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..25 {
                    let key = format!("w{worker}-{i}");
                    let actions = vec![DocumentAction {
                        kind: ActionKind::Upload,
                        document: json!({"id": key, "title": "concurrent", "price": 1.0}),
                    }];
                    ok(service.index_documents("items", actions));
                }
            }));
        }
        for _ in 0..4 {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..25 {
                    let outcome = ok(service.search(
                        "items",
                        &SearchQuery {
                            search: Some("concurrent".to_owned()),
                            ..Default::default()
                        },
                    ));
                    // Every returned document matches; ordering is by key.
                    let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
                    let mut sorted = keys.clone();
                    sorted.sort();
                    assert_eq!(keys, sorted);
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap_or_else(|e| panic!("worker panicked: {e:?}"));
        }
        // All 100 documents landed exactly once.
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 100);
    }
}
