//! Contract tests for vectorizer queries (Phase 2.4): the `vectorizers` index
//! configuration, vector generation at document-index time, and `kind: "text"`
//! vector queries.

use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::*;

/// The vectorizer configuration shared by the test indexes: a `uri` vectorizer
/// named `embedder` whose text source is `title` + `content`.
fn vectorizers() -> Value {
    json!([
        {
            "name": "embedder",
            "kind": "uri",
            "parameters": {"uri": "https://example.com/embed"},
            "sourceContext": {"sourceType": "field", "fields": ["title", "content"]}
        }
    ])
}

/// A vector index with a vectorizer-backed field (`content_vector`) and a
/// raw-vector field (`raw_vector`, no vectorizer).
fn vectorizer_index_body(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
            {"name": "content", "type": "Edm.String", "searchable": true},
            {"name": "category", "type": "Edm.String", "filterable": true, "facetable": true},
            {"name": "content_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 64, "vectorSearchProfile": "cos", "vectorizer": "embedder"},
            {"name": "raw_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 64, "vectorSearchProfile": "cos"}
        ],
        "vectorizers": vectorizers(),
        "vectorSearch": {
            "algorithms": [
                {"name": "hnsw-1", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40, "efSearch": 20, "metric": "cosine"}}
            ],
            "profiles": [
                {"name": "cos", "algorithmConfigurationName": "hnsw-1"}
            ]
        }
    })
}

async fn create_vectorizer_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(vectorizer_index_body(name)),
        ),
    )
    .await
}

fn get_document_request(name: &str, key: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/docs('{key}')?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

fn doc_ids(body: &Value) -> Vec<String> {
    body["value"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|doc| doc["id"].as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Documents whose text shares tokens with the query "quantum computing":
/// doc 1 shares both tokens, doc 3 shares "quantum", doc 2 shares none.
fn vectorizer_documents() -> Value {
    json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "quantum computing applications",
             "category": "tech"}},
        {"@search.action": "upload", "document":
            {"id": "2", "title": "classical physics", "content": "classical mechanics",
             "category": "tech"}},
        {"@search.action": "upload", "document":
            {"id": "3", "title": "quantum mechanics", "content": "quantum physics",
             "category": "misc"}}
    ])
}

#[tokio::test]
async fn create_index_with_vectorizer_returns_201() {
    let app = app();
    let (status, body) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "vecs");
    // The vectorizers array is echoed.
    assert_eq!(body["vectorizers"][0]["name"], "embedder");
    // The field's vectorizer reference is echoed.
    let fields = body["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("no fields"));
    let content_vector = fields
        .iter()
        .find(|f| f["name"] == "content_vector")
        .unwrap_or_else(|| panic!("no content_vector"));
    assert_eq!(content_vector["vectorizer"], "embedder");
}

