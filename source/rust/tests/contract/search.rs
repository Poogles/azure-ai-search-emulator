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
async fn search_facets_accept_count_option() {
    let app = app();
    let definition = json!({
        "name": "hotels",
        "fields": [
            {"name": "HotelId", "type": "Edm.Int32", "key": true},
            {"name": "Category", "type": "Edm.String", "facetable": true},
            {"name": "ParkingIncluded", "type": "Edm.Boolean", "facetable": true}
        ]
    });
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            "/indexes?api-version=2024-07-01",
            Some(API_KEY),
            Some(definition),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "hotels",
            json!([
                {"@search.action": "upload", "document": {"HotelId": 1, "Category": "boutique", "ParkingIncluded": true}},
                {"@search.action": "upload", "document": {"HotelId": 2, "Category": "boutique", "ParkingIncluded": false}},
                {"@search.action": "upload", "document": {"HotelId": 3, "Category": "luxury", "ParkingIncluded": true}},
                {"@search.action": "upload", "document": {"HotelId": 4, "Category": "motel", "ParkingIncluded": false}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The SDK sends facet options as `field,count:N` items in the array.
    let (status, body) = call(
        app,
        search_request(
            "hotels",
            json!({"search": "*", "facets": ["Category,count:3", "ParkingIncluded"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let category = &body["@search.facets"]["Category"];
    assert_eq!(category.as_array().map(Vec::len), Some(3));
    assert_eq!(category[0]["value"], "boutique");
    assert_eq!(category[0]["count"], 2);
    let parking = &body["@search.facets"]["ParkingIncluded"];
    assert_eq!(parking.as_array().map(Vec::len), Some(2));
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

fn analyze_request(name: &str, body: serde_json::Value) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/search.analyze?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(body))
}

#[tokio::test]
async fn analyze_text_returns_tokens_with_offsets() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        analyze_request("items", json!({"text": "Hello, World!"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tokens = &body["tokens"];
    assert!(tokens.as_array().map_or(0, Vec::len) >= 2);
    assert_eq!(tokens[0]["token"], "hello");
    assert_eq!(tokens[0]["startOffset"], 0);
    assert_eq!(tokens[0]["endOffset"], 5);
    assert_eq!(tokens[0]["position"], 0);
    assert_eq!(tokens[1]["token"], "world");
    assert_eq!(tokens[1]["position"], 1);
}

#[tokio::test]
async fn analyze_text_missing_text_field_returns_400() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, analyze_request("items", json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidRequest");
}

#[tokio::test]
async fn analyze_text_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(app, analyze_request("missing", json!({"text": "hello"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn service_stats_returns_static_response() {
    let app = app();
    let uri = format!("/servicestats?api-version={API_VERSION}");
    let (status, body) = call(app, request("GET", &uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["counters"]["knowledgeBaseCounter"]["usage"], 0);
    assert_eq!(body["counters"]["knowledgeSourceCounter"]["usage"], 0);
    assert_eq!(
        body["limits"]["maxVectorIndexSizePerIndexInBytes"],
        1_073_741_824
    );
}

#[tokio::test]
async fn service_stats_requires_api_key() {
    let app = app();
    let uri = format!("/servicestats?api-version={API_VERSION}");
    let (status, _) = call(app, request("GET", &uri, None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
