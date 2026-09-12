use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::*;

async fn create_custom_index(
    app: &axum::Router,
    definition: serde_json::Value,
) -> (StatusCode, Value) {
    let app = app.clone();
    let uri = format!("/indexes?api-version={API_VERSION}");
    call(app, request("POST", &uri, Some(API_KEY), Some(definition))).await
}

fn ids(body: &Value) -> Vec<String> {
    body["value"]
        .as_array()
        .map(|docs| {
            docs.iter()
                .map(|d| d["id"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// queryType=full (Lucene)
// ---------------------------------------------------------------------------

async fn app_with_docs() -> axum::Router {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure search", "price": 10.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "azure emulators", "price": 20.0}},
                {"@search.action": "upload", "document": {"id": "3", "title": "other", "price": 30.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn full_query_boolean_operators() {
    let app = app_with_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure AND emulators", "queryType": "full"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2"]);

    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure OR other", "queryType": "full"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut matched = ids(&body);
    matched.sort();
    assert_eq!(matched, vec!["1", "2", "3"]);

    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "azure NOT emulators", "queryType": "full"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1"]);
}

#[tokio::test]
async fn full_query_field_scope_fuzzy_wildcard_phrase() {
    let app = app_with_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "title:emulators", "queryType": "full"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2"]);

    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "emul~", "queryType": "full"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2"]);

    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "emul*", "queryType": "full"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2"]);

    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "\"azure search\"", "queryType": "full"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1"]);
}

#[tokio::test]
async fn full_query_invalid_syntax_rejected() {
    let app = app_with_docs().await;
    for search in ["missing:azure", "\"unterminated", "title:[a TO]"] {
        let (status, body) = call(
            app.clone(),
            search_request("items", json!({"search": search, "queryType": "full"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "query {search:?} accepted: {body}"
        );
        assert_eq!(body["error"]["code"], "InvalidQuery");
    }
}

#[tokio::test]
async fn invalid_query_type_rejected() {
    let app = app_with_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "azure", "queryType": "basic"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

// ---------------------------------------------------------------------------
// Per-field analyzers
// ---------------------------------------------------------------------------

fn analyzed_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "code", "type": "Edm.String", "searchable": true, "analyzer": "keyword"},
            {"name": "label", "type": "Edm.String", "searchable": true, "analyzer": "whitespace"}
        ]
    })
}

#[tokio::test]
async fn keyword_field_matches_verbatim_only() {
    let app = app();
    let (status, _) = create_custom_index(&app, analyzed_definition("items")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "first", "code": "ABC-123", "label": "Hello World"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "second", "code": "abc 123", "label": "hello world"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Exact verbatim value matches; sub-tokens and case variants do not.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "ABC-123"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1"]);

    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&body).is_empty());

    // The whitespace-analyzed `label` preserves case: "Hello" matches
    // document 1 only (the English-analyzed `title` would stem/lowercase).
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "Hello", "searchFields": "label"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1"]);
}

#[tokio::test]
async fn analyze_endpoint_supports_new_analyzers() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    for (analyzer, text, expected) in [
        ("keyword", "Hello World", vec!["Hello World"]),
        ("whitespace", "Hello  World", vec!["Hello", "World"]),
        ("alphanum", "Hello-World", vec!["Hello", "World"]),
        ("latin", "Running", vec!["running"]),
        ("chinese", "北京天", vec!["北京", "京天"]),
        ("fr.microsoft", "chevaux", vec!["cheval"]),
    ] {
        let uri = format!("/indexes('items')/search.analyze?api-version={API_VERSION}");
        let (status, body) = call(
            app.clone(),
            request(
                "POST",
                &uri,
                Some(API_KEY),
                Some(json!({"text": text, "analyzer": analyzer})),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "analyzer {analyzer} rejected: {body}"
        );
        let tokens: Vec<String> = body["tokens"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .map(|t| t["token"].as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(tokens, expected, "analyzer {analyzer} tokens");
    }
}

// ---------------------------------------------------------------------------
// Numeric full-text search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn numeric_values_are_full_text_searchable() {
    let app = app();
    let (status, _) = create_custom_index(
        &app,
        json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true},
                {"name": "price", "type": "Edm.Double", "searchable": true, "filterable": true}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "cheap", "price": 100.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "mid", "price": 200.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // "200" appears only in document 2's price.
    let (status, body) = call(app, search_request("items", json!({"search": "200"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2"]);
}

// ---------------------------------------------------------------------------
// Synonym search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn synonym_map_expands_search_terms() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "hotels in Washington"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "flights to Boston"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/synonymmaps?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"name": "m", "format": "solr", "synonyms": "WA, Washington"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // "wa" has no literal match but expands to "washington".
    let (status, body) = call(app, search_request("items", json!({"search": "wa"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1"]);
}

// ---------------------------------------------------------------------------
// Sentence-window highlights
// ---------------------------------------------------------------------------

#[tokio::test]
async fn highlights_return_sentence_windows() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {
                    "id": "1",
                    "title": "Azure is great. Nothing relevant here. Search finds azure twice.",
                    "price": 1.0
                }}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request("items", json!({"search": "azure", "highlight": "title"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let highlights = &body["value"][0]["@search.highlights"]["title"];
    let fragments = highlights.as_array().cloned().unwrap_or_default();
    // Two matching sentences, each its own fragment; the middle sentence is
    // excluded.
    assert_eq!(fragments.len(), 2);
    assert!(fragments[0]
        .as_str()
        .unwrap_or_default()
        .contains("<em>Azure</em>"));
    assert!(fragments[1]
        .as_str()
        .unwrap_or_default()
        .contains("<em>azure</em>"));
}
