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
