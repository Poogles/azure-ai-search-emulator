use axum::http::StatusCode;
use serde_json::json;

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
async fn create_or_update_index_upserts() {
    let app = app();
    // First PUT creates the index.
    let first = put_index_request("items", Some(API_KEY), Some(API_VERSION));
    let (status, body) = call(app.clone(), first).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "items");

    // A second PUT with the same name replaces it (no 409).
    let second = put_index_request("items", Some(API_KEY), Some(API_VERSION));
    let (status, body) = call(app, second).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "items");
}

#[tokio::test]
async fn in_place_update_preserves_documents_when_compatible() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let docs = json!([
        {"@search.action": "upload", "document": {"id": "1", "title": "alpha", "price": 10.0}},
        {"@search.action": "upload", "document": {"id": "2", "title": "beta", "price": 20.0}}
    ]);
    let (status, _) = call(app.clone(), upload_request("items", docs)).await;
    assert_eq!(status, StatusCode::OK);

    // Update in place with a compatible schema: add a field.
    let updated = json!({
        "name": "items",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true},
            {"name": "category", "type": "Edm.String", "filterable": true, "facetable": true}
        ]
    });
    let (status, body) = call(
        app.clone(),
        request(
            "PUT",
            &format!("/indexes('items')?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(updated),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "items");

    // The documents survive the in-place update.
    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|doc| doc["id"].as_str().unwrap_or(""))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(ids, vec!["1", "2"]);
}

#[tokio::test]
async fn in_place_update_discards_documents_when_incompatible() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let docs = json!([
        {"@search.action": "upload", "document": {"id": "1", "title": "alpha", "price": 10.0}}
    ]);
    let (status, _) = call(app.clone(), upload_request("items", docs)).await;
    assert_eq!(status, StatusCode::OK);

    // Update with an incompatible schema: drop the searchable `title` field.
    let updated = json!({
        "name": "items",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true}
        ]
    });
    let (status, _) = call(
        app.clone(),
        request(
            "PUT",
            &format!("/indexes('items')?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(updated),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The incompatible change replaces the index, discarding its documents.
    let (status, body) = call(app, search_request("items", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map_or(0, Vec::len), 0);
}

#[tokio::test]
async fn list_indexes_returns_created_indexes() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = create_index(&app, "other").await;
    assert_eq!(status, StatusCode::CREATED);

    let list = request(
        "GET",
        &format!("/indexes?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, body) = call(app, list).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["value"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(names, vec!["items", "other"]);
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
async fn get_index_returns_stored_definition() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let get = request(
        "GET",
        &format!("/indexes('items')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, body) = call(app, get).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "items");
    assert_eq!(body["fields"][0]["name"], "id");
    assert_eq!(body["fields"][0]["key"], true);
}

#[tokio::test]
async fn get_missing_index_returns_404() {
    let app = app();
    let get = request(
        "GET",
        &format!("/indexes('missing')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, body) = call(app, get).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn create_index_name_mismatch_returns_400_and_creates_nothing() {
    let app = app();
    // Path says 'items' but the body defines 'other'.
    let mismatched = request(
        "PUT",
        &format!("/indexes('items')?api-version={API_VERSION}"),
        Some(API_KEY),
        Some(index_definition("other")),
    );
    let (status, body) = call(app.clone(), mismatched).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidIndex");

    // The failed request must not have created the index.
    let get = request(
        "GET",
        &format!("/indexes('items')?api-version={API_VERSION}"),
        Some(API_KEY),
        None,
    );
    let (status, _) = call(app, get).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
