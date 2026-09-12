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
async fn continuation_survives_mutation() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "*", "top": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let next_params = body["@search.nextPageParameters"].clone();

    // A document mutation does NOT invalidate outstanding tokens (like Azure;
    // results may shift). The new doc sorts last, so skip=2 still resumes at "3".
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
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "3");
    assert_eq!(body["value"][1]["id"], "4");
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

#[tokio::test]
async fn continuation_only_request_preserves_filter() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "one", "price": 1.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "two", "price": 2.0}},
                {"@search.action": "upload", "document": {"id": "3", "title": "three", "price": 3.0}},
                {"@search.action": "upload", "document": {"id": "4", "title": "four", "price": 4.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, page1) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "*", "top": 1, "filter": "price ge 2", "orderby": "price desc"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["value"][0]["id"], "4");
    // The nextLink token is URL-safe: no `+` or `/` that a query string
    // would mangle.
    let next_link = page1["@odata.nextLink"].as_str().unwrap_or("");
    let token = next_link.split("continuation=").nth(1).unwrap_or("");
    assert!(!token.is_empty());
    assert!(
        !token.contains('+') && !token.contains('/'),
        "token must be URL-safe: {token}"
    );

    // Later pages may send only the continuation (plus a page size) and stay
    // on the same filtered, ordered result set.
    let mut params = page1["@search.nextPageParameters"].clone();
    let mut last_id = String::new();
    let mut last_had_token = true;
    for expected in ["3", "2"] {
        let continuation = params["continuation"].as_str().unwrap_or("").to_owned();
        let (status, page) = call(
            app.clone(),
            search_request(
                "items",
                json!({"search": "*", "top": 1, "continuation": continuation}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["value"].as_array().map(Vec::len), Some(1));
        last_id = page["value"][0]["id"].as_str().unwrap_or("").to_owned();
        assert_eq!(last_id, expected);
        last_had_token = page.get("@odata.nextLink").is_some();
        params = page["@search.nextPageParameters"].clone();
    }
    // The final page carries no continuation.
    assert_eq!(last_id, "2");
    assert!(!last_had_token);
}

#[tokio::test]
async fn top_zero_returns_empty_page_without_token() {
    let app = app_with_five_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "top": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("@odata.count").is_none());
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
    // No token: an empty page ends the sequence rather than encoding the
    // same skip (which would loop forever).
    assert!(body.get("@odata.nextLink").is_none());
    assert!(body.get("@search.nextPageParameters").is_none());
}
