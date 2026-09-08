use std::sync::Arc;

use aisearch_emulator::api::{build_router, AppState};
use aisearch_emulator::config::{Config, StorageMode};
use aisearch_emulator::storage::{InMemoryStorage, Storage};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

pub const API_KEY: &str = "test-key";
pub const API_VERSION: &str = "2024-07-01";

pub fn must<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("expected Ok, got error: {err}"),
    }
}

pub fn app() -> axum::Router {
    app_with_admin(true)
}

pub fn app_with_admin(enable_admin: bool) -> axum::Router {
    let config = Config {
        port: 8080,
        storage_mode: StorageMode::Memory,
        api_versions: vec![API_VERSION.to_owned()],
        log_level: "off".to_owned(),
        enable_admin,
    };
    let storage: Arc<dyn Storage> = Arc::new(InMemoryStorage::new());
    build_router(AppState::new(config, storage))
}

pub fn index_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true}
        ]
    })
}

pub fn request(
    method: &str,
    uri: &str,
    api_key: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = api_key {
        builder = builder.header("api-key", key);
    }
    let bytes = match body.map(|value| serde_json::to_vec(&value)).transpose() {
        Ok(bytes) => bytes,
        Err(err) => panic!("failed to serialize body: {err}"),
    };
    let body = bytes.map(Body::from).unwrap_or_default();
    must(builder.body(body))
}

pub fn put_index_request(
    name: &str,
    api_key: Option<&str>,
    api_version: Option<&str>,
) -> Request<Body> {
    let uri = match api_version {
        Some(version) => format!("/indexes('{name}')?api-version={version}"),
        None => format!("/indexes('{name}')"),
    };
    request("PUT", &uri, api_key, Some(index_definition(name)))
}

pub fn post_index_request(
    name: &str,
    api_key: Option<&str>,
    api_version: Option<&str>,
) -> Request<Body> {
    let uri = match api_version {
        Some(version) => format!("/indexes?api-version={version}"),
        None => "/indexes".to_owned(),
    };
    request("POST", &uri, api_key, Some(index_definition(name)))
}

pub fn upload_request(name: &str, documents: Value) -> Request<Body> {
    let uri = format!("/indexes('{name}')/docs/search.index?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(documents))
}

pub fn search_request(name: &str, query: Value) -> Request<Body> {
    let uri = format!("/indexes('{name}')/docs/search.post.search?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(query))
}

pub async fn call(app: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = must(app.oneshot(request).await);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(err) => panic!("failed to read response body: {err}"),
    };
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        must(serde_json::from_slice(&bytes))
    };
    (status, value)
}

pub async fn create_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    let app = app.clone();
    call(
        app,
        post_index_request(name, Some(API_KEY), Some(API_VERSION)),
    )
    .await
}
