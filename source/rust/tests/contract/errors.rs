use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

#[tokio::test]
async fn missing_api_key_returns_401() {
    let app = app();
    let (status, body) = call(app, put_index_request("items", None, Some(API_VERSION))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn empty_api_key_returns_401() {
    let app = app();
    let (status, body) = call(app, put_index_request("items", Some(""), Some(API_VERSION))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn unsupported_api_version_returns_400() {
    let app = app();
    let (status, body) = call(
        app,
        put_index_request("items", Some(API_KEY), Some("1900-01-01")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "ApiVersionUnsupported");
}

#[tokio::test]
async fn missing_api_version_returns_400() {
    let app = app();
    let (status, body) = call(app, put_index_request("items", Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "ApiVersionMissing");
}

#[tokio::test]
async fn operations_on_deleted_index_return_404() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let delete = request(
        "DELETE",
        &format!("/indexes('items')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, _) = call(app.clone(), delete).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "x"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");

    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn error_body_is_azure_compatible() {
    let app = app();
    let (status, body) = call(app, put_index_request("items", None, Some(API_VERSION))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error = &body["error"];
    assert!(error.is_object());
    assert!(error["code"].is_string());
    assert!(error["message"].is_string());
    assert_eq!(error.as_object().map(serde_json::Map::len), Some(2));
}