#[tokio::test]
async fn create_index_rejects_vectorizer_problems() {
    let app = app();
    let key = json!({"name": "id", "type": "Edm.String", "key": true});
    let title = json!({"name": "title", "type": "Edm.String", "searchable": true});
    let vector_field = json!({
        "name": "v", "type": "Collection(Edm.Single)", "searchable": true,
        "dimensions": 4, "vectorSearchProfile": "p", "vectorizer": "embedder"
    });
    let vector_search = json!({
        "algorithms": [{"name": "a", "kind": "hnsw"}],
        "profiles": [{"name": "p", "algorithmConfigurationName": "a"}]
    });
    let body = |vectorizers: Value, fields: Value| -> Value {
        let mut b = json!({"name": "vz", "fields": fields, "vectorSearch": vector_search});
        b["vectorizers"] = vectorizers;
        b
    };
    let cases: Vec<(&str, Value)> = vec![
        (
            "missing kind",
            body(
                json!([{"name": "embedder", "parameters": {"uri": "u"}}]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "unsupported kind",
            body(
                json!([{"name": "embedder", "kind": "none", "parameters": {"uri": "u"}}]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "missing parameters.uri",
            body(
                json!([{"name": "embedder", "kind": "uri"}]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "duplicate name",
            body(
                json!([
                    {"name": "embedder", "kind": "uri", "parameters": {"uri": "u"}},
                    {"name": "embedder", "kind": "uri", "parameters": {"uri": "u"}}
                ]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "sourceContext unknown field",
            body(
                json!([{
                    "name": "embedder", "kind": "uri", "parameters": {"uri": "u"},
                    "sourceContext": {"sourceType": "field", "fields": ["missing"]}
                }]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "sourceContext non-searchable field",
            body(
                json!([{
                    "name": "embedder", "kind": "uri", "parameters": {"uri": "u"},
                    "sourceContext": {"sourceType": "field", "fields": ["id"]}
                }]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "bad sourceType",
            body(
                json!([{
                    "name": "embedder", "kind": "uri", "parameters": {"uri": "u"},
                    "sourceContext": {"sourceType": "document", "fields": ["title"]}
                }]),
                json!([key, title, vector_field.clone()]),
            ),
        ),
        (
            "field references unknown vectorizer",
            body(
                json!([{"name": "other", "kind": "uri", "parameters": {"uri": "u"}}]),
                json!([key, title, vector_field]),
            ),
        ),
    ];
    for (label, b) in &cases {
        let (status, response) = call(
            app.clone(),
            request(
                "POST",
                &format!("/indexes?api-version={API_VERSION}"),
                Some(API_KEY),
                Some(b.clone()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case: {label}");
        assert_eq!(response["error"]["code"], "InvalidIndex", "case: {label}");
    }
}

#[tokio::test]
async fn upload_without_vector_generates_and_stores_vector() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    // Every document succeeded.
    for item in body["value"]
        .as_array()
        .unwrap_or_else(|| panic!("no results"))
    {
        assert_eq!(item["status"], true, "doc {}", item["key"]);
    }
    // The generated vector is stored and returned by get_document.
    let (status, doc) = call(app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    let vector = doc["content_vector"]
        .as_array()
        .unwrap_or_else(|| panic!("content_vector present"));
    assert_eq!(vector.len(), 64);
    // A non-empty source text yields a non-zero vector.
    assert!(vector.iter().any(|v| v.as_f64() != Some(0.0)));
}

#[tokio::test]
async fn upload_with_explicit_vector_uses_it() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let explicit: Vec<Value> = (0..64)
        .map(|i| json!(if i == 0 { 1.0 } else { 0.0 }))
        .collect();
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "applications",
             "category": "tech", "content_vector": explicit}}
    ]);
    let (status, body) = call(app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], true);
    let (status, doc) = call(app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    // The explicit vector is stored verbatim (not overwritten by generation).
    let stored = doc["content_vector"]
        .as_array()
        .unwrap_or_else(|| panic!("content_vector present"));
    assert_eq!(stored[0].as_f64(), Some(1.0));
    assert_eq!(stored[1].as_f64(), Some(0.0));
}

#[tokio::test]
async fn upload_without_vector_on_raw_field_omits_it() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    // `raw_vector` has no vectorizer; omitting it is accepted (no vector).
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "hello", "content": "world", "category": "tech"}}
    ]);
    let (status, body) = call(app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["status"], true);
    let (status, doc) = call(app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc.get("raw_vector").is_none());
    // The vectorizer-backed field was still generated.
    assert!(doc["content_vector"].as_array().is_some());
}

#[tokio::test]
async fn text_query_orders_by_token_overlap() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&body);
    // Doc 1 shares both query tokens; doc 3 shares "quantum"; doc 2 shares none.
    assert_eq!(ids[0], "1");
    assert!(ids.iter().position(|id| id == "3").is_some());
    let pos3 = ids
        .iter()
        .position(|id| id == "3")
        .unwrap_or_else(|| panic!("doc 3 missing"));
    let pos2 = ids
        .iter()
        .position(|id| id == "2")
        .unwrap_or_else(|| panic!("doc 2 missing"));
    assert!(
        pos3 < pos2,
        "doc 3 (shared token) should rank above doc 2: {ids:?}"
    );
}

#[tokio::test]
async fn text_query_respects_k() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body).len(), 2);
}

