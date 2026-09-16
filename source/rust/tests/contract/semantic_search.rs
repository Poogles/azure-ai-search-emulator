//! Contract tests for semantic search: index schema validation, query
//! parsing (nested and flat SDK formats), answer/caption extraction,
//! reranker scores, and error cases.

use axum::http::StatusCode;
use serde_json::{json, Value};

use super::common::{call, create_index, search_request, upload_request, API_KEY, API_VERSION};

fn must_array<'a>(value: &'a Value, label: &str) -> &'a [Value] {
    match value.as_array() {
        Some(arr) => arr,
        None => panic!("expected {label} to be an array"),
    }
}

fn must_find<'a>(docs: &'a [Value], id: &str, label: &str) -> &'a Value {
    match docs.iter().find(|d| d["id"] == id) {
        Some(doc) => doc,
        None => panic!("expected {label} with id {id}"),
    }
}

fn must_f64(value: &Value, label: &str) -> f64 {
    match value.as_f64() {
        Some(f) => f,
        None => panic!("expected {label} to be a number"),
    }
}

/// A semantic index in the SDK wire format (`prioritizedFields`).
fn semantic_index_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "summary", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "keywords", "type": "Edm.Collection(Edm.String)", "searchable": true, "retrievable": true},
            {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true, "facetable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "default",
                "prioritizedFields": {
                    "titleField": {"fieldName": "title"},
                    "prioritizedContentFields": [{"fieldName": "content"}],
                    "prioritizedKeywordsFields": [{"fieldName": "keywords"}]
                },
                "rankingOrder": "BoostedRerankerScore"
            }]
        }
    })
}

/// A semantic index in the canonical wire format (`priorities` / `sources`).
fn canonical_semantic_index_definition(name: &str) -> Value {
    json!({
        "name": name,
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "summary", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "default",
                "priorities": {
                    "answers": ["title", "content"],
                    "captions": ["summary", "content"]
                },
                "sources": [
                    {"name": "content-source", "type": "text", "field": "content"},
                    {"name": "title-source", "type": "text", "field": "title"}
                ],
                "reranker": {"name": "standard"},
                "rescorers": []
            }]
        }
    })
}

async fn create_semantic_index(app: &axum::Router, name: &str) -> (StatusCode, Value) {
    let uri = format!("/indexes('{name}')?api-version={API_VERSION}");
    let req = super::common::request(
        "PUT",
        &uri,
        Some(API_KEY),
        Some(semantic_index_definition(name)),
    );
    call(app.clone(), req).await
}

fn semantic_documents() -> Value {
    json!({
        "value": [
            {
                "@search.action": "upload",
                "id": "1",
                "title": "Azure AI Search Overview",
                "content": "Azure AI Search is a cloud service. It provides rich text search capabilities. It also supports vector search. The service is highly available.",
                "summary": "An overview of Azure AI Search capabilities and features.",
                "keywords": ["azure", "search"],
                "price": 10.0
            },
            {
                "@search.action": "upload",
                "id": "2",
                "title": "Vector Search Guide",
                "content": "Vector search uses embeddings. Neural networks create the embeddings. The search finds similar documents. This is useful for semantic matching.",
                "summary": "A guide to vector search and embeddings.",
                "keywords": ["vector", "embeddings"],
                "price": 20.0
            },
            {
                "@search.action": "upload",
                "id": "3",
                "title": "Filtering in Search",
                "content": "Filters narrow search results. You can filter by price. You can filter by category. Filters use OData syntax.",
                "summary": "How to use filters to narrow search results.",
                "keywords": ["filters", "odata"],
                "price": 30.0
            }
        ]
    })
}

async fn setup_semantic(app: &axum::Router) {
    let (status, _) = create_semantic_index(app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), upload_request("sem", semantic_documents())).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Index schema validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_index_creation_succeeds() {
    let app = super::common::app();
    let (status, body) = create_semantic_index(&app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "sem");
    assert!(body["semantic"]["configurations"].is_array());
}

#[tokio::test]
async fn canonical_semantic_index_creation_succeeds() {
    let app = super::common::app();
    let uri = format!("/indexes('sem')?api-version={API_VERSION}");
    let req = super::common::request(
        "PUT",
        &uri,
        Some(API_KEY),
        Some(canonical_semantic_index_definition("sem")),
    );
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "sem");
}

