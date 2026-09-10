use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::*;

fn hotels_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "HotelId", "type": "Edm.Int32", "key": true},
            {"name": "Category", "type": "Edm.String", "searchable": true},
            {
                "name": "Address",
                "type": "Edm.ComplexType",
                "fields": [
                    {"name": "City", "type": "Edm.String", "searchable": true, "filterable": true},
                    {"name": "StateProvince", "type": "Edm.String", "filterable": true},
                    {"name": "Country", "type": "Edm.String", "filterable": true}
                ]
            }
        ]
    })
}

async fn create_hotels_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    let app = app.clone();
    let uri = format!("/indexes?api-version={API_VERSION}");
    call(
        app,
        request("POST", &uri, Some(API_KEY), Some(hotels_definition(name))),
    )
    .await
}

#[tokio::test]
async fn create_index_accepts_complex_type_field() {
    let app = app();
    let (status, body) = create_hotels_index(&app, "hotels").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "complex type field rejected: {body}"
    );
}

#[tokio::test]
async fn upload_and_filter_on_nested_complex_field() {
    let app = app();
    let (status, body) = create_hotels_index(&app, "hotels").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "complex type field rejected: {body}"
    );

    let (status, body) = call(
        app.clone(),
        upload_request(
            "hotels",
            json!([
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": 1,
                        "Category": "boutique",
                        "Address": {"City": "Miami", "StateProvince": "FL", "Country": "USA"}
                    }
                },
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": 2,
                        "Category": "luxury",
                        "Address": {"City": "Seattle", "StateProvince": "WA", "Country": "USA"}
                    }
                },
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": 3,
                        "Category": "motel",
                        "Address": {"City": "Montreal", "StateProvince": "QC", "Country": "CAN"}
                    }
                }
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(3));
    for item in value.as_array().cloned().unwrap_or_default() {
        assert_eq!(item["status"], true, "upload failed: {item}");
    }

    // The exact filter shape used by sample_query_filter.py.
    let (status, body) = call(
        app,
        search_request(
            "hotels",
            json!({
                "search": "*",
                "filter": "Address/StateProvince eq 'FL' and Address/Country eq 'USA'"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "nested filter rejected: {body}");
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(1));
    assert_eq!(value[0]["HotelId"], 1);
}

fn collection_complex_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "HotelId", "type": "Edm.String", "key": true},
            {"name": "HotelName", "type": "Edm.String", "searchable": true},
            {
                "name": "Address",
                "type": "Edm.Collection(Edm.ComplexType)",
                "fields": [
                    {"name": "City", "type": "Edm.String", "searchable": true},
                    {"name": "State", "type": "Edm.String"}
                ]
            }
        ]
    })
}

#[tokio::test]
async fn create_index_accepts_collection_of_complex_field() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(collection_complex_definition("hotels")),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "collection-of-complex field rejected: {body}"
    );
    // The type is echoed faithfully so SDK round-trips agree.
    let fields = body["fields"].as_array().cloned().unwrap_or_default();
    let address = fields
        .iter()
        .find(|f| f["name"] == "Address")
        .unwrap_or_else(|| panic!("Address field missing from echo: {body}"));
    assert_eq!(address["type"], "Edm.Collection(Edm.ComplexType)");
}

#[tokio::test]
async fn get_index_echoes_collection_of_complex_field() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(collection_complex_definition("hotels")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let get_uri = format!("/indexes('hotels')?api-version={API_VERSION}");
    let (status, body) = call(app, request("GET", &get_uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::OK);
    let fields = body["fields"].as_array().cloned().unwrap_or_default();
    let address = fields
        .iter()
        .find(|f| f["name"] == "Address")
        .unwrap_or_else(|| panic!("Address field missing from echo: {body}"));
    assert_eq!(address["type"], "Edm.Collection(Edm.ComplexType)");
}

#[tokio::test]
async fn upload_and_search_collection_of_complex_field() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(collection_complex_definition("hotels")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Each document carries an array of complex objects; the searchable
    // `City` subfield is indexed across all elements.
    let (status, body) = call(
        app.clone(),
        upload_request(
            "hotels",
            json!([
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": "1",
                        "HotelName": "Contoso",
                        "Address": [
                            {"City": "Miami", "State": "FL"},
                            {"City": "Seattle", "State": "WA"}
                        ]
                    }
                },
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": "2",
                        "HotelName": "Fabrikam",
                        "Address": [{"City": "Montreal", "State": "QC"}]
                    }
                }
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for item in body["value"].as_array().cloned().unwrap_or_default() {
        assert_eq!(item["status"], true, "upload failed: {item}");
    }

    // A search for a city that appears in a collection element matches.
    let (status, body) = call(app, search_request("hotels", json!({"search": "Seattle"}))).await;
    assert_eq!(status, StatusCode::OK, "search rejected: {body}");
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(1));
    assert_eq!(value[0]["HotelId"], "1");
}

#[tokio::test]
async fn collection_of_complex_rejects_non_array_value() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(collection_complex_definition("hotels")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A single object (not an array) is not a valid collection value.
    let (status, body) = call(
        app,
        upload_request(
            "hotels",
            json!([
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": "1",
                        "Address": {"City": "Miami", "State": "FL"}
                    }
                }
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["value"][0];
    assert_eq!(
        item["status"], false,
        "expected per-document failure: {item}"
    );
}

#[tokio::test]
async fn collection_of_complex_rejects_unknown_subfield() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(collection_complex_definition("hotels")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        upload_request(
            "hotels",
            json!([
                {
                    "@search.action": "upload",
                    "document": {
                        "HotelId": "1",
                        "Address": [{"City": "Miami", "Bogus": "x"}]
                    }
                }
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["value"][0];
    assert_eq!(
        item["status"], false,
        "expected per-document failure: {item}"
    );
}