#[tokio::test]
async fn text_query_rejects_bad_requests() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "missing text",
            json!({"vectorQueries": [
                {"kind": "text", "fields": "content_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "empty text",
            json!({"vectorQueries": [
                {"kind": "text", "text": "", "fields": "content_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "vector also present",
            json!({"vectorQueries": [
                {"kind": "text", "text": "hello", "vector": [1.0], "fields": "content_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "field without vectorizer",
            json!({"vectorQueries": [
                {"kind": "text", "text": "hello", "fields": "raw_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "unknown kind",
            json!({"vectorQueries": [
                {"kind": "image", "text": "hello", "fields": "content_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
    ];
    for (label, body, code) in &cases {
        let (status, response) = call(app.clone(), search_request("vecs", body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case: {label}");
        assert_eq!(response["error"]["code"], *code, "case: {label}");
    }
}

#[tokio::test]
async fn text_and_vector_queries_union() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    // Doc 1 matches the text query; doc 2's raw vector matches the vector query.
    let query_vector: Vec<Value> = (0..64)
        .map(|i| json!(if i == 1 { 1.0 } else { 0.0 }))
        .collect();
    let explicit2 = query_vector.clone();
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "applications", "category": "tech"}},
        {"@search.action": "upload", "document":
            {"id": "2", "title": "unrelated", "content": "nothing", "category": "tech",
             "raw_vector": explicit2}}
    ]);
    let (status, _) = call(app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 1},
                    {"kind": "vector", "vector": query_vector, "fields": "raw_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The union contains both the text match (1) and the vector match (2).
    let ids: Vec<String> = doc_ids(&body);
    assert!(ids.contains(&"1".to_owned()), "expected doc 1: {ids:?}");
    assert!(ids.contains(&"2".to_owned()), "expected doc 2: {ids:?}");
}

#[tokio::test]
async fn text_query_hybrid_fuses_with_full_text() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    // Full-text "quantum" matches docs 1 and 3; the text query also ranks 1 and 3.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "search": "quantum",
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&body);
    // Doc 1 matches both sides, so it ranks first under RRF fusion.
    assert_eq!(ids[0], "1");
    assert!(ids.contains(&"3".to_owned()), "expected doc 3: {ids:?}");
}

#[tokio::test]
async fn text_query_prefilter_constrains_candidates() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    // preFilter: only misc docs (doc 3) are candidates.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "filter": "category eq 'misc'",
                "vectorFilterMode": "preFilter",
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&body);
    assert_eq!(ids, vec!["3".to_owned()]);
}

#[tokio::test]
async fn text_query_exhaustive_uses_brute_force() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector",
                     "k": 3, "exhaustive": true}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body)[0], "1");
}

#[tokio::test]
async fn multiple_text_queries_union_best_score() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 1},
                    {"kind": "text", "text": "classical physics", "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The union contains the top match of each query (1 and 2).
    let ids: Vec<String> = doc_ids(&body);
    assert!(ids.contains(&"1".to_owned()), "expected doc 1: {ids:?}");
    assert!(ids.contains(&"2".to_owned()), "expected doc 2: {ids:?}");
}

#[tokio::test]
async fn source_context_fields_determine_text_source() {
    let app = app();
    // A vectorizer whose source is only `content` (not `title`).
    let mut body = vectorizer_index_body("vecs");
    body["vectorizers"] = json!([
        {
            "name": "embedder",
            "kind": "uri",
            "parameters": {"uri": "https://example.com/embed"},
            "sourceContext": {"sourceType": "field", "fields": ["content"]}
        }
    ]);
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Doc 1's title is "quantum computing" but its content is "applications";
    // a query for "quantum" should NOT match doc 1 (title is not the source).
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "applications", "category": "tech"}},
        {"@search.action": "upload", "document":
            {"id": "2", "title": "unrelated", "content": "quantum research", "category": "tech"}}
    ]);
    let (status, _) = call(app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, found) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum research", "fields": "content_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Doc 2's content shares "quantum research"; doc 1's content ("applications") does not.
    assert_eq!(doc_ids(&found)[0], "2");
}

#[tokio::test]
async fn zero_vector_document_does_not_match() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    // Doc 1 has empty source text (no title/content) → zero vector.
    let docs = json!([
        {"@search.action": "upload", "document": {"id": "1", "category": "tech"}},
        {"@search.action": "upload", "document":
            {"id": "2", "title": "quantum computing", "content": "applications", "category": "tech"}}
    ]);
    let (status, _) = call(app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The zero-vector doc 1 has zero cosine similarity and does not appear.
    let ids = doc_ids(&body);
    assert!(
        !ids.contains(&"1".to_owned()),
        "zero-vector doc should not match: {ids:?}"
    );
    assert!(ids.contains(&"2".to_owned()), "expected doc 2: {ids:?}");
}

#[tokio::test]
async fn text_query_paging_and_continuation() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let query = json!({
        "vectorQueries": [
            {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
        ],
        "top": 2
    });
    let (status, first) = call(app.clone(), search_request("vecs", query.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&first).len(), 2);
    // Follow the continuation token (resending the same vectorQueries).
    let next = first["@search.nextPageParameters"].clone();
    let (status, second) = call(app, search_request("vecs", next)).await;
    assert_eq!(status, StatusCode::OK);
    let all = {
        let mut ids = doc_ids(&first);
        ids.extend(doc_ids(&second));
        ids
    };
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|id| !id.is_empty()));
}

#[tokio::test]
async fn text_query_select_projection() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    // Without select, the retrievable vector field is returned.
    let (status, body) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["value"][0]["content_vector"].as_array().is_some());
    // With a select that excludes the vector, it is omitted.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "select": "id,title",
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["value"][0].get("content_vector").is_none());
    assert_eq!(body["value"][0]["id"], "1");
}