#[tokio::test]
async fn semantic_index_missing_configurations_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true}
        ],
        "semantic": {}
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_empty_configurations_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true}
        ],
        "semantic": {"configurations": []}
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_duplicate_configuration_names_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [
                {"name": "a", "prioritizedFields": {"prioritizedContentFields": [{"fieldName": "content"}]}},
                {"name": "a", "prioritizedFields": {"prioritizedContentFields": [{"fieldName": "content"}]}}
            ]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_non_searchable_source_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": false, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "priorities": {"answers": ["content"]},
                "sources": [{"name": "s", "type": "text", "field": "content"}]
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_non_retrievable_priority_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": false}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "priorities": {"answers": ["content"]}
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_unknown_field_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "prioritizedFields": {"prioritizedContentFields": [{"fieldName": "missing"}]}
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_bad_reranker_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "priorities": {"answers": ["content"]},
                "reranker": {"name": "neural"}
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_non_empty_rescorers_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "priorities": {"answers": ["content"]},
                "rescorers": [{"name": "my-rescorer"}]
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

#[tokio::test]
async fn semantic_index_non_text_source_rejected() {
    let app = super::common::app();
    let body = json!({
        "name": "bad",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "content", "type": "Edm.String", "searchable": true, "retrievable": true}
        ],
        "semantic": {
            "configurations": [{
                "name": "a",
                "priorities": {"answers": ["content"]},
                "sources": [{"name": "s", "type": "image", "field": "content"}]
            }]
        }
    });
    let uri = format!("/indexes('bad')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, response) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], "InvalidIndex");
}

