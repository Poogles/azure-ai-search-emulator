use axum::http::StatusCode;

use super::common::*;

#[tokio::test]
async fn create_index_returns_201_and_echoes_definition() {
    let app = app();
    let (status, body) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "items");
    assert_eq!(body["fields"][0]["name"], "id");
    assert_eq!(body["fields"][0]["key"], true);
    assert_eq!(body["fields"][1]["searchable"], true);
}

#[tokio::test]
async fn create_duplicate_index_returns_409() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "IndexAlreadyExists");
}

#[tokio::test]
async fn delete_index_returns_204() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let delete = request(
        "DELETE",
        &format!("/indexes('items')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, body) = call(app, delete).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null());
}

#[tokio::test]
async fn delete_missing_index_returns_404() {
    let app = app();
    let delete = request(
        "DELETE",
        &format!("/indexes('missing')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, body) = call(app, delete).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}
