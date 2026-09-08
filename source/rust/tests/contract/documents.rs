use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

#[tokio::test]
async fn upload_documents_returns_per_document_results() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "one"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "two"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["key"], "1");
    assert_eq!(value[0]["status"], true);
    assert_eq!(value[0]["statusCode"], 201);
    assert!(value[0]["errorMessage"].is_null());
    assert_eq!(value[1]["key"], "2");
    assert_eq!(value[1]["status"], true);
}

#[tokio::test]
async fn upload_invalid_document_reports_failure_in_batch() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "one"}},
                {"@search.action": "upload", "document": {"title": "missing key"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], true);
    assert_eq!(body["value"][1]["status"], false);
    assert_eq!(body["value"][1]["statusCode"], 400);
    assert!(body["value"][1]["errorMessage"].is_string());
}

#[tokio::test]
async fn search_returns_azure_response_shape() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "b", "title": "azure search"}},
                {"@search.action": "upload", "document": {"id": "a", "title": "azure emulators"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, search_request("items", json!({"search": "azure"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["@odata.context"].is_string());
    assert!(body["@search.facets"].is_null());
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["@search.score"], 1.0);
    assert_eq!(value[0]["id"], "a");
    assert_eq!(value[0]["title"], "azure emulators");
    assert_eq!(value[1]["id"], "b");
}

#[tokio::test]
async fn search_with_count_returns_odata_count() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "same"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "same"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["@odata.count"], 2);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn search_match_all_returns_every_document() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "alpha"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "beta"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn upload_to_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(
        app,
        upload_request(
            "missing",
            json!([{"@search.action": "upload", "document": {"id": "1"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}