#[tokio::test]
async fn text_query_composes_with_orderby_facets_select_filter() {
    let app = app();
    let (status, _) = create_vectorizer_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "search": "quantum",
                "filter": "category eq 'tech' or category eq 'misc'",
                "orderby": "id asc",
                "select": "id,title",
                "facets": ["category"],
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // orderby wins over vector rank: key order.
    assert_eq!(doc_ids(&body), vec!["1", "2", "3"]);
    // Facets are computed over the matched set; select projects.
    assert!(body["@search.facets"]["category"].as_array().is_some());
    assert!(body["value"][0].get("content_vector").is_none());
    assert!(body["value"][0]["title"].as_str().is_some());
}

#[tokio::test]
async fn profile_level_vectorizer_is_the_sdk_wire_format() {
    // The pinned SDKs associate the vectorizer with a field through the
    // profile's `vectorizer` property (not a field-level property). Separate
    // profiles keep `raw_vector` vectorizer-free.
    let body = json!({
        "name": "vecs",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
            {"name": "content", "type": "Edm.String", "searchable": true},
            {"name": "category", "type": "Edm.String", "filterable": true, "facetable": true},
            {"name": "content_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 64, "vectorSearchProfile": "with_vz"},
            {"name": "raw_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 64, "vectorSearchProfile": "no_vz"}
        ],
        "vectorizers": vectorizers(),
        "vectorSearch": {
            "algorithms": [
                {"name": "hnsw-1", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40, "efSearch": 20, "metric": "cosine"}}
            ],
            "profiles": [
                {"name": "with_vz", "algorithmConfigurationName": "hnsw-1", "vectorizer": "embedder"},
                {"name": "no_vz", "algorithmConfigurationName": "hnsw-1"}
            ]
        }
    });
    let app = app();
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, upload_body) =
        call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    for item in upload_body["value"]
        .as_array()
        .unwrap_or_else(|| panic!("no results"))
    {
        assert_eq!(
            item["status"], true,
            "doc {}: {}",
            item["key"], item["errorMessage"]
        );
    }
    // The profile-level vectorizer generates the vector for content_vector.
    let (status, doc) = call(app.clone(), get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc["content_vector"].as_array().is_some());
    // A kind:text query resolves the vectorizer through the profile.
    let (status, found) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "text", "text": "quantum computing", "fields": "content_vector", "k": 3}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&found)[0], "1");
}

