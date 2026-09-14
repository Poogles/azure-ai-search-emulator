//! HTTP / Azure compatibility layer.

pub mod named_resources;

use std::borrow::Cow;
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
use crate::service::{DocumentAction, ResourceKind, SearchOutcome, SearchService};
use crate::vector::VectorEngine;
use crate::version::VersionAdapter;
use named_resources::{
    create_alias, create_knowledge_base, create_knowledge_source, create_or_update_named_resource,
    create_synonym_map, list_aliases, list_knowledge_bases, list_knowledge_sources,
    list_synonym_maps, parse_named_resource_segment, parse_named_segment,
};

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
        .route("/aliases", post(create_alias).get(list_aliases))
        .route(
            "/knowledgesources",
            post(create_knowledge_source).get(list_knowledge_sources),
        )
        .route(
            "/knowledgebases",
            post(create_knowledge_base).get(list_knowledge_bases),
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
/// index (and its documents). Also dispatches `PUT` on the named resource
/// routes (`synonymmaps('{name}')`, `aliases('{name}')`,
/// `knowledgesources('{name}')`, `knowledgebases('{name}')`), which share the
/// single-segment route.
async fn create_or_update_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let definition = parse_body(&body)?;
    if let Some((kind, name)) = parse_named_resource_segment(&raw_name)? {
        let value = create_or_update_named_resource(&state, kind, &name, &definition)?;
        return Ok((StatusCode::CREATED, Json(value)));
    }
    let name = parse_index_name(&raw_name)?;
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

/// `GET /indexes('{name}')` — Get Index. Also dispatches `GET` on the named
/// resource routes (`synonymmaps('{name}')`, `aliases('{name}')`,
/// `knowledgesources('{name}')`, `knowledgebases('{name}')`), which share the
/// single-segment route.
async fn get_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if let Some((kind, name)) = parse_named_resource_segment(&raw_name)? {
        let value = match kind {
            ResourceKind::SynonymMap => state.service.get_synonym_map(&name)?.to_value(),
            _ => state.service.get_named_resource(kind, &name)?.to_value(),
        };
        return Ok(Json(value));
    }
    let name = parse_index_name(&raw_name)?;
    Ok(Json(state.service.get_index(&name)?))
}

/// `DELETE /indexes('{name}')` — Delete Index. Also dispatches `DELETE` on
/// the named resource routes (`synonymmaps('{name}')`, `aliases('{name}')`,
/// `knowledgesources('{name}')`, `knowledgebases('{name}')`), which share the
/// single-segment route.
async fn delete_index(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if let Some((kind, name)) = parse_named_resource_segment(&raw_name)? {
        match kind {
            ResourceKind::SynonymMap => state.service.delete_synonym_map(&name)?,
            _ => state.service.delete_named_resource(kind, &name)?,
        }
        return Ok(StatusCode::NO_CONTENT);
    }
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
    let actions = batch_items(&batch)?;
    let batch = actions
        .iter()
        .map(DocumentAction::from_value)
        .collect::<Result<Vec<_>, ApiError>>()?;
    let results = state.service.index_documents(&name, batch)?;
    let value = Value::Array(
        results
            .iter()
            .map(crate::service::IndexingResultItem::to_value)
            .collect(),
    );
    Ok(Json(json!({ "value": value })))
}

/// Two-segment `OData` paths: routes on the first segment's prefix — the
/// knowledge-base `retrieve` route or the index document route. (The `OData`
/// segment `knowledgebases('name')` is a single dynamic segment, so the
/// prefix cannot be expressed in the router's static patterns.)
async fn document_by_key(
    State(state): State<AppState>,
    method: Method,
    Path((raw_name, raw_key)): Path<(String, String)>,
) -> Response {
    if raw_name.starts_with(ResourceKind::KnowledgeBase.path_prefix()) {
        knowledge_base_retrieve(&state, &method, &raw_name, &raw_key)
    } else {
        get_document_by_key(&state, &method, &raw_name, &raw_key)
    }
}

