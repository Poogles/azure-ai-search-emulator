use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::*;

fn suggester_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "tags", "type": "Edm.Collection(Edm.String)", "searchable": true}
        ],
        "suggesters": [
            {"name": "sg", "searchFields": ["title", "tags"]}
        ]
    })
}

async fn create_suggester_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    let app = app.clone();
    let uri = format!("/indexes?api-version={API_VERSION}");
    call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(suggester_definition(name)),
        ),
    )
    .await
}

async fn seed(app: &axum::Router) {
    let (status, body) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "Boston Harbor Hotel", "tags": ["spa"]}},
                {"@search.action": "upload", "document": {"id": "2", "title": "Portland Airport Inn", "tags": ["boston", "wifi"]}},
                {"@search.action": "upload", "document": {"id": "3", "title": "Seattle Downtown", "tags": ["wifi"]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "seed upload failed: {body}");
}

fn autocomplete_request(name: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/docs/search.post.autocomplete?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(body))
}

fn suggest_request(name: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/docs/search.post.suggest?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(body))
}

#[tokio::test]
async fn create_index_accepts_suggester() {
    let app = app();
    let (status, body) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED, "suggester rejected: {body}");
}

#[tokio::test]
async fn create_index_accepts_source_fields_key() {
    let app = app();
    let definition = json!({
        "name": "items",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true}
        ],
        "suggesters": [
            {"name": "sg", "searchMode": "analyzingInfixMatching", "sourceFields": ["title"]}
        ]
    });
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, body) = call(app, request("POST", &uri, Some(API_KEY), Some(definition))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "sourceFields suggester rejected: {body}"
    );
}

#[tokio::test]
async fn create_index_rejects_suggester_unknown_field() {
    let app = app();
    let definition = json!({
        "name": "items",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true}
        ],
        "suggesters": [
            {"name": "sg", "searchFields": ["missing"]}
        ]
    });
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, body) = call(app, request("POST", &uri, Some(API_KEY), Some(definition))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn create_index_rejects_suggester_non_searchable_field() {
    let app = app();
    let definition = json!({
        "name": "items",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "filterable": true}
        ],
        "suggesters": [
            {"name": "sg", "searchFields": ["title"]}
        ]
    });
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, body) = call(app, request("POST", &uri, Some(API_KEY), Some(definition))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn autocomplete_returns_prefix_matches() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        autocomplete_request("items", json!({"search": "bos", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "autocomplete failed: {body}");
    let Some(value) = body["value"].as_array() else {
        panic!("expected value array: {body}");
    };
    let texts: Vec<&str> = value
        .iter()
        .map(|v| match v["text"].as_str() {
            Some(text) => text,
            None => panic!("expected text string: {v}"),
        })
        .collect();
    assert_eq!(texts, vec!["Boston", "boston"]);
    assert_eq!(value[0]["queryPlusText"], "bos Boston");
}

#[tokio::test]
async fn autocomplete_is_case_insensitive() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        autocomplete_request("items", json!({"search": "BOS", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn autocomplete_top_limits_results() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        autocomplete_request(
            "items",
            json!({"search": "bos", "suggesterName": "sg", "top": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn autocomplete_no_match_returns_empty_value() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        autocomplete_request("items", json!({"search": "zzz", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn suggest_returns_matching_documents_with_text() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        suggest_request("items", json!({"search": "bos", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "suggest failed: {body}");
    let Some(value) = body["value"].as_array() else {
        panic!("expected value array: {body}");
    };
    assert_eq!(value.len(), 2);
    // Documents come back in key order, each with the matched word and full fields.
    assert_eq!(value[0]["id"], "1");
    assert_eq!(value[0]["@search.text"], "Boston");
    assert_eq!(value[0]["title"], "Boston Harbor Hotel");
    assert_eq!(value[1]["id"], "2");
    assert_eq!(value[1]["@search.text"], "boston");
}

#[tokio::test]
async fn suggest_top_limits_results() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        suggest_request(
            "items",
            json!({"search": "bos", "suggesterName": "sg", "top": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let Some(value) = body["value"].as_array() else {
        panic!("expected value array: {body}");
    };
    assert_eq!(value.len(), 1);
    assert_eq!(value[0]["id"], "1");
}

#[tokio::test]
async fn suggest_no_match_returns_empty_value() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        suggest_request("items", json!({"search": "zzz", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn autocomplete_unknown_suggester_returns_400() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        autocomplete_request("items", json!({"search": "bos", "suggesterName": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn suggest_unknown_suggester_returns_400() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    let (status, body) = call(
        app,
        suggest_request("items", json!({"search": "bos", "suggesterName": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn autocomplete_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(
        app,
        autocomplete_request("missing", json!({"search": "bos", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn autocomplete_missing_search_returns_400() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        autocomplete_request("items", json!({"suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn suggest_missing_suggester_name_returns_400() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, suggest_request("items", json!({"search": "bos"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn suggest_rejects_non_positive_top() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    for body in [
        json!({"search": "bos", "suggesterName": "sg", "top": 0}),
        json!({"search": "bos", "suggesterName": "sg", "top": -1}),
        json!({"search": "bos", "suggesterName": "sg", "top": "many"}),
    ] {
        let (status, body) = call(app.clone(), suggest_request("items", body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "InvalidQuery");
    }
}

#[tokio::test]
async fn autocomplete_rejects_bad_top_query_param() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let uri =
        format!("/indexes('items')/docs/search.post.autocomplete?api-version={API_VERSION}&top=0");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"search": "bos", "suggesterName": "sg"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn autocomplete_requires_auth() {
    let app = app();
    let uri = format!("/indexes('items')/docs/search.post.autocomplete?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"search": "bos", "suggesterName": "sg"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn autocomplete_matches_infix() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    // "osto" is an infix (not a prefix) of "Boston"/"boston".
    let (status, body) = call(
        app.clone(),
        autocomplete_request("items", json!({"search": "osto", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let texts: Vec<&str> = body["value"]
        .as_array()
        .map(|items| items.iter().filter_map(|c| c["text"].as_str()).collect())
        .unwrap_or_default();
    assert!(texts.contains(&"Boston"), "expected Boston in {texts:?}");
    assert!(texts.contains(&"boston"), "expected boston in {texts:?}");
}

#[tokio::test]
async fn suggest_matches_infix() {
    let app = app();
    let (status, _) = create_suggester_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    seed(&app).await;

    // "rbor" is an infix of "Harbor" (doc 1).
    let (status, body) = call(
        app,
        suggest_request("items", json!({"search": "rbor", "suggesterName": "sg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec!["1"]);
}
