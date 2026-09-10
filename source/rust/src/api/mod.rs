//! HTTP / Azure compatibility layer.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::error::ApiError;
use crate::query::SearchEngine;
use crate::service::{ActionKind, DocumentAction, SearchOutcome, SearchService};
use crate::vector::VectorEngine;
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
        let vectors = Arc::new(VectorEngine::new());
        let versions = VersionAdapter::new(config.api_versions.clone());
        let max_vector_dimension = config.max_vector_dimension;
        Self {
            service: Arc::new(SearchService::new(
                storage,
                engine,
                vectors,
                max_vector_dimension,
            )),
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
            "/synonymmaps",
            post(create_synonym_map).get(list_synonym_maps),
        )
        .route(
            "/{index}",
            get(get_index)
                .put(create_or_update_index)
                .delete(delete_index),
        )
        .route("/{index}/docs/search.index", post(upload_documents))
        .route("/{index}/docs/search.post.search", post(search_documents))
        .route(
            "/{index}/docs/search.post.autocomplete",
            post(autocomplete_documents),
        )
        .route("/{index}/docs/search.post.suggest", post(suggest_documents))
        .route("/{index}/docs/$count", get(document_count))
        .route("/{index}/search.analyze", post(analyze_text))
        .route("/servicestats", get(service_stats))
        // `docs('key')` is a single OData path segment, so this is a
        // two-segment route (unlike the three-segment search routes above).
        // It accepts any method and rejects non-GET requests with a 404 so
        // that unknown routes keep returning 404 rather than 405 (e.g.
        // `POST /admin/reset` when the admin surface is disabled).
        .route("/{index}/{key}", any(document_by_key))
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
/// index (and its documents). Also dispatches `PUT /synonymmaps('{name}')`
/// (create or update synonym map), which shares the single-segment route.
async fn create_or_update_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if let Some(raw) = raw_name.strip_prefix("synonymmaps(") {
        let name = parse_synonym_map_name(raw)?;
        let definition = parse_body(&body)?;
        let body_name = definition.get("name").and_then(Value::as_str).unwrap_or("");
        if name != body_name {
            return Err(ApiError::bad_request(
                "InvalidSynonymMap",
                format!("Synonym map name in path ({name:?}) does not match name in body."),
            ));
        }
        let format = definition
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("");
        let synonyms = definition
            .get("synonyms")
            .and_then(Value::as_str)
            .unwrap_or("");
        let map = state
            .service
            .create_or_update_synonym_map(&name, format, synonyms)?;
        return Ok((StatusCode::CREATED, Json(map.to_value())));
    }
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

/// `GET /indexes('{name}')` — Get Index. Also dispatches
/// `GET /synonymmaps('{name}')` (get synonym map), which shares the
/// single-segment route.
async fn get_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if let Some(raw) = raw_name.strip_prefix("synonymmaps(") {
        let name = parse_synonym_map_name(raw)?;
        return Ok(Json(state.service.get_synonym_map(&name)?.to_value()));
    }
    let name = parse_index_name(&raw_name)?;
    Ok(Json(state.service.get_index(&name)?))
}