#[tokio::test]
async fn nested_vector_search_vectorizers_is_the_sdk_wire_format() {
    // The pinned SDKs nest `vectorizers` inside `vectorSearch` (rather than
    // as a top-level index property).
    let mut body = vectorizer_index_body("vecs");
    let vectorizers = body
        .as_object_mut()
        .and_then(|obj| obj.remove("vectorizers"))
        .unwrap_or_else(|| panic!("test index has vectorizers"));
    body["vectorSearch"]["vectorizers"] = vectorizers;
    let app = app();
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, upload) = call(app.clone(), upload_request("vecs", vectorizer_documents())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upload["value"][0]["status"], true);
    let (status, doc) = call(app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc["content_vector"].as_array().is_some());
}

#[tokio::test]
async fn profile_referencing_unknown_vectorizer_rejected() {
    let body = json!({
        "name": "vecs",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true},
            {"name": "v", "type": "Collection(Edm.Single)",
             "searchable": true, "dimensions": 4, "vectorSearchProfile": "p"}
        ],
        "vectorizers": [
            {"name": "embedder", "kind": "uri", "parameters": {"uri": "u"}}
        ],
        "vectorSearch": {
            "algorithms": [{"name": "a", "kind": "hnsw"}],
            "profiles": [{"name": "p", "algorithmConfigurationName": "a", "vectorizer": "missing"}]
        }
    });
    let app = app();
    let (status, response) = call(
        app,
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

/// A `null` value for a vectorizer-backed vector field is treated as omitted
/// (a vector is generated from the source text), whether the vectorizer is
/// associated through a field-level `vectorizer` property or the field's
/// profile (the pinned SDKs' wire format).
#[tokio::test]
async fn null_vector_on_vectorizer_field_generates_vector() {
    // Field-level vectorizer (the default test index).
    let field_app = app();
    let (status, _) = create_vectorizer_index(&field_app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "applications",
             "category": "tech", "content_vector": null}}
    ]);
    let (status, body) = call(field_app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["value"][0]["status"], true,
        "field-level: {}",
        body["value"][0]["errorMessage"]
    );
    let (status, doc) = call(field_app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    let vector = doc["content_vector"]
        .as_array()
        .unwrap_or_else(|| panic!("content_vector present"));
    assert_eq!(vector.len(), 64);
    assert!(vector.iter().any(|v| v.as_f64() != Some(0.0)));

    // Profile-level vectorizer (the pinned SDKs' wire format).
    let profile_body = json!({
        "name": "vecs",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
            {"name": "content", "type": "Edm.String", "searchable": true},
            {"name": "content_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 64, "vectorSearchProfile": "with_vz"}
        ],
        "vectorizers": vectorizers(),
        "vectorSearch": {
            "algorithms": [
                {"name": "hnsw-1", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40, "efSearch": 20, "metric": "cosine"}}
            ],
            "profiles": [
                {"name": "with_vz", "algorithmConfigurationName": "hnsw-1", "vectorizer": "embedder"}
            ]
        }
    });
    let profile_app = app();
    let (status, _) = call(
        profile_app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(profile_body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let docs = json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "quantum computing", "content": "applications",
             "content_vector": null}}
    ]);
    let (status, upload) = call(profile_app.clone(), upload_request("vecs", docs)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upload["value"][0]["status"], true,
        "profile-level: {}",
        upload["value"][0]["errorMessage"]
    );
    let (status, doc) = call(profile_app, get_document_request("vecs", "1")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc["content_vector"].as_array().is_some());
}
