use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::*;

fn vector_index_body(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
            {"name": "category", "type": "Edm.String", "filterable": true, "facetable": true},
            {"name": "content_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 3, "vectorSearchProfile": "cos"},
            {"name": "flat_vector", "type": "Collection(Edm.Single)",
             "searchable": true, "retrievable": true,
             "dimensions": 3, "vectorSearchProfile": "eknn"}
        ],
        "vectorSearch": {
            "algorithms": [
                {"name": "hnsw-1", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40, "efSearch": 20, "metric": "cosine"}},
                {"name": "eknn-1", "kind": "exhaustiveKnn",
                 "exhaustiveKnnParameters": {"metric": "cosine"}}
            ],
            "profiles": [
                {"name": "cos", "algorithmConfigurationName": "hnsw-1"},
                {"name": "eknn", "algorithmConfigurationName": "eknn-1"}
            ]
        }
    })
}

async fn create_vector_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    call(
        app.clone(),
        request(
            "POST",
            &format!("/indexes?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(vector_index_body(name)),
        ),
    )
    .await
}

fn vector_documents() -> Value {
    json!([
        {"@search.action": "upload", "document":
            {"id": "1", "title": "azure search", "category": "tech",
             "content_vector": [1.0, 0.0, 0.0], "flat_vector": [1.0, 0.0, 0.0]}},
        {"@search.action": "upload", "document":
            {"id": "2", "title": "azure emulators", "category": "tech",
             "content_vector": [0.0, 1.0, 0.0], "flat_vector": [0.0, 1.0, 0.0]}},
        {"@search.action": "upload", "document":
            {"id": "3", "title": "other", "category": "misc",
             "content_vector": [0.0, 0.0, 1.0], "flat_vector": [0.0, 0.0, 1.0]}},
        {"@search.action": "upload", "document":
            {"id": "4", "title": "azure mixed", "category": "misc",
             "content_vector": [0.7, 0.7, 0.0], "flat_vector": [0.7, 0.7, 0.0]}}
    ])
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

#[tokio::test]
async fn create_index_with_vector_fields_returns_201() {
    let app = app();
    let (status, body) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "vecs");
    assert_eq!(body["fields"][3]["dimensions"], 3);
}

