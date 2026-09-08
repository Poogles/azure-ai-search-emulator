use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

#[tokio::test]
async fn admin_reset_clears_all_state() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let reset = request("POST", "/admin/reset", None, None);
    let (status, body) = call(app.clone(), reset).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "reset");

    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn admin_reset_disabled_returns_404() {
    let app = app_with_admin(false);
    let reset = request("POST", "/admin/reset", None, None);
    let (status, _) = call(app, reset).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_endpoint_requires_no_credentials() {
    let app = app();
    let health = request("GET", "/health", None, None);
    let (status, body) = call(app, health).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}
