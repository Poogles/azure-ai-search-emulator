//! HTTP / Azure compatibility layer.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::error::ApiError;
use crate::query::SearchEngine;
use crate::service::{ActionKind, DocumentAction, SearchOutcome, SearchService};
use crate::version::VersionAdapter;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<SearchService>,
    pub config: Config,
    pub versions: VersionAdapter,
}

impl AppState {
    #[must_use]
    pub fn new(config: Config, storage: Arc<dyn crate::storage::Storage>) -> Self {
        let engine = Arc::new(SearchEngine::new());
        let versions = VersionAdapter::new(config.api_versions.clone());
        Self {
            service: Arc::new(SearchService::new(storage, engine)),
            config,
            versions,
        }
    }
}

/// Builds the application router.
pub fn build_router(state: AppState) -> Router {
    // The Azure path uses a single segment of the form `indexes('name')`, so the
    // whole segment is captured and parsed in the handlers. The literal
    // `/indexes` route (create / list) takes precedence over `/{index}`.
    let azure = Router::new()
        .route("/indexes", post(create_index).get(list_indexes))
        .route(
            "/{index}",
            get(get_index)
                .put(create_or_update_index)
                .delete(delete_index),
        )
        .route("/{index}/docs/search.index", post(upload_documents))
        .route("/{index}/docs/search.post.search", post(search_documents))
        .layer(middleware::from_fn_with_state(state.clone(), azure_guard));

    let mut app = Router::new().route("/health", get(health)).merge(azure);

    if state.config.enable_admin {
        app = app.route("/admin/reset", post(admin_reset));
    }

    app.layer(middleware::from_fn(request_logging))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Operational endpoints (not part of the Azure surface)
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn admin_reset(State(state): State<AppState>) -> Json<Value> {
    state.service.reset();
    Json(json!({ "status": "reset" }))
}

// ---------------------------------------------------------------------------
// Azure-compatible endpoints
// ---------------------------------------------------------------------------

/// `POST /indexes` — Create Index. The name comes from the body.
async fn create_index(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let definition = parse_body(&body)?;
    let echoed = state.service.create_index(&definition)?;
    Ok((StatusCode::CREATED, Json(echoed)))
}

/// `PUT /indexes('{name}')` — Create or Update Index. Replaces any existing
/// index (and its documents).
async fn create_or_update_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let name = parse_index_name(&raw_name)?;
    let definition = parse_body(&body)?;
    let body_name = definition.get("name").and_then(Value::as_str).unwrap_or("");
    if name != body_name {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!("Index name in path ({name:?}) does not match index name in body."),
        ));
    }
    let echoed = state.service.create_or_update_index(&definition)?;
    Ok((StatusCode::CREATED, Json(echoed)))
}

/// `GET /indexes` — List Indexes.
async fn list_indexes(State(state): State<AppState>) -> Json<Value> {
    let value = state.service.list_indexes();
    Json(json!({ "value": value }))
}

async fn get_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    Ok(Json(state.service.get_index(&name)?))
}

async fn delete_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = parse_index_name(&raw_name)?;
    state.service.delete_index(&name)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn upload_documents(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    let batch = parse_body(&body)?;
    // The SDK serializes an `IndexBatch` as `{"value": [...]}`; a bare array is
    // also accepted for direct HTTP use.
    let actions = match batch {
        Value::Array(items) => items,
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidDocuments",
                    "Document batch must be a JSON array of actions or an object with a \"value\" array.",
                )
            })?,
        _ => {
            return Err(ApiError::bad_request(
                "InvalidDocuments",
                "Document batch must be a JSON array of actions or an object with a \"value\" array.",
            ))
        }
    };
    let mut batch = Vec::with_capacity(actions.len());
    for action in &actions {
        let action_type = action
            .get("@search.action")
            .and_then(Value::as_str)
            .unwrap_or("upload");
        let kind = match action_type {
            "upload" => ActionKind::Upload,
            "merge" => ActionKind::Merge,
            "mergeOrUpload" => ActionKind::MergeOrUpload,
            "delete" => ActionKind::Delete,
            other => {
                return Err(ApiError::unsupported(
                    "UnsupportedAction",
                    format!("Document action {other:?} is not supported by the emulator."),
                ))
            }
        };
        // Two wire shapes are accepted:
        //   - Documented Azure format: {"@search.action": "...", "document": {...}}
        //   - Python SDK format:       {"@search.action": "...", ...fields}
        //     (the SDK spreads the document fields at the top level of the action)
        let doc = match action.get("document") {
            Some(document) => document.clone(),
            None => Value::Object(
                action
                    .as_object()
                    .ok_or_else(|| {
                        ApiError::bad_request(
                            "InvalidDocuments",
                            "Each batch action must be a JSON object.",
                        )
                    })?
                    .iter()
                    .filter(|(key, _)| key.as_str() != "@search.action")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
        };
        batch.push(DocumentAction {
            kind,
            document: doc,
        });
    }
    let results = state.service.index_documents(&name, batch)?;
    let value = Value::Array(
        results
            .iter()
            .map(crate::service::IndexingResultItem::to_value)
            .collect(),
    );
    Ok(Json(json!({ "value": value })))
}

async fn search_documents(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    let raw = if body.is_empty() {
        Value::Null
    } else {
        parse_body(&body)?
    };
    let api_version = uri
        .query()
        .and_then(|q| query_param(q, "api-version"))
        .map(str::to_owned);
    let query = state.service.parse_search(&name, &raw)?;
    let outcome = state.service.search(&name, &query)?;
    let continuation = state.service.next_continuation(&query, &outcome);
    Ok(Json(search_response(
        &name,
        api_version.as_ref(),
        &raw,
        &query,
        &outcome,
        continuation.as_deref(),
    )))
}