// ---------------------------------------------------------------------------
// Query validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_without_semantic_config_on_index_rejected() {
    let app = super::common::app();
    let (status, _) = create_index(&app, "plain").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "plain",
            json!({"value": [{"@search.action": "upload", "id": "1", "title": "test", "price": 1.0}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default"}
    });
    let (status, body) = call(app, search_request("plain", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn semantic_search_unknown_configuration_rejected() {
    let app = super::common::app();
    let (status, _) = create_semantic_index(&app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);

    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "nonexistent"}
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn semantic_with_vector_queries_rejected() {
    let app = super::common::app();
    // An index with both a semantic configuration and a real vector field, so
    // the rejection is the semantic/vector conflict (not an unknown field).
    let body = json!({
        "name": "sem",
        "fields": [
            {"name": "id", "type": "Edm.String", "key": true},
            {"name": "title", "type": "Edm.String", "searchable": true, "retrievable": true},
            {"name": "vec", "type": "Edm.Collection(Edm.Single)", "searchable": true, "dimensions": 2, "vectorSearchProfile": "cos"}
        ],
        "vectorSearch": {
            "algorithms": [{"name": "hnsw-1", "kind": "hnsw", "hnswParameters": {"m": 4, "efConstruction": 40, "efSearch": 20, "metric": "cosine"}}],
            "profiles": [{"name": "cos", "algorithmConfigurationName": "hnsw-1"}]
        },
        "semantic": {
            "configurations": [{
                "name": "default",
                "prioritizedFields": {"titleField": {"fieldName": "title"}}
            }]
        }
    });
    let uri = format!("/indexes('sem')?api-version={API_VERSION}");
    let req = super::common::request("PUT", &uri, Some(API_KEY), Some(body));
    let (status, _) = call(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);

    let query = json!({
        "search": "test",
        "queryType": "semantic",
        "semanticConfiguration": "default",
        "vectorQueries": [
            {"kind": "vector", "vector": [1.0, 0.0], "fields": "vec", "k": 1}
        ]
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn semantic_with_full_query_type_rejected() {
    let app = super::common::app();
    let (status, _) = create_semantic_index(&app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);

    let query = json!({
        "search": "test",
        "queryType": "full",
        "semantic": {"semanticConfiguration": "default"}
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn semantic_query_type_without_configuration_rejected() {
    let app = super::common::app();
    let (status, _) = create_semantic_index(&app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);

    let query = json!({
        "search": "test",
        "queryType": "semantic"
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn semantic_malformed_request_rejected() {
    let app = super::common::app();
    let (status, _) = create_semantic_index(&app, "sem").await;
    assert_eq!(status, StatusCode::CREATED);

    // Missing configuration.
    let query = json!({
        "search": "test",
        "semantic": {"answers": {"count": 1}}
    });
    let (status, body) = call(app.clone(), search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");

    // Invalid error handling mode.
    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default", "semanticErrorHandling": "bogus"}
    });
    let (status, body) = call(app.clone(), search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");

    // Invalid answers type.
    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default", "answers": {"count": 1, "type": "generative"}}
    });
    let (status, body) = call(app.clone(), search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");

    // answers.count out of range (0).
    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default", "answers": {"count": 0}}
    });
    let (status, body) = call(app.clone(), search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");

    // answers.count out of range (6, above the default max of 5).
    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default", "answers": {"count": 6}}
    });
    let (status, body) = call(app.clone(), search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");

    // captions.count out of range (4, above the default max of 3).
    let query = json!({
        "search": "test",
        "semantic": {"semanticConfiguration": "default", "captions": {"count": 4}}
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

// ---------------------------------------------------------------------------
// Response shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_returns_reranker_score_on_all_documents() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "search",
        "semantic": {"semanticConfiguration": "default"}
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let docs = must_array(&body["value"], "value");
    assert!(!docs.is_empty());
    for doc in docs {
        let score = must_f64(&doc["@search.rerankerScore"], "@search.rerankerScore");
        assert!(
            (0.0..=1.0).contains(&score),
            "reranker score out of range: {score}"
        );
    }
}

#[tokio::test]
async fn semantic_search_returns_top_level_answers() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "vector search embeddings",
        "semantic": {
            "semanticConfiguration": "default",
            "answers": {"count": 3, "type": "extractive"}
        }
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let answers = must_array(&body["@search.answers"], "@search.answers");
    assert!(!answers.is_empty());
    assert!(answers.len() <= 3);
    for answer in answers {
        let score = must_f64(&answer["score"], "answer score");
        assert!((0.0..=1.0).contains(&score));
        assert!(answer["key"].as_str().is_some());
        assert!(answer["text"].as_str().is_some());
        assert!(answer["highlights"].as_str().is_some());
    }
    // The top answer should come from the vector-search document.
    assert_eq!(answers[0]["key"], "2");
}

#[tokio::test]
async fn semantic_search_returns_captions() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "azure search",
        "semantic": {
            "semanticConfiguration": "default",
            "captions": {"count": 2, "type": "extractive"}
        }
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let docs = must_array(&body["value"], "value");
    let first_doc = must_find(docs, "1", "doc");
    let captions = must_array(&first_doc["@search.captions"], "captions");
    assert!(!captions.is_empty());
    assert!(captions.len() <= 2);
    for caption in captions {
        assert!(caption["highlights"].as_str().is_some());
        let text = caption["text"].as_str().unwrap_or("");
        assert!(text.chars().count() <= 200);
    }
}

#[tokio::test]
async fn semantic_search_captions_with_nested_answers() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "azure search",
        "semantic": {
            "semanticConfiguration": "default",
            "captions": {
                "count": 1,
                "type": "extractive",
                "answers": {"count": 1, "type": "extractive"}
            }
        }
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let docs = must_array(&body["value"], "value");
    let first_doc = must_find(docs, "1", "doc");
    let captions = must_array(&first_doc["@search.captions"], "captions");
    assert!(!captions.is_empty());
    let nested = must_array(&captions[0]["answers"], "caption answers");
    assert!(!nested.is_empty());
    assert!(nested[0]["score"].as_f64().is_some());
    assert!(nested[0]["text"].as_str().is_some());
    assert!(nested[0]["highlights"].as_str().is_some());
}

#[tokio::test]
async fn semantic_search_query_context_accepted() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "vector search",
        "semantic": {
            "semanticConfiguration": "default",
            "queryContext": {"questions": ["What is vector search used for?"]},
            "answers": {"count": 3}
        }
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["@search.answers"].is_array());
}

#[tokio::test]
async fn semantic_search_answers_omitted_when_not_requested() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "search",
        "semantic": {"semanticConfiguration": "default"}
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("@search.answers").is_none(),
        "expected @search.answers to be omitted"
    );
    for doc in must_array(&body["value"], "value") {
        assert!(
            doc.get("@search.captions").is_none(),
            "expected @search.captions to be omitted"
        );
    }
}

#[tokio::test]
async fn non_semantic_search_on_semantic_index_has_no_semantic_properties() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({"search": "vector search"});
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("@search.answers").is_none());
    for doc in must_array(&body["value"], "value") {
        assert!(doc.get("@search.captions").is_none());
        assert!(doc.get("@search.rerankerScore").is_none());
    }
}

#[tokio::test]
async fn semantic_search_with_filter_orderby_select_facets_highlight() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "search",
        "semantic": {
            "semanticConfiguration": "default",
            "answers": {"count": 3},
            "captions": {"count": 1}
        },
        "filter": "price gt 5",
        "orderby": ["price asc"],
        "select": ["id", "title", "price"],
        "facets": ["price"],
        "highlight": ["title"],
        "highlightPreTag": "<b>",
        "highlightPostTag": "</b>"
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let docs = must_array(&body["value"], "value");
    assert!(!docs.is_empty());
    // orderby price asc: doc 1 (10.0) first.
    assert_eq!(docs[0]["id"], "1");
    // select projection: no content/summary fields.
    assert!(docs[0].get("content").is_none());
    assert!(docs[0].get("summary").is_none());
    // facets present.
    assert!(body["@search.facets"].is_object());
    // highlights use the custom tags.
    if let Some(highlights) = docs[0]["@search.highlights"].as_object() {
        for fragments in highlights.values() {
            if let Some(fragments) = fragments.as_array() {
                for fragment in fragments {
                    if let Some(text) = fragment.as_str() {
                        assert!(text.contains("<b>"));
                    }
                }
            }
        }
    }
    // Semantic properties still present.
    assert!(body["@search.answers"].is_array());
    assert!(docs[0]["@search.rerankerScore"].as_f64().is_some());
}

#[tokio::test]
async fn semantic_search_answers_selected_before_paging() {
    let app = super::common::app();
    setup_semantic(&app).await;

    // Page 1 with top=1: the answer set is still computed over the full
    // result set, so the top answer may come from a document beyond the page.
    let query = json!({
        "search": "search",
        "semantic": {
            "semanticConfiguration": "default",
            "answers": {"count": 3}
        },
        "top": 1
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let docs = must_array(&body["value"], "value");
    assert_eq!(docs.len(), 1);
    assert!(body["@search.answers"].is_array());
}

// ---------------------------------------------------------------------------
// Flat SDK format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flat_sdk_semantic_search() {
    let app = super::common::app();
    setup_semantic(&app).await;

    // The exact body the pinned Python/.NET SDKs send.
    let query = json!({
        "search": "vector search embeddings",
        "queryType": "semantic",
        "semanticConfiguration": "default",
        "semanticErrorHandling": "fail",
        "semanticMaxWaitInMilliseconds": 500,
        "semanticQuery": "vector search?",
        "answers": "extractive|count-3,threshold-0.7",
        "captions": "extractive|highlight-true"
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["@search.answers"].is_array());
    let docs = must_array(&body["value"], "value");
    for doc in docs {
        assert!(doc["@search.rerankerScore"].as_f64().is_some());
    }
    let first_doc = must_find(docs, "2", "doc");
    assert!(first_doc["@search.captions"].is_array());
}

#[tokio::test]
async fn flat_sdk_semantic_search_query_answer_fallback() {
    let app = super::common::app();
    setup_semantic(&app).await;

    // The `queryAnswer` / `queryCaption` / `queryAnswerCount` properties.
    let query = json!({
        "search": "vector search",
        "queryType": "semantic",
        "semanticConfiguration": "default",
        "queryAnswer": "extractive",
        "queryAnswerCount": 2,
        "queryCaption": "extractive",
        "semanticErrorMode": "partial"
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    let answers = must_array(&body["@search.answers"], "@search.answers");
    assert!(answers.len() <= 2);
    let docs = must_array(&body["value"], "value");
    let first_doc = must_find(docs, "2", "doc");
    assert!(first_doc["@search.captions"].is_array());
}

#[tokio::test]
async fn flat_sdk_semantic_search_none_types_request_nothing() {
    let app = super::common::app();
    setup_semantic(&app).await;

    let query = json!({
        "search": "vector search",
        "queryType": "semantic",
        "semanticConfiguration": "default",
        "answers": "none",
        "captions": "none"
    });
    let (status, body) = call(app, search_request("sem", query)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("@search.answers").is_none());
    for doc in must_array(&body["value"], "value") {
        assert!(doc.get("@search.captions").is_none());
        assert!(doc["@search.rerankerScore"].as_f64().is_some());
    }
}
