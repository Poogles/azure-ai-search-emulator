use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

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
async fn search_excluded_terms_remove_matches() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure search"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "azure emulators"}},
                {"@search.action": "upload", "document": {"id": "3", "title": "other"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "azure -emulators"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");

    let (status, body) = call(app, search_request("items", json!({"search": "-azure"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "3");
}

#[tokio::test]
async fn search_quoted_phrase_requires_adjacency() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "quick brown fox"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "brown quick"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request("items", json!({"search": "\"quick brown\""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");
}

#[tokio::test]
async fn search_fields_restrict_scope() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure", "price": 1.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "other", "price": 2.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // "azure" only appears in the title, so scoping to price matches nothing.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "azure", "searchFields": "title"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));

    // An unknown search field is rejected.
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "azure", "searchFields": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}