/// Asserts that each `(label, body)` index creation fails with
/// `400 InvalidIndex`.
async fn assert_invalid_indexes(app: &axum::Router, cases: &[(&str, Value)]) {
    for (label, body) in cases {
        let (status, response) = call(
            app.clone(),
            request(
                "POST",
                &format!("/indexes?api-version={API_VERSION}"),
                Some(API_KEY),
                Some(body.clone()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case: {label}");
        assert_eq!(response["error"]["code"], "InvalidIndex", "case: {label}");
    }
}

fn minimal_vector_search() -> Value {
    json!({
        "algorithms": [{"name": "a", "kind": "hnsw"}],
        "profiles": [{"name": "p", "algorithmConfigurationName": "a"}]
    })
}

fn vector_field(name: &str, extra: &Value) -> Value {
    let mut field = serde_json::Map::new();
    field.insert("name".to_owned(), Value::String(name.to_owned()));
    field.insert(
        "type".to_owned(),
        Value::String("Collection(Edm.Single)".to_owned()),
    );
    field.insert("searchable".to_owned(), Value::Bool(true));
    field.insert("dimensions".to_owned(), Value::from(2));
    field.insert(
        "vectorSearchProfile".to_owned(),
        Value::String("p".to_owned()),
    );
    if let Some(overrides) = extra.as_object() {
        for (key, value) in overrides {
            field.insert(key.clone(), value.clone());
        }
    }
    Value::Object(field)
}

fn vector_index_named(name: &str, fields: &Value, vector_search: Option<Value>) -> Value {
    let mut body = json!({"name": name, "fields": fields});
    if let Some(config) = vector_search {
        body["vectorSearch"] = config;
    }
    body
}

#[tokio::test]
async fn create_index_rejects_vector_config_problems() {
    let app = app();
    let key = json!({"name": "id", "type": "Edm.String", "key": true});
    // Each body is expected to fail with `400 InvalidIndex`.
    let mut cases: Vec<(&str, Value)> = vec![
        // Vector fields but no vectorSearch.
        (
            "missing vectorSearch",
            vector_index_named("v1", &json!([key, vector_field("v", &json!({}))]), None),
        ),
        // Unknown profile.
        (
            "unknown profile",
            vector_index_named(
                "v5",
                &json!([
                    key,
                    vector_field("v", &json!({"vectorSearchProfile": "nope"}))
                ]),
                Some(minimal_vector_search()),
            ),
        ),
        // Profile references unknown algorithm.
        (
            "unknown algorithm",
            vector_index_named(
                "v6",
                &json!([key, vector_field("v", &json!({}))]),
                Some(json!({
                    "algorithms": [{"name": "a", "kind": "hnsw"}],
                    "profiles": [{"name": "p", "algorithmConfigurationName": "missing"}]
                })),
            ),
        ),
        // Unknown metric.
        (
            "unknown metric",
            vector_index_named(
                "v10",
                &json!([key, vector_field("v", &json!({}))]),
                Some(json!({
                    "algorithms": [{"name": "a", "kind": "hnsw",
                                    "hnswParameters": {"metric": "manhattan"}}],
                    "profiles": [{"name": "p", "algorithmConfigurationName": "a"}]
                })),
            ),
        ),
    ];
    // More than 16 vector fields.
    let mut many_fields = vec![key];
    for i in 0..17 {
        many_fields.push(vector_field(&format!("v{i}"), &json!({})));
    }
    cases.push((
        "too many vector fields",
        vector_index_named(
            "v11",
            &Value::Array(many_fields),
            Some(minimal_vector_search()),
        ),
    ));
    assert_invalid_indexes(&app, &cases).await;
}

#[tokio::test]
async fn create_index_rejects_vector_field_problems() {
    let app = app();
    let key = json!({"name": "id", "type": "Edm.String", "key": true});
    // Each body is expected to fail with `400 InvalidIndex`.
    let cases: Vec<(&str, Value)> = vec![
        // Missing dimensions.
        (
            "missing dimensions",
            vector_index_named(
                "v2",
                &json!([key,
                    {"name": "v", "type": "Collection(Edm.Single)",
                     "searchable": true, "vectorSearchProfile": "p"}]),
                Some(minimal_vector_search()),
            ),
        ),
        // Zero dimensions.
        (
            "zero dimensions",
            vector_index_named(
                "v3",
                &json!([key, vector_field("v", &json!({"dimensions": 0}))]),
                Some(minimal_vector_search()),
            ),
        ),
        // Non-integer dimensions.
        (
            "string dimensions",
            vector_index_named(
                "v4",
                &json!([key, vector_field("v", &json!({"dimensions": "three"}))]),
                Some(minimal_vector_search()),
            ),
        ),
        // Quantized vector type.
        (
            "quantized type",
            vector_index_named(
                "v7",
                &json!([key,
                    {"name": "v", "type": "Collection(Edm.Half)",
                     "searchable": true, "dimensions": 2, "vectorSearchProfile": "p"}]),
                Some(minimal_vector_search()),
            ),
        ),
        // searchable: false.
        (
            "not searchable",
            vector_index_named(
                "v8",
                &json!([key, vector_field("v", &json!({"searchable": false}))]),
                Some(minimal_vector_search()),
            ),
        ),
        // Vector field marked filterable.
        (
            "filterable vector",
            vector_index_named(
                "v9",
                &json!([key, vector_field("v", &json!({"filterable": true}))]),
                Some(minimal_vector_search()),
            ),
        ),
    ];
    assert_invalid_indexes(&app, &cases).await;
}

#[tokio::test]
async fn upload_validates_vector_documents_per_document() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        app,
        upload_request(
            "vecs",
            json!([
                {"@search.action": "upload", "document":
                    {"id": "ok", "content_vector": [1.0, 2.0, 3.0],
                     "flat_vector": [1.0, 2.0, 3.0]}},
                {"@search.action": "upload", "document":
                    {"id": "short", "content_vector": [1.0, 2.0],
                     "flat_vector": [1.0, 2.0, 3.0]}},
                {"@search.action": "upload", "document":
                    {"id": "nope", "content_vector": [1.0, "x", 3.0],
                     "flat_vector": [1.0, 2.0, 3.0]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let Some(results) = body["value"].as_array() else {
        panic!("expected a batch results array");
    };
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["statusCode"], 201);
    assert_eq!(results[1]["statusCode"], 400);
    assert!(results[1]["errorMessage"]
        .as_str()
        .unwrap_or("")
        .contains("dimension"));
    assert_eq!(results[2]["statusCode"], 400);
}

#[tokio::test]
async fn vector_search_returns_nearest_first_with_scores() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body), vec!["1", "4"]);
    // Scores descend; the exact match scores 1.0 for cosine.
    let scores: Vec<f64> = body["value"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|doc| doc["@search.score"].as_f64().unwrap_or(-1.0))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(scores.len(), 2);
    assert!((scores[0] - 1.0).abs() < 1e-5);
    assert!(scores[0] >= scores[1]);
}

#[tokio::test]
async fn vector_search_fields_accepts_string_and_array() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // Comma-separated string over two fields: union of per-field hits.
    let (status, body) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "vector", "vector": [0.0, 0.0, 1.0],
                     "fields": "content_vector, flat_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body)[0], "3");

    // JSON array of fields.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "vector", "vector": [0.0, 0.0, 1.0],
                     "fields": ["content_vector"], "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body)[0], "3");
}

#[tokio::test]
async fn exhaustive_query_matches_brute_force_field() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let probe = json!([0.6, 0.6, 0.1]);
    let (status, approx) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": probe, "fields": "content_vector", "k": 4}
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, forced) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": probe, "fields": "content_vector",
                 "k": 4, "exhaustive": true}
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, eknn) = call(
        app,
        search_request(
            "vecs",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": probe, "fields": "flat_vector", "k": 4}
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Same data, same metric: all three paths agree on the top hit.
    assert_eq!(doc_ids(&approx)[0], doc_ids(&forced)[0]);
    assert_eq!(doc_ids(&approx)[0], doc_ids(&eknn)[0]);
}

#[tokio::test]
async fn hybrid_search_returns_union_of_both_sides() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // Full-text "azure" matches 1, 2, 4; vector [0,0,1] top-2 matches 3 and
    // a near-zero neighbour. Document 3 matches only the vector side but
    // must still be present (union, not intersection).
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "search": "azure",
                "vectorQueries": [
                    {"kind": "vector", "vector": [0.0, 0.0, 1.0],
                     "fields": "content_vector", "k": 2}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&body);
    assert!(ids.contains(&"1".to_owned()));
    assert!(ids.contains(&"2".to_owned()));
    assert!(ids.contains(&"3".to_owned()));
    assert!(ids.contains(&"4".to_owned()));
}

