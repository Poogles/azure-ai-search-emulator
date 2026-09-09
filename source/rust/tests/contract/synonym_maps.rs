use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

fn create_request(
    name: &str,
    format: &str,
    synonyms: &str,
) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/synonymmaps?api-version={API_VERSION}");
    request(
        "POST",
        &uri,
        Some(API_KEY),
        Some(json!({
            "name": name,
            "format": format,
            "synonyms": synonyms
        })),
    )
}

fn map_request(
    method: &str,
    name: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/synonymmaps('{name}')?api-version={API_VERSION}");
    request(method, &uri, Some(API_KEY), body)
}

fn list_request() -> axum::http::Request<axum::body::Body> {
    let uri = format!("/synonymmaps?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn create_synonym_map_returns_201_with_etag() {
    let app = app();
    let (status, body) = call(
        app,
        create_request(
            "map1",
            "solr",
            "USA, United States\nWashington, Wash. => WA",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "map1");
    assert_eq!(body["format"], "solr");
    assert_eq!(
        body["synonyms"],
        "USA, United States\nWashington, Wash. => WA"
    );
    assert!(body["@odata.etag"].is_string());
}

#[tokio::test]
async fn create_duplicate_synonym_map_returns_409() {
    let app = app();
    let (status, _) = call(app.clone(), create_request("map1", "solr", "a, b")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, create_request("map1", "solr", "c, d")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "SynonymMapAlreadyExists");
}

#[tokio::test]
async fn create_synonym_map_requires_auth() {
    let app = app();
    let uri = format!("/synonymmaps?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"name": "map1", "format": "solr", "synonyms": "a, b"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn create_synonym_map_rejects_unsupported_format() {
    let app = app();
    let (status, body) = call(app, create_request("map1", "json", "a, b")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidSynonymMap");
}

#[tokio::test]
async fn create_synonym_map_rejects_missing_name() {
    let app = app();
    let uri = format!("/synonymmaps?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"format": "solr", "synonyms": "a, b"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidSynonymMap");
}

#[tokio::test]
async fn create_synonym_map_rejects_empty_synonyms() {
    let app = app();
    let (status, body) = call(app, create_request("map1", "solr", "   ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidSynonymMap");
}

#[tokio::test]
async fn list_synonym_maps_returns_value_array_sorted_by_name() {
    let app = app();
    let (status, _) = call(app.clone(), create_request("zeta", "solr", "a, b")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), create_request("alpha", "solr", "c, d")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, list_request()).await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["name"], "alpha");
    assert_eq!(value[1]["name"], "zeta");
}

#[tokio::test]
async fn list_synonym_maps_empty_returns_empty_value() {
    let app = app();
    let (status, body) = call(app, list_request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn get_synonym_map_returns_map() {
    let app = app();
    let (status, _) = call(app.clone(), create_request("map1", "solr", "a, b\nc, d")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, map_request("GET", "map1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "map1");
    assert_eq!(body["format"], "solr");
    assert_eq!(body["synonyms"], "a, b\nc, d");
    assert!(body["@odata.etag"].is_string());
}

#[tokio::test]
async fn get_missing_synonym_map_returns_404() {
    let app = app();
    let (status, body) = call(app, map_request("GET", "missing", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn get_synonym_map_requires_auth() {
    let app = app();
    let uri = format!("/synonymmaps('map1')?api-version={API_VERSION}");
    let (status, body) = call(app, request("GET", &uri, None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn update_synonym_map_replaces_and_bumps_etag() {
    let app = app();
    let (status, created) = call(app.clone(), create_request("map1", "solr", "a, b")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app.clone(),
        map_request(
            "PUT",
            "map1",
            Some(json!({"name": "map1", "format": "solr", "synonyms": "c, d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["synonyms"], "c, d");
    assert_ne!(body["@odata.etag"], created["@odata.etag"]);

    let (status, body) = call(app, map_request("GET", "map1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["synonyms"], "c, d");
}

#[tokio::test]
async fn update_synonym_map_name_mismatch_returns_400() {
    let app = app();
    let (status, body) = call(
        app,
        map_request(
            "PUT",
            "map1",
            Some(json!({"name": "other", "format": "solr", "synonyms": "a, b"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidSynonymMap");
}

#[tokio::test]
async fn delete_synonym_map_returns_204_then_404() {
    let app = app();
    let (status, _) = call(app.clone(), create_request("map1", "solr", "a, b")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = call(app.clone(), map_request("DELETE", "map1", None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(app, map_request("GET", "map1", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn delete_missing_synonym_map_returns_404() {
    let app = app();
    let (status, body) = call(app, map_request("DELETE", "missing", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn admin_reset_clears_synonym_maps() {
    let app = app();
    let (status, _) = call(app.clone(), create_request("map1", "solr", "a, b")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = call(app.clone(), request("POST", "/admin/reset", None, None)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, list_request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}
