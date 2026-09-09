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