fn search_response(
    name: &str,
    api_version: Option<&String>,
    raw_request: &Value,
    query: &crate::service::SearchQuery,
    outcome: &SearchOutcome,
    continuation: Option<&str>,
) -> Value {
    let mut map = Map::new();
    map.insert(
        "@odata.context".to_owned(),
        Value::String("/$metadata#documents".to_owned()),
    );
    if query.count {
        map.insert("@odata.count".to_owned(), Value::from(outcome.total));
    }
    let facets_value = match &outcome.facets {
        Some(facets) => facets.clone(),
        None => Value::Null,
    };
    map.insert("@search.facets".to_owned(), facets_value);
    let value = outcome
        .documents
        .iter()
        .map(|doc| {
            let mut entry = Map::new();
            entry.insert("@search.score".to_owned(), json!(1.0));
            for (key, value) in &doc.fields {
                if query.select.is_empty() || query.select.iter().any(|s| s == key) {
                    entry.insert(key.clone(), value.clone());
                }
            }
            Value::Object(entry)
        })
        .collect();
    map.insert("value".to_owned(), Value::Array(value));
    if let Some(token) = continuation {
        let mut next_link = format!("/indexes('{name}')/docs/search.post.search");
        let mut params = Vec::new();
        if let Some(version) = api_version {
            params.push(format!("api-version={version}"));
        }
        params.push(format!("continuation={token}"));
        next_link.push('?');
        next_link.push_str(&params.join("&"));
        map.insert("@odata.nextLink".to_owned(), Value::String(next_link));
        // The pinned Python SDK pages by re-POSTing `@search.nextPageParameters`
        // (a serialized search request) rather than following nextLink, so the
        // next request is the original body plus the continuation token. The
        // SDK drops unknown properties on re-serialization, so the paging
        // cursor is also carried in the first-class `skip` property (which
        // survives the round-trip); `continuation` additionally enables stale
        // token detection for clients that preserve it.
        let mut next_params = match raw_request {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        };
        next_params.insert("continuation".to_owned(), Value::String(token.to_owned()));
        next_params.insert("skip".to_owned(), Value::from(outcome.next_skip));
        map.insert(
            "@search.nextPageParameters".to_owned(),
            Value::Object(next_params),
        );
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Authentication and API-version guard for the Azure-compatible surface.
///
/// - Any non-empty `api-key` header is accepted; missing/empty returns 401.
/// - The `api-version` query parameter is required and must be supported.
async fn azure_guard(
    State(state): State<AppState>,
    request: Request,
    next: middleware::Next,
) -> Result<Response, ApiError> {
    // Only enforce Azure auth/version on the index operation surface
    // (`/indexes` and `/indexes('name')...`); let everything else fall through
    // to routing.
    let path = request.uri().path();
    if path != "/indexes" && !path.starts_with("/indexes(") {
        return Ok(next.run(request).await);
    }
    let api_key = request
        .headers()
        .get("api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    if api_key.is_empty() {
        return Err(ApiError::authentication_failed(
            "An API key is required. Provide a non-empty 'api-key' header.",
        ));
    }
    let api_version = request
        .uri()
        .query()
        .and_then(|q| query_param(q, "api-version"));
    state.versions.check(api_version)?;
    Ok(next.run(request).await)
}

/// Structured request logging: method, endpoint, API version, index,
/// operation, and `x-ms-client-request-id`. Request bodies are never logged.
async fn request_logging(request: Request, next: middleware::Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let api_version = request
        .uri()
        .query()
        .and_then(|q| query_param(q, "api-version"))
        .map(str::to_owned);
    let index = extract_index(&path);
    let operation = operation_for(&method, &path);
    let client_request_id = request
        .headers()
        .get("x-ms-client-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let response = next.run(request).await;
    let status = response.status();

    tracing::info!(
        method = %method,
        path = %path,
        api_version = api_version.as_deref(),
        index = index.as_deref(),
        operation = %operation,
        client_request_id = client_request_id.as_deref(),
        status = %status.as_u16(),
        "request"
    );
    response
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parses the OData-style index path segment `indexes('name')`.
fn parse_index_name(raw: &str) -> Result<String, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "InvalidIndexName",
            format!("Invalid index path segment {raw:?}; expected indexes('name')."),
        )
    };
    let inner = raw
        .strip_prefix("indexes(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(invalid)?;
    let name = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .ok_or_else(invalid)?;
    if name.is_empty() {
        return Err(invalid());
    }
    Ok(name.to_owned())
}

fn parse_body(body: &axum::body::Bytes) -> Result<Value, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidRequest",
            "Request body is required.",
        ));
    }
    serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request("InvalidRequest", format!("Invalid JSON body: {e}")))
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        let name = parts.next()?;
        let value = parts.next().unwrap_or("");
        (name == key).then_some(value)
    })
}

fn extract_index(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/indexes(")?;
    let name = rest.split(')').next()?;
    let name = name.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))?;
    (!name.is_empty()).then_some(name.to_owned())
}

fn operation_for(method: &axum::http::Method, path: &str) -> &'static str {
    if path == "/health" {
        return "health";
    }
    if path == "/admin/reset" {
        return "adminReset";
    }
    if path.contains("/docs/search.index") {
        return "uploadDocuments";
    }
    if path.contains("/docs/search.post.search") {
        return "search";
    }
    match method.as_str() {
        "PUT" => "createIndex",
        "DELETE" => "deleteIndex",
        "GET" => "getIndex",
        _ => "unknown",
    }
}
