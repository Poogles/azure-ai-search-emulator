use axum::http::StatusCode;
use serde_json::{json, Value};

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
async fn upload_accepts_geography_point_geojson_value() {
    let app = app();
    let definition = json!({
        "name": "hotels",
        "fields": [
            {"name": "HotelId", "type": "Edm.Int32", "key": true},
            {"name": "Location", "type": "Edm.GeographyPoint", "searchable": true}
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

    // The SDK serializes GeographyPoint as a GeoJSON object, not a string.
    let (status, body) = call(
        app,
        upload_request(
            "hotels",
            json!([
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": 100,
                        "Location": {"type": "Point", "coordinates": [-122.131_577, 47.678_581]}
                    }
                }
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["key"], "100");
    assert_eq!(body["value"][0]["status"], true);
    assert_eq!(body["value"][0]["statusCode"], 201);
    assert!(body["value"][0]["errorMessage"].is_null());
}

fn get_document_request(name: &str, key: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/docs('{key}')?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn get_document_returns_stored_document() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "one", "price": 1.5}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, get_document_request("items", "1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "1");
    assert_eq!(body["title"], "one");
    assert_eq!(body["price"], 1.5);
}

#[tokio::test]
async fn get_missing_document_returns_404() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, get_document_request("items", "missing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn get_document_on_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(app, get_document_request("missing", "1")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

// ---------------------------------------------------------------------------
// `stored` / `retrievable` field-visibility enforcement
// ---------------------------------------------------------------------------

/// An index exercising all four `stored`/`retrievable` combinations:
/// `title` (both true, the default), `secret` (stored, not retrievable),
/// `ephemeral` (retrievable, not stored), and `ghost` (neither).
fn stored_retrievable_index(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "secret", "type": "Edm.String", "searchable": true, "retrievable": false},
            {"name": "ephemeral", "type": "Edm.String", "searchable": true, "stored": false},
            {"name": "ghost", "type": "Edm.String", "searchable": true, "stored": false, "retrievable": false}
        ]
    })
}

async fn create_stored_retrievable_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(stored_retrievable_index(name)),
        ),
    )
    .await
}

fn stored_retrievable_doc() -> Value {
    json!([
        {"@search.action": "upload", "document": {
            "id": "1", "title": "one", "secret": "s1", "ephemeral": "e1", "ghost": "g1"
        }}
    ])
}

#[tokio::test]
async fn search_omits_non_retrievable_and_returns_retrievable() {
    let app = app();
    let (status, _) = create_stored_retrievable_index(&app, "sr").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("sr", stored_retrievable_doc())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, search_request("sr", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::OK);
    let doc = &body["value"][0];
    // Key field, the default stored+retrievable field, and the retrievable
    // (non-stored) field are all returned.
    assert_eq!(doc["id"], "1");
    assert_eq!(doc["title"], "one");
    assert_eq!(doc["ephemeral"], "e1");
    // `retrievable: false` (stored) is omitted unless explicitly selected.
    assert!(doc.get("secret").is_none(), "secret present: {doc}");
    // `stored: false, retrievable: false` is never returned.
    assert!(doc.get("ghost").is_none(), "ghost present: {doc}");
}

#[tokio::test]
async fn search_select_includes_stored_non_retrievable() {
    let app = app();
    let (status, _) = create_stored_retrievable_index(&app, "sr").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("sr", stored_retrievable_doc())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request("sr", json!({"search": "*", "select": "id,secret"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let doc = &body["value"][0];
    // Selecting the stored, non-retrievable field returns it.
    assert_eq!(doc["id"], "1");
    assert_eq!(doc["secret"], "s1");
    // Unselected fields are projected out.
    assert!(doc.get("title").is_none(), "title present: {doc}");
    assert!(doc.get("ephemeral").is_none(), "ephemeral present: {doc}");
}

#[tokio::test]
async fn get_document_returns_only_stored_fields() {
    let app = app();
    let (status, _) = create_stored_retrievable_index(&app, "sr").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("sr", stored_retrievable_doc())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, get_document_request("sr", "1")).await;
    assert_eq!(status, StatusCode::OK);
    // Key + stored fields, including the stored, non-retrievable one.
    assert_eq!(body["id"], "1");
    assert_eq!(body["title"], "one");
    assert_eq!(body["secret"], "s1");
    // `stored: false` fields are not persisted for get_document.
    assert!(body.get("ephemeral").is_none(), "ephemeral present: {body}");
    assert!(body.get("ghost").is_none(), "ghost present: {body}");
}

fn document_count_request(name: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/docs/$count?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn document_count_returns_count() {
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
                {"@search.action": "upload", "document": {"id": "3", "title": "three"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, document_count_request("items")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, 3);
}

#[tokio::test]
async fn document_count_empty_index_returns_zero() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, document_count_request("items")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, 0);
}

#[tokio::test]
async fn document_count_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(app, document_count_request("missing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
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

#[tokio::test]
async fn upload_validates_narrow_integer_time_duration_binary_values() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({
                "name": "typed",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "level", "type": "Edm.Int8"},
                    {"name": "code", "type": "Edm.Int16"},
                    {"name": "at", "type": "Edm.Time"},
                    {"name": "dur", "type": "Edm.Duration"},
                    {"name": "blob", "type": "Edm.Binary"}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "typed",
            json!([
                {"@search.action": "upload", "document": {
                    "id": "ok", "level": 100, "code": 30000,
                    "at": "10:30:45", "dur": "P1DT2H", "blob": "aGVsbG8="}},
                {"@search.action": "upload", "document": {
                    "id": "bad-int", "level": 1000, "code": 1,
                    "at": "10:30:45", "dur": "P1D", "blob": "aGk="}},
                {"@search.action": "upload", "document": {
                    "id": "bad-time", "level": 1, "code": 1,
                    "at": "25:00:00", "dur": "P1D", "blob": "aGk="}},
                {"@search.action": "upload", "document": {
                    "id": "bad-dur", "level": 1, "code": 1,
                    "at": "10:30:45", "dur": "tomorrow", "blob": "aGk="}},
                {"@search.action": "upload", "document": {
                    "id": "bad-blob", "level": 1, "code": 1,
                    "at": "10:30:45", "dur": "P1D", "blob": "***"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["value"].as_array().cloned().unwrap_or_default();
    assert_eq!(results.len(), 5);
    assert_eq!(results[0]["status"], true);
    for result in &results[1..] {
        assert_eq!(result["status"], false, "invalid doc accepted: {result}");
        assert_eq!(result["statusCode"], 400);
    }
}
