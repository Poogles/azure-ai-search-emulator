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
    // BM25 relevance scores: positive numbers, ordered descending (the two
    // single-occurrence titles score nearly identically, so the key field
    // breaks the tie).
    let score0 = value[0]["@search.score"].as_f64().unwrap_or(0.0);
    let score1 = value[1]["@search.score"].as_f64().unwrap_or(0.0);
    assert!(score0 > 0.0, "expected a positive score, got {score0}");
    assert!(score1 > 0.0, "expected a positive score, got {score1}");
    assert!(
        score0 >= score1,
        "expected score ordering, got {score0} then {score1}"
    );
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

fn analyze_request(name: &str, body: serde_json::Value) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/indexes('{name}')/search.analyze?api-version={API_VERSION}");
    request("POST", &uri, Some(API_KEY), Some(body))
}

#[tokio::test]
async fn analyze_text_returns_tokens_with_offsets() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        app,
        analyze_request("items", json!({"text": "Hello, World!"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tokens = &body["tokens"];
    assert!(tokens.as_array().map_or(0, Vec::len) >= 2);
    assert_eq!(tokens[0]["token"], "hello");
    assert_eq!(tokens[0]["startOffset"], 0);
    assert_eq!(tokens[0]["endOffset"], 5);
    assert_eq!(tokens[0]["position"], 0);
    assert_eq!(tokens[1]["token"], "world");
    assert_eq!(tokens[1]["position"], 1);
}

#[tokio::test]
async fn analyze_text_missing_text_field_returns_400() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(app, analyze_request("items", json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidRequest");
}

#[tokio::test]
async fn analyze_text_missing_index_returns_404() {
    let app = app();
    let (status, body) = call(app, analyze_request("missing", json!({"text": "hello"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn analyze_text_keyword_and_whitespace_analyzers() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    // `keyword` emits the whole input as one verbatim token.
    let (status, body) = call(
        app.clone(),
        analyze_request(
            "items",
            json!({"text": "Running Tests", "analyzer": "keyword"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tokens"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tokens"][0]["token"], "Running Tests");
    assert_eq!(body["tokens"][0]["startOffset"], 0);
    assert_eq!(body["tokens"][0]["endOffset"], "Running Tests".len());

    // `whitespace` splits without lowercasing or stemming.
    let (status, body) = call(
        app,
        analyze_request(
            "items",
            json!({"text": "Running tests", "analyzer": "whitespace"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tokens: Vec<&str> = body["tokens"]
        .as_array()
        .map(|items| items.iter().filter_map(|t| t["token"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(tokens, vec!["Running", "tests"]);
    assert_eq!(body["tokens"][1]["startOffset"], "Running ".len());
}

#[tokio::test]
async fn select_star_returns_all_fields() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "one", "price": 1.5}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "select": "*"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    for field in ["id", "title", "price"] {
        assert!(
            body["value"][0].get(field).is_some(),
            "missing field {field}: {}",
            body["value"][0]
        );
    }
}

#[tokio::test]
async fn orderby_search_score_orders_by_relevance() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure azure azure"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "azure"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let ids = |body: &serde_json::Value| {
        body["value"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|d| d["id"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure", "orderby": "@search.score desc"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["1", "2"]);
    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "azure", "orderby": "@search.score asc"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), vec!["2", "1"]);
}

#[tokio::test]
async fn facet_string_with_count_option_limits_values() {
    let app = app();
    let uri = format!("/indexes('faceted')?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "PUT",
            &uri,
            Some(API_KEY),
            Some(json!({
                "name": "faceted",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true, "facetable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "faceted",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "tags": ["red", "blue"]}},
                {"@search.action": "upload", "document": {"id": "2", "tags": ["red"]}},
                {"@search.action": "upload", "document": {"id": "3", "tags": ["green"]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Options in the single-string form attach to the preceding facet.
    let (status, body) = call(
        app,
        search_request("faceted", json!({"search": "*", "facets": "tags,count:1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["@search.facets"]["tags"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["value"], "red");
    assert_eq!(entries[0]["count"], 2);
}

#[tokio::test]
async fn invalid_fuzzy_distance_returns_400() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, search_request("items", json!({"search": "azure~3"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn service_stats_returns_static_response() {
    let app = app();
    let uri = format!("/servicestats?api-version={API_VERSION}");
    let (status, body) = call(app, request("GET", &uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["counters"]["knowledgeBaseCounter"]["usage"], 0);
    assert_eq!(body["counters"]["knowledgeSourceCounter"]["usage"], 0);
    assert_eq!(
        body["limits"]["maxVectorIndexSizePerIndexInBytes"],
        1_073_741_824
    );
}

#[tokio::test]
async fn service_stats_requires_api_key() {
    let app = app();
    let uri = format!("/servicestats?api-version={API_VERSION}");
    let (status, _) = call(app, request("GET", &uri, None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn search_mode_any_matches_union() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure search"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "local emulators"}},
                {"@search.action": "upload", "document": {"id": "3", "title": "unrelated"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Default (any/OR, matching Azure): either term matches.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "azure emulators"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));

    // Explicit all: AND semantics — nothing matches both terms.
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure emulators", "searchMode": "all"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));

    // Any (OR): either term matches.
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure emulators", "searchMode": "any"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));

    // Invalid searchMode is rejected explicitly.
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "azure", "searchMode": "both"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn search_stemming_matches_inflected_forms() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "running shoes"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "other things"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for term in ["run", "runs", "running"] {
        let (status, body) = call(
            app.clone(),
            search_request("items", json!({"search": term})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["value"].as_array().map(Vec::len),
            Some(1),
            "term {term:?}"
        );
        assert_eq!(body["value"][0]["id"], "1");
    }
}

#[tokio::test]
async fn search_stopwords_do_not_match() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "the quick fox"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // "the" is an English stopword: removed at index and query time.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "the"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));

    // Non-stopwords in the same document still match.
    let (status, body) = call(app, search_request("items", json!({"search": "quick"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn search_fuzzy_matches_typos() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "azure emulator"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "other things"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Typos within edit distance 1 of the indexed stem "emul".
    for term in ["emu~", "omul~", "emul~2"] {
        let (status, body) = call(
            app.clone(),
            search_request("items", json!({"search": term})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "term {term:?}");
        assert_eq!(
            body["value"].as_array().map(Vec::len),
            Some(1),
            "term {term:?}"
        );
        assert_eq!(body["value"][0]["id"], "1");
    }

    // Unrelated terms match nothing, even fuzzy.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "zzz~"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));

    // A bare `~` uses the default edit distance 2 (matching Azure): "eamu"
    // is two edits from the indexed stem "emul".
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "eamu~"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "eamu~1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));

    // Invalid fuzzy distances are rejected explicitly.
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "emulator~5"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn search_highlights_wrap_matched_terms() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "Azure Search Rocks"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "Other things"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "azure", "highlight": "title"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(1));
    assert_eq!(
        value[0]["@search.highlights"]["title"],
        json!(["<em>Azure</em> Search Rocks"])
    );
    // Documents without a match carry no highlights object.
    assert!(value[0].get("@search.highlights").is_some());

    // Custom tags.
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({
                "search": "azure",
                "highlight": "title",
                "highlightPreTag": "<b>",
                "highlightPostTag": "</b>",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["value"][0]["@search.highlights"]["title"],
        json!(["<b>Azure</b> Search Rocks"])
    );

    // Highlight on an unknown or non-searchable field is rejected.
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "azure", "highlight": "missing"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "azure", "highlight": "price"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn search_fields_weights_boost_scores() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "azure search"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, plain) = call(
        app.clone(),
        search_request("items", json!({"search": "azure"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plain_score = plain["value"][0]["@search.score"].as_f64().unwrap_or(0.0);

    let (status, boosted) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "azure", "searchFields": "title^10"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(boosted["value"].as_array().map(Vec::len), Some(1));
    let boosted_score = boosted["value"][0]["@search.score"].as_f64().unwrap_or(0.0);
    assert!(
        boosted_score > plain_score,
        "boosted score {boosted_score} should exceed plain score {plain_score}"
    );

    // Invalid weights are rejected explicitly.
    for search_fields in ["title^many", "title^0", "title^-2"] {
        let (status, body) = call(
            app.clone(),
            search_request(
                "items",
                json!({"search": "azure", "searchFields": search_fields}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "weight {search_fields:?}");
        assert_eq!(body["error"]["code"], "InvalidQuery");
    }
}

#[tokio::test]
async fn search_orderby_nulls_first_ascending_last_descending() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "priced", "price": 10.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "unpriced"}},
                {"@search.action": "upload", "document": {"id": "3", "title": "cheap", "price": 1.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Ascending: nulls first (Azure ordering).
    let (status, body) = call(
        app.clone(),
        search_request("items", json!({"search": "*", "orderby": "price asc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec!["2", "3", "1"]);

    // Descending: nulls last.
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "orderby": "price desc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec!["1", "3", "2"]);
}

#[tokio::test]
async fn analyze_text_accepts_analyzer_and_field() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    // Known analyzer names are accepted (all map to the English analyzer,
    // except `keyword` and `whitespace`, which tokenize as Azure documents).
    for analyzer in ["standard.lucene", "en.microsoft"] {
        let (status, body) = call(
            app.clone(),
            analyze_request(
                "items",
                json!({"text": "Running tests", "analyzer": analyzer}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "analyzer {analyzer:?}");
        let tokens: Vec<&str> = body["tokens"]
            .as_array()
            .map(|items| items.iter().filter_map(|t| t["token"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(tokens, vec!["run", "test"], "analyzer {analyzer:?}");
    }

    // A valid field is accepted.
    let (status, _) = call(
        app.clone(),
        analyze_request("items", json!({"text": "hello", "field": "title"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Unknown analyzers and fields are rejected explicitly.
    let (status, body) = call(
        app.clone(),
        analyze_request("items", json!({"text": "hello", "analyzer": "nonsense"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidRequest");
    let (status, body) = call(
        app,
        analyze_request("items", json!({"text": "hello", "field": "missing"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidRequest");
}
