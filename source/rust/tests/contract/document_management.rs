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
async fn upload_accepts_sdk_value_envelope() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "items",
            json!({
                "value": [
                    {"@search.action": "upload", "id": "1", "title": "one"},
                    {"@search.action": "upload", "id": "2", "title": "two"}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["key"], "1");
    assert_eq!(value[0]["status"], true);
    assert_eq!(value[1]["key"], "2");
    assert_eq!(value[1]["status"], true);
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

#[tokio::test]
async fn merge_updates_existing_document() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "one", "price": 1.0}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "merge", "document": {"id": "1", "price": 2.0}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], true);
    assert_eq!(body["value"][0]["statusCode"], 200);

    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["title"], "one");
    assert_eq!(body["value"][0]["price"], 2.0);
}

#[tokio::test]
async fn merge_missing_document_reports_404_per_document() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "items",
            json!([{"@search.action": "merge", "document": {"id": "missing", "price": 2.0}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], false);
    assert_eq!(body["value"][0]["statusCode"], 404);
}

#[tokio::test]
async fn merge_or_upload_merges_existing_and_uploads_new() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "one", "price": 1.0}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        upload_request(
            "items",
            json!([
                {"@search.action": "mergeOrUpload", "document": {"id": "1", "price": 9.0}},
                {"@search.action": "mergeOrUpload", "document": {"id": "2", "title": "two", "price": 3.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["statusCode"], 200);
    assert_eq!(body["value"][1]["statusCode"], 201);
}

#[tokio::test]
async fn delete_removes_document() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
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

    let (status, body) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "delete", "document": {"id": "1"}},
                {"@search.action": "delete", "document": {"id": "missing"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], true);
    assert_eq!(body["value"][0]["statusCode"], 200);
    assert_eq!(body["value"][1]["status"], false);
    assert_eq!(body["value"][1]["statusCode"], 404);

    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "2");
}
