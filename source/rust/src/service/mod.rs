//! Domain / service layer: index lifecycle, document upload, and search.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::ApiError;
use crate::query::{QueryError, SearchEngine};
use crate::storage::{Document, IndexDefinition, Storage, StorageError};

/// Field types accepted by the Phase 1 schema validator.
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

/// A parsed search request (Phase 1 subset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    pub search: Option<String>,
    pub count: bool,
    pub top: Option<u64>,
    pub skip: u64,
}

/// The result of a search operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub total: u64,
    pub documents: Vec<Document>,
}

pub struct SearchService {
    storage: Arc<dyn Storage>,
    engine: Arc<SearchEngine>,
}

impl SearchService {
    pub fn new(storage: Arc<dyn Storage>, engine: Arc<SearchEngine>) -> Self {
        Self { storage, engine }
    }

    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
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

    /// Validates and stores a batch of documents, returning one result per
    /// document in request order.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn upload_documents(
        &self,
        index: &str,
        documents: Vec<Value>,
    ) -> Result<Vec<IndexingResultItem>, ApiError> {
        let definition = self.require_index(index)?;
        let key_name = key_field_name(&definition);
        let mut accepted = Vec::new();
        let mut results = Vec::with_capacity(documents.len());
        for document in documents {
            match validate_document(&definition, &document) {
                Ok(doc) => {
                    let key = doc.key.clone();
                    accepted.push(doc);
                    results.push(IndexingResultItem {
                        key,
                        succeeded: true,
                        status_code: 201,
                        error_message: None,
                    });
                }
                Err(message) => {
                    let key = document
                        .get(&key_name)
                        .and_then(key_display)
                        .unwrap_or_default();
                    results.push(IndexingResultItem {
                        key,
                        succeeded: false,
                        status_code: 400,
                        error_message: Some(message),
                    });
                }
            }
        }
        if !accepted.is_empty() {
            // Index the engine first (borrow), then move the documents into
            // storage. Either order self-heals on the next upload (engine
            // upserts by key; search resolves keys against storage), but this
            // avoids cloning the batch and leaves storage untouched if the
            // engine rejects the documents.
            self.engine
                .index_documents(index, &accepted)
                .map_err(|e| engine_error(index, e))?;
            self.storage
                .put_documents(index, accepted)
                .map_err(|e| ApiError::not_found(e.to_string()))?;
        }
        Ok(results)
    }

    /// Runs a Phase 1 search over an index, applying substring matching and
    /// `skip`/`top` paging.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn search(&self, index: &str, query: &SearchQuery) -> Result<SearchOutcome, ApiError> {
        self.require_index(index)?;
        let term = query.search.as_deref().unwrap_or("");
        let matched_keys = self
            .engine
            .search(index, term)
            .map_err(|e| engine_error(index, e))?;
        let documents = self
            .storage
            .get_documents(index)
            .map_err(|e| ApiError::not_found(e.to_string()))?;
        let matched: Vec<Document> = documents
            .into_iter()
            .filter(|doc| matched_keys.contains(&doc.key))
            .collect();
        let total = u64::try_from(matched.len()).unwrap_or(u64::MAX);
        let skip = usize::try_from(query.skip).unwrap_or(usize::MAX);
        let take = query
            .top
            .and_then(|t| usize::try_from(t).ok())
            .unwrap_or(usize::MAX);
        let page: Vec<Document> = matched.into_iter().skip(skip).take(take).collect();
        Ok(SearchOutcome {
            total,
            documents: page,
        })
    }

    pub fn reset(&self) {
        self.storage.reset();
        self.engine.reset();
    }

    fn require_index(&self, name: &str) -> Result<IndexDefinition, ApiError> {
        self.storage
            .get_index(name)
            .ok_or_else(|| ApiError::not_found(format!("Index {name:?} was not found.")))
    }
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

/// Search request options that are not implemented in Phase 1 and must be
/// rejected with an explicit error.
const UNSUPPORTED_SEARCH_OPTIONS: &[&str] = &[
    "filter",
    "facets",
    "orderby",
    "searchFields",
    "searchMode",
    "select",
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
];

/// Parses a search request body, rejecting query features that are not
/// implemented in Phase 1 with an explicit error.
///
/// # Errors
///
/// Returns an [`ApiError`] if the body is not a JSON object, an unsupported
/// option is present, or `top` is not a non-negative integer.
pub fn parse_search_request(body: &Value) -> Result<SearchQuery, ApiError> {
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
                format!("The {key:?} search option is not supported by the emulator (Phase 1)."),
            ));
        }
    }
    if let Some(query_type) = obj.get("queryType").and_then(Value::as_str) {
        if query_type != "simple" {
            return Err(ApiError::unsupported(
                "UnsupportedQuery",
                format!("queryType {query_type:?} is not supported by the emulator (Phase 1)."),
            ));
        }
    }

    let search = obj.get("search").and_then(Value::as_str).map(str::to_owned);
    let count = obj.get("count").and_then(Value::as_bool).unwrap_or(false);
    let top = match obj.get("top") {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"top\" must be a non-negative integer.")
        })?),
    };
    let skip = obj.get("skip").and_then(Value::as_u64).unwrap_or(0);

    Ok(SearchQuery {
        search,
        count,
        top,
        skip,
    })
}

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
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true},
                {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true}
            ]
        })
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
        let results = ok(service.upload_documents(
            "items",
            vec![
                json!({"id": "1", "title": "one", "price": 1.5}),
                json!({"title": "missing key"}),
                json!({"id": "3", "unknown": true}),
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
    fn upsert_index_rebuilds_search_state() {
        let service = service();
        ok(service.create_index(&index_body()));
        ok(service.upload_documents(
            "items",
            vec![json!({"id": "1", "title": "hello", "price": 1.0})],
        ));
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
        ok(service.upload_documents(
            "items",
            vec![
                json!({"id": "b", "title": "azure search"}),
                json!({"id": "a", "title": "azure emulators"}),
                json!({"id": "c", "title": "other"}),
            ],
        ));
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
        ok(service.upload_documents(
            "items",
            (0..5)
                .map(|i| json!({"id": i.to_string(), "title": "same"}))
                .collect(),
        ));
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                count: true,
                top: Some(2),
                skip: 1,
            },
        ));
        assert_eq!(outcome.total, 5);
        assert_eq!(outcome.documents.len(), 2);
        assert_eq!(outcome.documents[0].key, "1");
    }

    #[test]
    fn search_unknown_index_not_found() {
        let service = service();
        let api_error = err(service.search("missing", &SearchQuery::default()));
        assert_eq!(api_error.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn parse_search_request_rejects_unsupported_options() {
        let api_error = err(parse_search_request(
            &json!({"search": "x", "filter": "id eq '1'"}),
        ));
        assert_eq!(api_error.code, "UnsupportedQuery");
        let query = ok(parse_search_request(&json!({"search": "x", "count": true})));
        assert!(query.count);
        let api_error = err(parse_search_request(&json!({"queryType": "full"})));
        assert_eq!(api_error.code, "UnsupportedQuery");
    }
}