#[tokio::test]
async fn vector_filter_modes_differ_on_crafted_fixture() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let base = json!({
        "filter": "category eq 'misc'",
        "vectorQueries": [
            {"kind": "vector", "vector": [1.0, 0.0, 0.0],
             "fields": "content_vector", "k": 1}
        ]
    });
    // postFilter (default): top-1 by vector is doc 1 (tech) → filtered out.
    let (status, body) = call(app.clone(), search_request("vecs", base.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc_ids(&body).is_empty());

    // preFilter: candidates are the misc docs first → top-1 is doc 4.
    let mut pre = base.clone();
    pre["vectorFilterMode"] = json!("preFilter");
    let (status, body) = call(app, search_request("vecs", pre)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body), vec!["4"]);
}

#[tokio::test]
async fn multiple_vector_queries_union_with_best_score() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 1},
                    {"kind": "vector", "vector": [0.0, 1.0, 0.0],
                     "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = doc_ids(&body);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"1".to_owned()));
    assert!(ids.contains(&"2".to_owned()));
}

#[tokio::test]
async fn vector_search_paging_and_token_binding() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let query = json!({
        "count": true,
        "top": 1,
        "vectorQueries": [
            {"kind": "vector", "vector": [0.5, 0.5, 0.0],
             "fields": "content_vector", "k": 4}
        ]
    });
    let (status, body) = call(app.clone(), search_request("vecs", query.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["@odata.count"], 4);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    let next = body["@search.nextPageParameters"].clone();
    assert!(next["continuation"].is_string());

    // Re-posting the next-page parameters works.
    let (status, second) = call(app.clone(), search_request("vecs", next.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["value"].as_array().map(Vec::len), Some(1));

    // Changing the vector query mid-paging is rejected.
    let mut changed = next;
    changed["vectorQueries"] = json!([
        {"kind": "vector", "vector": [0.0, 0.0, 1.0], "fields": "content_vector", "k": 4}
    ]);
    let (status, body) = call(app, search_request("vecs", changed)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn vector_search_rejects_bad_queries() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // Each body is expected to fail; the second element is the error code.
    let too_many: Vec<Value> = (0..6)
        .map(|_| {
            json!({"kind": "vector", "vector": [1.0, 0.0, 0.0],
                   "fields": "content_vector", "k": 1})
        })
        .collect();
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "unknown field",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": [1.0, 0.0, 0.0], "fields": "title", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "dimension mismatch",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": [1.0, 0.0], "fields": "content_vector", "k": 1}
            ]}),
            "InvalidQuery",
        ),
        (
            "zero k",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": [1.0, 0.0, 0.0], "fields": "content_vector", "k": 0}
            ]}),
            "InvalidQuery",
        ),
        (
            "k over max",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                 "fields": "content_vector", "k": 1001}
            ]}),
            "InvalidQuery",
        ),
        (
            "too many queries",
            json!({"vectorQueries": too_many}),
            "InvalidQuery",
        ),
        (
            "vectorizer kind",
            json!({"vectorQueries": [
                {"kind": "text", "text": "hello", "fields": "content_vector", "k": 1}
            ]}),
            "UnsupportedQuery",
        ),
        (
            "semantic option",
            json!({"semantic": {"mode": "strict"}}),
            "UnsupportedQuery",
        ),
        (
            "bad filter mode",
            json!({
                "vectorFilterMode": "sideways",
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 1}
                ]
            }),
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
async fn vector_fields_in_select_and_rejected_query_options() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // `select` includes the vector payload.
    let (status, body) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({
                "select": "id,content_vector",
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["id"], "1");
    assert!(body["value"][0]["content_vector"].is_array());

    // Vector fields are rejected in filter/orderby/facets/searchFields.
    let rejected = vec![
        json!({"filter": "content_vector eq 'x'"}),
        json!({"orderby": "content_vector"}),
        json!({"facets": "content_vector"}),
        json!({"search": "azure", "searchFields": "content_vector"}),
    ];
    for body in &rejected {
        let (status, response) = call(app.clone(), search_request("vecs", body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        assert_eq!(response["error"]["code"], "InvalidQuery", "body: {body}");
    }
}

#[tokio::test]
async fn non_retrievable_vector_omitted_unless_selected() {
    let app = app();
    let mut body = vector_index_body("vecs");
    // Mark `content_vector` (fields[3]) non-retrievable.
    body["fields"][3]["retrievable"] = json!(false);
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
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // Without `select`, the non-retrievable vector is omitted but the
    // retrievable one is still returned.
    let (status, body) = call(
        app.clone(),
        search_request(
            "vecs",
            json!({
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["id"], "1");
    assert!(body["value"][0].get("content_vector").is_none());
    assert!(body["value"][0]["flat_vector"].is_array());

    // Explicitly selecting the non-retrievable vector returns it.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "select": "id,content_vector",
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"][0]["id"], "1");
    assert!(body["value"][0]["content_vector"].is_array());
}

#[tokio::test]
async fn sdk_key_aliases_are_accepted() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    // `k_nearest_neighbors` and snake_case `vector_filter_mode`.
    let (status, body) = call(
        app,
        search_request(
            "vecs",
            json!({
                "filter": "category eq 'misc'",
                "vector_filter_mode": "preFilter",
                "vectorQueries": [
                    {"kind": "vector", "vector": [1.0, 0.0, 0.0],
                     "fields": "content_vector", "k_nearest_neighbors": 1}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body), vec!["4"]);
}

#[tokio::test]
async fn dot_product_search_orders_by_raw_inner_product() {
    // `dotProduct` on an `hnsw` algorithm executes as an exact scan (raw
    // inner products are unrepresentable as non-negative HNSW distances):
    // ordering holds for negative dots and large unnormalized magnitudes.
    let app = app();
    let body = json!({
        "name": "dots",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "v", "type": "Collection(Edm.Single)",
             "searchable": true, "dimensions": 2, "vectorSearchProfile": "dot"}
        ],
        "vectorSearch": {
            "algorithms": [
                {"name": "hnsw-dot", "kind": "hnsw",
                 "hnswParameters": {"m": 4, "efConstruction": 40,
                                    "efSearch": 20, "metric": "dotProduct"}}
            ],
            "profiles": [
                {"name": "dot", "algorithmConfigurationName": "hnsw-dot"}
            ]
        }
    });
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
    let (status, _) = call(
        app.clone(),
        upload_request(
            "dots",
            json!([
                {"@search.action": "upload", "document": {"id": "neg", "v": [-3.0, 0.0]}},
                {"@search.action": "upload", "document": {"id": "zero", "v": [0.0, 0.0]}},
                {"@search.action": "upload", "document": {"id": "big", "v": [100.0, 100.0]}},
                {"@search.action": "upload", "document": {"id": "unit", "v": [1.0, 0.0]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request(
            "dots",
            json!({"vectorQueries": [
                {"kind": "vector", "vector": [1.0, 1.0], "fields": "v", "k": 4}
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc_ids(&body), vec!["big", "unit", "zero", "neg"]);
    assert_eq!(body["value"][0]["@search.score"], 200.0);
}

#[tokio::test]
async fn deleted_documents_never_match_vector_search() {
    let app = app();
    let (status, _) = create_vector_index(&app, "vecs").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("vecs", vector_documents())).await;
    assert_eq!(status, StatusCode::OK);

    let probe = json!({"vectorQueries": [
        {"kind": "vector", "vector": [1.0, 0.0, 0.0], "fields": "content_vector", "k": 4}
    ]});
    let (status, body) = call(app.clone(), search_request("vecs", probe.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc_ids(&body).contains(&"1".to_owned()));

    let (status, _) = call(
        app.clone(),
        upload_request(
            "vecs",
            json!([{"@search.action": "delete", "document": {"id": "1"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(app, search_request("vecs", probe)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!doc_ids(&body).contains(&"1".to_owned()));
}