/// `POST /knowledgebases('{name}')/retrieve` — agentic retrieval. The
/// emulator performs no model inference (an initial-design non-goal), so it
/// returns an empty retrieval response; the knowledge base must exist.
/// Other methods on the retrieve route are 405; any other second segment
/// on a knowledge base is not an Azure route (404).
fn knowledge_base_retrieve(
    state: &AppState,
    method: &Method,
    raw_name: &str,
    raw_key: &str,
) -> Response {
    if raw_key != "retrieve" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let knowledge_base = ResourceKind::KnowledgeBase;
    let name = match parse_named_segment(
        raw_name,
        knowledge_base.path_prefix(),
        knowledge_base.label(),
        knowledge_base.invalid_code(),
    ) {
        Ok(name) => name,
        Err(error) => return error.into_response(),
    };
    match state.service.get_named_resource(knowledge_base, &name) {
        Ok(_) => Json(json!({
            "response": [],
            "activity": [],
            "references": []
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

/// `GET /indexes('{name}')/docs('{key}')` — Get Document. Any other method
/// on a two-segment path is not an Azure route: it falls through with the
/// same empty 404 the router's fallback would produce, so unimplemented
/// routes keep their pinned signatures (e.g. `POST .../search.analyze`).
fn get_document_by_key(
    state: &AppState,
    method: &Method,
    raw_name: &str,
    raw_key: &str,
) -> Response {
    if method != Method::GET {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = match parse_index_name(raw_name) {
        Ok(name) => name,
        Err(error) => return error.into_response(),
    };
    let key = match parse_document_key(raw_key) {
        Ok(key) => key,
        Err(error) => return error.into_response(),
    };
    match state.service.get_document(&name, &key) {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
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
        .map(Cow::into_owned);
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
    let (search_text, suggester_name, top, filter) = suggest_request_params(&raw, uri.query())?;
    let completions =
        state
            .service
            .autocomplete(&name, &suggester_name, &search_text, top, filter.as_deref())?;
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
    let (search_text, suggester_name, top, filter) = suggest_request_params(&raw, uri.query())?;
    let suggestions =
        state
            .service
            .suggest(&name, &suggester_name, &search_text, top, filter.as_deref())?;
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
) -> Result<(StatusCode, [(&'static str, &'static str); 1], String), ApiError> {
    let name = parse_index_name(&raw_name)?;
    let count = state.service.count_documents(&name)?;
    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        count.to_string(),
    ))
}

/// `POST /indexes('{name}')/search.analyze` — Analyze Text. Tokenizes the
/// provided text with the emulator's English analyzer and returns the tokens
/// with offsets and positions. An explicit `analyzer` (`analyzerName` alias
/// accepted) must be a known analyzer name and `field` (`fieldName` alias
/// accepted) must exist in the index schema; both otherwise map to the same
/// analyzer (see `docs/known_differences.md`).
async fn analyze_text(
    State(state): State<AppState>,
    Path(raw_name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_index_name(&raw_name)?;
    let raw = parse_body(&body)?;
    let text = raw.get("text").and_then(Value::as_str).ok_or_else(|| {
        ApiError::bad_request("InvalidRequest", "The \"text\" field is required.")
    })?;
    let analyzer = raw
        .get("analyzer")
        .or_else(|| raw.get("analyzerName"))
        .and_then(Value::as_str);
    let field = raw
        .get("field")
        .or_else(|| raw.get("fieldName"))
        .and_then(Value::as_str);
    state.service.validate_analyze(&name, analyzer, field)?;
    let tokens = crate::query::analyze_with_offsets_and_analyzer(text, analyzer);
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
    // Omit `@search.facets` when no facets were requested. Emitting an explicit
    // JSON null breaks the .NET SDK's SearchResults deserializer (it calls
    // EnumerateObject on the value); Azure omits the member.
    if let Some(facets) = &outcome.facets {
        map.insert("@search.facets".to_owned(), facets.clone());
    }
    map.insert(
        "value".to_owned(),
        Value::Array(build_page_entries(query, outcome)),
    );
    if let Some(token) = continuation {
        map.insert(
            "@odata.nextLink".to_owned(),
            Value::String(build_next_link(name, api_version, token)),
        );
        map.insert(
            "@search.nextPageParameters".to_owned(),
            build_next_page_params(raw_request, token, outcome.next_skip),
        );
    }
    Value::Object(map)
}

/// The `value` array of a search response: one entry per page document with
/// its `@search.score`, its `@search.highlights` (when present), and the
/// selected fields.
fn build_page_entries(query: &crate::service::SearchQuery, outcome: &SearchOutcome) -> Vec<Value> {
    outcome
        .documents
        .iter()
        .map(|doc| {
            let mut entry = Map::new();
            // Per-document BM25 relevance scores for full-text matches (best
            // score wins for hybrid matches); absent keys default to 1.0
            // (match-all queries carry no query to score against).
            let score = outcome.scores.get(&doc.key).copied().unwrap_or(1.0);
            entry.insert("@search.score".to_owned(), json!(score));
            // Highlight fragments, only for documents with matches in the
            // requested highlight fields.
            if let Some(fields) = outcome.highlights.get(&doc.key) {
                let highlights = fields
                    .iter()
                    .map(|(field, fragments)| (field.clone(), json!(fragments)))
                    .collect();
                entry.insert("@search.highlights".to_owned(), Value::Object(highlights));
            }
            for (key, value) in &doc.fields {
                if query.select.is_empty() || query.select.iter().any(|s| s == key) {
                    entry.insert(key.clone(), value.clone());
                }
            }
            Value::Object(entry)
        })
        .collect()
}

/// The `@odata.nextLink` for the next page: the search route with the
/// request's `api-version` (when present) and the continuation token.
fn build_next_link(name: &str, api_version: Option<&String>, token: &str) -> String {
    let mut next_link = format!("/indexes('{name}')/docs/search.post.search");
    let mut params = Vec::new();
    if let Some(version) = api_version {
        params.push(format!("api-version={version}"));
    }
    params.push(format!("continuation={token}"));
    next_link.push('?');
    next_link.push_str(&params.join("&"));
    next_link
}

/// The `@search.nextPageParameters` compatibility shim: the original request
/// body plus the paging cursor. The pinned Python SDK pages by re-POSTing
/// this (a serialized search request) rather than following nextLink, so the
/// next request is the original body plus the continuation token. The SDK
/// drops unknown properties on re-serialization, so the paging cursor is
/// also carried in the first-class `skip` property (which survives the
/// round-trip); `continuation` additionally binds the result-set state
/// (filter/orderby) for clients that preserve it.
fn build_next_page_params(raw_request: &Value, token: &str, next_skip: u64) -> Value {
    let mut next_params = match raw_request {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    next_params.insert("continuation".to_owned(), Value::String(token.to_owned()));
    next_params.insert("skip".to_owned(), Value::from(next_skip));
    Value::Object(next_params)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Whether a request path is on the Azure-compatible surface that requires
/// an API key and a supported `api-version`: the collection and named
/// (`resource('name')`) routes for indexes, synonym maps, aliases, knowledge
/// sources, and knowledge bases, plus `/servicestats`.
fn is_azure_surface_path(path: &str) -> bool {
    path == "/servicestats"
        || path == "/indexes"
        || is_named_segment(path, "/indexes")
        || ResourceKind::ALL.iter().any(|kind| {
            path == kind.collection_path() || is_named_segment(path, kind.collection_path())
        })
}

/// Whether `path` is a named (`resource('name')`) segment of a collection: it
/// starts with the collection path (e.g. `/indexes`) followed by `(`.
fn is_named_segment(path: &str, collection: &str) -> bool {
    path.starts_with(collection) && path.as_bytes().get(collection.len()) == Some(&b'(')
}

/// Authentication and API-version guard for the Azure-compatible surface.
///
/// - Any non-empty `api-key` header is accepted; missing/empty returns 401.
/// - The `api-version` query parameter is required and must be supported.
async fn azure_guard(
    State(state): State<AppState>,
    request: Request,
    next: middleware::Next,
) -> Result<Response, ApiError> {
    // Only enforce Azure auth/version on the index operation surface; let
    // everything else fall through to routing.
    if !is_azure_surface_path(request.uri().path()) {
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
    state.versions.check(api_version.as_deref())?;
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
        .map(Cow::into_owned);
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

/// Parses the OData-style document path segment `docs('key')`.
fn parse_document_key(raw: &str) -> Result<String, ApiError> {
    parse_named_segment(raw, "docs(", "document", "InvalidRequest")
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

/// The document actions of an upload batch: a bare JSON array, or an object
/// with a `"value"` array (the SDK's `IndexBatch` wire shape).
fn batch_items(batch: &Value) -> Result<&Vec<Value>, ApiError> {
    match batch {
        Value::Array(items) => Ok(items),
        _ => batch.get("value").and_then(Value::as_array).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidDocuments",
                "Document batch must be a JSON array of actions or an object with a \"value\" array.",
            )
        }),
    }
}

/// The value of `key` in a `form_urlencoded` query string, percent-decoded.
/// The value is borrowed from `query` unless decoding required allocation.
fn query_param<'a>(query: &'a str, key: &str) -> Option<Cow<'a, str>> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// Extracts the suggest/autocomplete request parameters: the search text
/// (`search`), the suggester name (`suggesterName`), and the result limit
/// (`top`, default 5). The pinned SDK sends these in the JSON body; the
/// query-string form (used by the GET variants of the routes) is also
/// accepted.
fn suggest_request_params(
    raw: &Value,
    query: Option<&str>,
) -> Result<(String, String, u64, Option<String>), ApiError> {
    let param = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|key| {
            let from_body = raw.get(key).and_then(Value::as_str).map(Cow::Borrowed);
            from_body
                .or_else(|| query.and_then(|q| query_param(q, key)))
                .map(Cow::into_owned)
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
                Some(raw_top) => parse_top_param(raw_top.as_ref())?,
            }
        }
        Some(value) => value.as_u64().filter(|top| *top > 0).ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"top\" must be a positive integer.")
        })?,
    };
    // An optional `filter` narrows the candidate documents (like the search
    // route); absent or empty means no narrowing.
    let filter = param(&["filter"]);
    Ok((search_text, suggester_name, top, filter))
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

/// The logging operation name for a request: fixed routes and document
/// subpaths are lookup tables; the named resource routes are derived from
/// [`ResourceKind`].
fn operation_for(method: &axum::http::Method, path: &str) -> &'static str {
    const FIXED: [(&str, &str); 2] = [("/health", "health"), ("/admin/reset", "adminReset")];
    const SUBPATHS: [(&str, &str); 4] = [
        ("/docs/search.index", "uploadDocuments"),
        ("/docs/search.post.search", "search"),
        ("/docs/search.post.autocomplete", "autocomplete"),
        ("/docs/search.post.suggest", "suggest"),
    ];
    for (prefix, operation) in FIXED {
        if prefix == path {
            return operation;
        }
    }
    for (prefix, operation) in SUBPATHS {
        if path.contains(prefix) {
            return operation;
        }
    }
    if path.contains("/docs(") {
        return if method.as_str() == "GET" {
            "getDocument"
        } else {
            "unknown"
        };
    }
    for kind in ResourceKind::ALL {
        if path == kind.collection_path() {
            return if method.as_str() == "POST" {
                kind.create_operation()
            } else {
                kind.list_operation()
            };
        }
        if is_named_segment(path, kind.collection_path()) {
            return kind.named_operation(method.as_str());
        }
    }
    match method.as_str() {
        "PUT" => "createIndex",
        "DELETE" => "deleteIndex",
        "GET" => "getIndex",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_page_params_carry_the_original_body_plus_cursor() {
        let raw = json!({ "search": "azure", "top": 2, "filter": "price gt 1" });
        let params = build_next_page_params(&raw, "tok123", 7);
        assert_eq!(
            params,
            json!({
                "search": "azure",
                "top": 2,
                "filter": "price gt 1",
                "continuation": "tok123",
                "skip": 7,
            })
        );
    }

    #[test]
    fn next_page_params_tolerate_a_non_object_body() {
        let params = build_next_page_params(&Value::Null, "tok123", 3);
        assert_eq!(params, json!({ "continuation": "tok123", "skip": 3 }));
    }

    #[test]
    fn next_page_params_override_existing_cursor_keys() {
        let raw = json!({ "continuation": "stale", "skip": 1 });
        let params = build_next_page_params(&raw, "fresh", 9);
        assert_eq!(params["continuation"], json!("fresh"));
        assert_eq!(params["skip"], json!(9));
    }

    #[test]
    fn next_link_carries_version_and_token() {
        let link = build_next_link("items", Some(&"2024-07-01".to_owned()), "tok123");
        assert_eq!(
            link,
            "/indexes('items')/docs/search.post.search?api-version=2024-07-01&continuation=tok123"
        );
        let link = build_next_link("items", None, "tok123");
        assert_eq!(
            link,
            "/indexes('items')/docs/search.post.search?continuation=tok123"
        );
    }
}
