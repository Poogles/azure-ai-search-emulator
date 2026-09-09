use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

async fn app_with_five_docs() -> axum::Router {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "one"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "two"}},
                {"@search.action": "upload", "document": {"id": "3", "title": "three"}},
                {"@search.action": "upload", "document": {"id": "4", "title": "four"}},
                {"@search.action": "upload", "document": {"id": "5", "title": "five"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn top_and_skip_page_results() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "top": 2, "skip": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "2");
    assert_eq!(body["value"][1]["id"], "3");
}

#[tokio::test]
async fn full_page_has_no_continuation() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "top": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(5));
    assert!(body.get("@odata.nextLink").is_none());
    assert!(body.get("@search.nextPageParameters").is_none());
}

#[tokio::test]
async fn partial_page_returns_continuation() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "*", "top": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "1");
    let Some(next_link) = body["@odata.nextLink"].as_str() else {
        panic!("expected @odata.nextLink in partial page response");
    };
    assert!(next_link.contains("continuation="));
    let Some(next_params) = body["@search.nextPageParameters"].as_object() else {
        panic!("expected @search.nextPageParameters in partial page response");
    };
    let next_params = next_params.clone();
    assert!(next_params["continuation"].is_string());

    // Follow the continuation: the next page starts after the first two.
    let (status, body) = call(app, search_request("items", json!(next_params))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "3");
    assert_eq!(body["value"][1]["id"], "4");
}

#[tokio::test]
async fn stale_continuation_after_mutation_returns_400() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "*", "top": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let next_params = body["@search.nextPageParameters"].clone();

    // A document mutation invalidates outstanding tokens.
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "6", "title": "six"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, search_request("items", next_params)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
    assert!(body["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("Stale")));
}

#[tokio::test]
async fn invalid_continuation_returns_400() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "continuation": "not-valid-base64!!"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}