/// `DELETE /indexes('{name}')` — Delete Index. Also dispatches
/// `DELETE /synonymmaps('{name}')` (delete synonym map), which shares the
/// single-segment route.
async fn delete_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if let Some(raw) = raw_name.strip_prefix("synonymmaps(") {
        let name = parse_synonym_map_name(raw)?;
        state.service.delete_synonym_map(&name)?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let name = parse_index_name(&raw_name)?;
    state.service.delete_index(&name)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /synonymmaps` — Create Synonym Map. The name comes from the body.
async fn create_synonym_map(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let definition = parse_body(&body)?;
    let name = definition.get("name").and_then(Value::as_str).unwrap_or("");
    let format = definition
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("");
    let synonyms = definition
        .get("synonyms")
        .and_then(Value::as_str)
        .unwrap_or("");
    let map = state.service.create_synonym_map(name, format, synonyms)?;
    Ok((StatusCode::CREATED, Json(map.to_value())))
}

/// `GET /synonymmaps` — List Synonym Maps.
async fn list_synonym_maps(State(state): State<AppState>) -> Json<Value> {
    let maps = state
        .service
        .list_synonym_maps()
        .into_iter()
        .map(|map| map.to_value())
        .collect::<Vec<_>>();
    Json(json!({ "value": maps }))
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

/// `GET /indexes('{name}')/docs('{key}')` — Get Document. Any other method
/// on a two-segment path is not an Azure route: it falls through with the
/// same empty 404 the router's fallback would produce, so unimplemented
/// routes keep their pinned signatures (e.g. `POST .../search.analyze`).
async fn document_by_key(
    State(state): State<AppState>,
    method: Method,
    Path((raw_name, raw_key)): Path<(String, String)>,
) -> Result<Json<Value>, Response> {
    if method != Method::GET {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    let name = parse_index_name(&raw_name).map_err(IntoResponse::into_response)?;
    let key = parse_document_key(&raw_key).map_err(IntoResponse::into_response)?;
    state
        .service
        .get_document(&name, &key)
        .map(Json)
        .map_err(IntoResponse::into_response)
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

/// `POST /indexes('{name}')/docs/search.post.autocomplete` — Autocomplete.
/// Returns the completed terms for the search text, matched by prefix against
/// the suggester's search fields.
async fn autocomplete_documents(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    let raw = parse_body(&body)?;
    let (search_text, suggester_name, top) = suggest_request_params(&raw, uri.query())?;
    let completions = state
        .service
        .autocomplete(&name, &suggester_name, &search_text, top)?;
    let value = completions
        .iter()
        .map(|c| {
            json!({
                "text": c.text,
                "queryPlusText": c.query_plus_text,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "value": value })))
}

/// `POST /indexes('{name}')/docs/search.post.suggest` — Suggest. Returns the
/// documents that match the search text against the suggester's search
/// fields, each with an additional `@search.text` field carrying the matched
/// word.
async fn suggest_documents(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    let raw = parse_body(&body)?;
    let (search_text, suggester_name, top) = suggest_request_params(&raw, uri.query())?;
    let suggestions = state
        .service
        .suggest(&name, &suggester_name, &search_text, top)?;
    let value = suggestions
        .iter()
        .map(|s| {
            let mut entry = Map::new();
            entry.insert("@search.text".to_owned(), Value::String(s.text.clone()));
            for (key, value) in &s.document.fields {
                entry.insert(key.clone(), value.clone());
            }
            Value::Object(entry)
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "value": value })))
}

/// `GET /indexes('{name}')/docs/$count` — Document Count. Returns a bare
/// integer (the number of documents in the index).
async fn document_count(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<(StatusCode, axum::http::HeaderMap, axum::body::Bytes), ApiError> {
    let name = parse_index_name(&raw_name)?;
    let count = state.service.count_documents(&name)?;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok((
        StatusCode::OK,
        headers,
        axum::body::Bytes::from(count.to_string()),
    ))
}

/// `POST /indexes('{name}')/search.analyze` — Analyze Text. Tokenizes the
/// provided text and returns the tokens with offsets and positions.
async fn analyze_text(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    state.service.require_index_public(&name)?;
    let raw = parse_body(&body)?;
    let text = raw.get("text").and_then(Value::as_str).ok_or_else(|| {
        ApiError::bad_request("InvalidRequest", "The \"text\" field is required.")
    })?;
    let tokens = crate::query::analyze_with_offsets(text);
    let token_values: Vec<Value> = tokens
        .iter()
        .map(|t| {
            json!({
                "token": t.token,
                "startOffset": t.start_offset,
                "endOffset": t.end_offset,
                "position": t.position,
            })
        })
        .collect();
    Ok(Json(json!({ "tokens": token_values })))
}

/// `GET /servicestats` — Service Statistics. Returns a static response with
/// zero counters and default limits.
async fn service_stats() -> Json<Value> {
    Json(json!({
        "counters": {
            "knowledgeBaseCounter": {"usage": 0},
            "knowledgeSourceCounter": {"usage": 0}
        },
        "limits": {
            "maxVectorIndexSizePerIndexInBytes": 1_073_741_824
        }
    }))
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
            // Vector and hybrid searches carry per-document scores; plain
            // full-text searches default to 1.0.
            let score = outcome.scores.get(&doc.key).copied().unwrap_or(1.0);
            entry.insert("@search.score".to_owned(), json!(score));
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
    // (`/indexes`, `/indexes('name')...`, `/synonymmaps`,
    // `/synonymmaps('name')...`, and `/servicestats`); let everything else
    // fall through to routing.
    let path = request.uri().path();
    if path != "/indexes"
        && !path.starts_with("/indexes(")
        && path != "/synonymmaps"
        && !path.starts_with("/synonymmaps(")
        && path != "/servicestats"
    {
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

/// Parses the OData-style synonym-map path segment `synonymmaps('name')`.
/// Takes the segment with the `synonymmaps(` prefix already stripped.
fn parse_synonym_map_name(raw: &str) -> Result<String, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "InvalidSynonymMap",
            format!("Invalid synonym map path segment {raw:?}; expected synonymmaps('name')."),
        )
    };
    let inner = raw.strip_suffix(')').ok_or_else(invalid)?;
    let name = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .ok_or_else(invalid)?;
    if name.is_empty() {
        return Err(invalid());
    }
    Ok(name.to_owned())
}

/// Parses the OData-style document path segment `docs('key')`.
fn parse_document_key(raw: &str) -> Result<String, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "InvalidRequest",
            format!("Invalid document path segment {raw:?}; expected docs('key')."),
        )
    };
    let inner = raw
        .strip_prefix("docs(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(invalid)?;
    let key = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .ok_or_else(invalid)?;
    if key.is_empty() {
        return Err(invalid());
    }
    Ok(key.to_owned())
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

/// Extracts the suggest/autocomplete request parameters: the search text
/// (`search`), the suggester name (`suggesterName`), and the result limit
/// (`top`, default 5). The pinned SDK sends these in the JSON body; the
/// query-string form (used by the GET variants of the routes) is also
/// accepted.
fn suggest_request_params(
    raw: &Value,
    query: Option<&str>,
) -> Result<(String, String, u64), ApiError> {
    let param = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|key| {
            let key: &str = key;
            raw.get(key)
                .and_then(Value::as_str)
                .or_else(|| query.and_then(|q| query_param(q, key)))
                .map(str::to_owned)
                .filter(|s| !s.trim().is_empty())
        })
    };
    let search_text = param(&["search", "searchText"]).ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "The \"search\" field is required.")
    })?;
    let suggester_name = param(&["suggesterName"]).ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "The \"suggesterName\" field is required.")
    })?;
    let top = match raw.get("top") {
        // An explicit `top` must be a positive integer; an absent (or null)
        // `top` falls back to the query string, then the default of 5.
        None | Some(Value::Null) => {
            match query.and_then(|q| query_param(q, "top").or_else(|| query_param(q, "$top"))) {
                None => 5,
                Some(raw_top) => parse_top_param(raw_top)?,
            }
        }
        Some(value) => value.as_u64().filter(|top| *top > 0).ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"top\" must be a positive integer.")
        })?,
    };
    Ok((search_text, suggester_name, top))
}

/// Parses a `top`/`$top` query-string value: must be a positive integer.
fn parse_top_param(raw: &str) -> Result<u64, ApiError> {
    raw.parse::<u64>()
        .ok()
        .filter(|top| *top > 0)
        .ok_or_else(|| ApiError::bad_request("InvalidQuery", "\"top\" must be a positive integer."))
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
    if path.contains("/docs/search.post.autocomplete") {
        return "autocomplete";
    }
    if path.contains("/docs/search.post.suggest") {
        return "suggest";
    }
    if path.contains("/docs(") {
        return if method.as_str() == "GET" {
            "getDocument"
        } else {
            "unknown"
        };
    }
    if path == "/synonymmaps" {
        return if method.as_str() == "POST" {
            "createSynonymMap"
        } else {
            "listSynonymMaps"
        };
    }
    if path.starts_with("/synonymmaps(") {
        return match method.as_str() {
            "GET" => "getSynonymMap",
            "PUT" => "createOrUpdateSynonymMap",
            "DELETE" => "deleteSynonymMap",
            _ => "unknown",
        };
    }
    match method.as_str() {
        "PUT" => "createIndex",
        "DELETE" => "deleteIndex",
        "GET" => "getIndex",
        _ => "unknown",
    }
}
