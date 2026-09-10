use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

async fn app_with_priced_docs() -> axum::Router {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "cheap", "price": 5.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "mid", "price": 50.0}},
                {"@search.action": "upload", "document": {"id": "3", "title": "expensive", "price": 500.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn filter_comparison_narrows_results() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "price ge 50"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["id"], "2");
    assert_eq!(value[1]["id"], "3");
}

#[tokio::test]
async fn filter_logical_operators_combine() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "*", "filter": "price lt 10 or price gt 100"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "1");
    assert_eq!(body["value"][1]["id"], "3");

    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "filter": "not (price lt 100)"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "3");
}

#[tokio::test]
async fn filter_combines_with_full_text() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "mid", "filter": "price gt 10"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "2");
}

#[tokio::test]
async fn filter_invalid_syntax_returns_400() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "price eq"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn filter_unknown_field_returns_400() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "missing eq 1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn filter_non_filterable_field_returns_400() {
    // `title` is searchable but not filterable in the shared test index.
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "filter": "title eq 'cheap'"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

async fn app_with_tagged_docs() -> axum::Router {
    let app = app();
    let (status, _) = call(
        app.clone(),
        request(
            "PUT",
            &format!("/indexes('tagged')?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(json!({
                "name": "tagged",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "tagged",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "tags": ["red", "blue"]}},
                {"@search.action": "upload", "document": {"id": "2", "tags": ["green"]}},
                {"@search.action": "upload", "document": {"id": "3", "tags": ["red", "green"]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn filter_odata_lambda_any_all() {
    let app = app_with_tagged_docs().await;

    // OData lambda `any`: `tags/any(t: t eq 'red')`.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/any(t: t eq 'red')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|d| d["id"].as_str().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(ids, vec!["1", "3"]);

    // OData lambda `all`: `tags/all(t: t ne 'green')` matches only doc 1.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/all(t: t ne 'green')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");

    // The space-separated form still works alongside the OData form.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags any t eq 'red'"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));

    // A malformed lambda (missing the ':' separator) is rejected.
    let (status, body) = call(
        app,
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/any(t eq 'red')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}
