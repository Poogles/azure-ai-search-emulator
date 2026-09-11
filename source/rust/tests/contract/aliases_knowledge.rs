//! Contract tests for index aliases (Gap 11) and knowledge sources /
//! knowledge bases + agentic retrieval (Gap 13).

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use super::common::*;

// ---------------------------------------------------------------------------
// Index aliases
// ---------------------------------------------------------------------------

fn alias_request(
    method: &str,
    name: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/aliases('{name}')?api-version={API_VERSION}");
    request(method, &uri, Some(API_KEY), body)
}

fn create_alias_request(name: &str, index: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/aliases?api-version={API_VERSION}");
    request(
        "POST",
        &uri,
        Some(API_KEY),
        Some(json!({"name": name, "indexes": [index]})),
    )
}

fn list_alias_request() -> axum::http::Request<axum::body::Body> {
    let uri = format!("/aliases?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn create_alias_returns_201_with_etag() {
    let app = app();
    let (status, body) = call(app, create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "alias1");
    assert_eq!(body["indexes"][0], "hotels");
    // Etags follow the Azure shape: a quoted hex string (`"0x..."`).
    let etag = body["@odata.etag"].as_str().unwrap_or("");
    assert!(
        etag.starts_with("\"0x") && etag.ends_with('"') && etag.len() > 5,
        "unexpected etag format: {etag}"
    );
}

#[tokio::test]
async fn create_duplicate_alias_returns_409() {
    let app = app();
    let (status, _) = call(app.clone(), create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, create_alias_request("alias1", "other")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "AliasAlreadyExists");
}

#[tokio::test]
async fn create_alias_requires_auth() {
    let app = app();
    let uri = format!("/aliases?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"name": "alias1", "indexes": ["hotels"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn create_alias_rejects_empty_indexes() {
    let app = app();
    let uri = format!("/aliases?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"name": "alias1", "indexes": []})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidAlias");
}

#[tokio::test]
async fn list_aliases_returns_value_array_sorted_by_name() {
    let app = app();
    let (status, _) = call(app.clone(), create_alias_request("zeta", "i1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), create_alias_request("alpha", "i2")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, list_alias_request()).await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["name"], "alpha");
    assert_eq!(value[1]["name"], "zeta");
}

#[tokio::test]
async fn get_alias_returns_alias() {
    let app = app();
    let (status, _) = call(app.clone(), create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, alias_request("GET", "alias1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "alias1");
    assert_eq!(body["indexes"][0], "hotels");
}

#[tokio::test]
async fn get_missing_alias_returns_404() {
    let app = app();
    let (status, body) = call(app, alias_request("GET", "missing", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn update_alias_replaces_and_bumps_etag() {
    let app = app();
    let (status, created) = call(app.clone(), create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        app.clone(),
        alias_request(
            "PUT",
            "alias1",
            Some(json!({"name": "alias1", "indexes": ["hotels-v2"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["indexes"][0], "hotels-v2");
    assert_ne!(body["@odata.etag"], created["@odata.etag"]);
}

#[tokio::test]
async fn update_alias_name_mismatch_returns_400() {
    let app = app();
    let (status, body) = call(
        app,
        alias_request(
            "PUT",
            "alias1",
            Some(json!({"name": "other", "indexes": ["hotels"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidAlias");
}

#[tokio::test]
async fn update_alias_rejects_empty_indexes() {
    let app = app();
    // PUT validates like POST: an empty `indexes` array is rejected.
    let (status, body) = call(
        app,
        alias_request(
            "PUT",
            "alias1",
            Some(json!({"name": "alias1", "indexes": []})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidAlias");
}

#[tokio::test]
async fn alias_and_index_names_share_a_namespace() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);

    // An alias cannot take the name of an existing index (POST or PUT).
    let (status, body) = call(app.clone(), create_alias_request("items", "items")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "AliasAlreadyExists");
    let (status, body) = call(
        app.clone(),
        alias_request(
            "PUT",
            "items",
            Some(json!({"name": "items", "indexes": ["items"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "AliasAlreadyExists");

    // An index cannot take the name of an existing alias (POST or PUT).
    let (status, _) = call(app.clone(), create_alias_request("alias1", "items")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        app.clone(),
        post_index_request("alias1", Some(API_KEY), Some(API_VERSION)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "IndexAlreadyExists");
    let (status, _) = call(
        app,
        put_index_request("alias1", Some(API_KEY), Some(API_VERSION)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_alias_returns_204_then_404() {
    let app = app();
    let (status, _) = call(app.clone(), create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), alias_request("DELETE", "alias1", None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(app, alias_request("GET", "alias1", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn admin_reset_clears_aliases() {
    let app = app();
    let (status, _) = call(app.clone(), create_alias_request("alias1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), request("POST", "/admin/reset", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(app, list_alias_request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------
// Knowledge sources
// ---------------------------------------------------------------------------

fn source_request(
    method: &str,
    name: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgesources('{name}')?api-version={API_VERSION}");
    request(method, &uri, Some(API_KEY), body)
}

fn create_source_request(name: &str, index: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgesources?api-version={API_VERSION}");
    request(
        "POST",
        &uri,
        Some(API_KEY),
        Some(json!({
            "name": name,
            "kind": "searchIndex",
            "searchIndexParameters": {"searchIndexName": index}
        })),
    )
}

fn list_source_request() -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgesources?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn create_knowledge_source_returns_201_with_etag() {
    let app = app();
    let (status, body) = call(app, create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "src1");
    assert_eq!(body["kind"], "searchIndex");
    assert_eq!(body["searchIndexParameters"]["searchIndexName"], "hotels");
    assert!(body["@odata.etag"].is_string());
}

#[tokio::test]
async fn create_duplicate_knowledge_source_returns_409() {
    let app = app();
    let (status, _) = call(app.clone(), create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, create_source_request("src1", "other")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "KnowledgeSourceAlreadyExists");
}

#[tokio::test]
async fn create_knowledge_source_requires_auth() {
    let app = app();
    let uri = format!("/knowledgesources?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"name": "src1", "kind": "searchIndex", "searchIndexParameters": {"searchIndexName": "hotels"}})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn create_knowledge_source_rejects_missing_kind() {
    let app = app();
    let uri = format!("/knowledgesources?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"name": "src1", "searchIndexParameters": {"searchIndexName": "hotels"}})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidKnowledgeSource");
}

#[tokio::test]
async fn create_search_index_source_rejects_missing_index_name() {
    let app = app();
    let uri = format!("/knowledgesources?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"name": "src1", "kind": "searchIndex", "searchIndexParameters": {}})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidKnowledgeSource");
}

#[tokio::test]
async fn create_search_index_source_accepts_missing_index() {
    // The emulator stores knowledge sources opaquely and does not check that
    // the referenced index exists; pin that decision.
    let app = app();
    let (status, body) = call(app, create_source_request("src1", "no-such-index")).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "opaque source rejected: {body}"
    );
    assert_eq!(
        body["searchIndexParameters"]["searchIndexName"],
        "no-such-index"
    );
}

#[tokio::test]
async fn list_knowledge_sources_returns_value_array_sorted_by_name() {
    let app = app();
    let (status, _) = call(app.clone(), create_source_request("zeta", "i1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), create_source_request("alpha", "i2")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, list_source_request()).await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["name"], "alpha");
    assert_eq!(value[1]["name"], "zeta");
}

#[tokio::test]
async fn get_knowledge_source_returns_source() {
    let app = app();
    let (status, _) = call(app.clone(), create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, source_request("GET", "src1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "src1");
    assert_eq!(body["kind"], "searchIndex");
}

#[tokio::test]
async fn get_missing_knowledge_source_returns_404() {
    let app = app();
    let (status, body) = call(app, source_request("GET", "missing", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn update_knowledge_source_replaces_and_bumps_etag() {
    let app = app();
    let (status, created) = call(app.clone(), create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        app.clone(),
        source_request(
            "PUT",
            "src1",
            Some(json!({
                "name": "src1",
                "kind": "searchIndex",
                "description": "updated",
                "searchIndexParameters": {"searchIndexName": "hotels"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["description"], "updated");
    assert_ne!(body["@odata.etag"], created["@odata.etag"]);
}

#[tokio::test]
async fn update_knowledge_source_name_mismatch_returns_400() {
    let app = app();
    let (status, body) = call(
        app,
        source_request(
            "PUT",
            "src1",
            Some(json!({
                "name": "other",
                "kind": "searchIndex",
                "searchIndexParameters": {"searchIndexName": "hotels"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidKnowledgeSource");
}

#[tokio::test]
async fn delete_knowledge_source_returns_204_then_404() {
    let app = app();
    let (status, _) = call(app.clone(), create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), source_request("DELETE", "src1", None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(app, source_request("GET", "src1", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn admin_reset_clears_knowledge_sources() {
    let app = app();
    let (status, _) = call(app.clone(), create_source_request("src1", "hotels")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), request("POST", "/admin/reset", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(app, list_source_request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------
// Knowledge bases + agentic retrieval
// ---------------------------------------------------------------------------

fn base_request(
    method: &str,
    name: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgebases('{name}')?api-version={API_VERSION}");
    request(method, &uri, Some(API_KEY), body)
}

fn create_base_request(name: &str, source: &str) -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgebases?api-version={API_VERSION}");
    request(
        "POST",
        &uri,
        Some(API_KEY),
        Some(json!({
            "name": name,
            "knowledgeSources": [{"name": source}]
        })),
    )
}

fn list_base_request() -> axum::http::Request<axum::body::Body> {
    let uri = format!("/knowledgebases?api-version={API_VERSION}");
    request("GET", &uri, Some(API_KEY), None)
}

#[tokio::test]
async fn create_knowledge_base_returns_201_with_etag() {
    let app = app();
    let (status, body) = call(app, create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "base1");
    assert_eq!(body["knowledgeSources"][0]["name"], "src1");
    assert!(body["@odata.etag"].is_string());
}

#[tokio::test]
async fn create_duplicate_knowledge_base_returns_409() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, create_base_request("base1", "src2")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "KnowledgeBaseAlreadyExists");
}

#[tokio::test]
async fn create_knowledge_base_requires_auth() {
    let app = app();
    let uri = format!("/knowledgebases?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"name": "base1", "knowledgeSources": [{"name": "src1"}]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "AuthenticationFailed");
}

#[tokio::test]
async fn create_knowledge_base_rejects_empty_sources() {
    let app = app();
    let uri = format!("/knowledgebases?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"name": "base1", "knowledgeSources": []})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidKnowledgeBase");
}

#[tokio::test]
async fn list_knowledge_bases_returns_value_array_sorted_by_name() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("zeta", "s1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), create_base_request("alpha", "s2")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, list_base_request()).await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["name"], "alpha");
    assert_eq!(value[1]["name"], "zeta");
}

#[tokio::test]
async fn get_knowledge_base_returns_base() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, base_request("GET", "base1", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "base1");
    assert_eq!(body["knowledgeSources"][0]["name"], "src1");
}

#[tokio::test]
async fn get_missing_knowledge_base_returns_404() {
    let app = app();
    let (status, body) = call(app, base_request("GET", "missing", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn update_knowledge_base_name_mismatch_returns_400() {
    let app = app();
    let (status, body) = call(
        app,
        base_request(
            "PUT",
            "base1",
            Some(json!({"name": "other", "knowledgeSources": [{"name": "src1"}]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidKnowledgeBase");
}

#[tokio::test]
async fn delete_knowledge_base_returns_204_then_404() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), base_request("DELETE", "base1", None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(app, base_request("GET", "base1", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn admin_reset_clears_knowledge_bases() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(app.clone(), request("POST", "/admin/reset", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(app, list_base_request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn retrieve_knowledge_base_returns_empty_response() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let uri = format!("/knowledgebases('base1')/retrieve?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({"intents": [{"type": "semantic", "search": "hotels"}]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["response"].as_array().map(Vec::len), Some(0));
    assert_eq!(body["activity"].as_array().map(Vec::len), Some(0));
    assert_eq!(body["references"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn retrieve_missing_knowledge_base_returns_404() {
    let app = app();
    let uri = format!("/knowledgebases('missing')/retrieve?api-version={API_VERSION}");
    let (status, body) = call(
        app,
        request("POST", &uri, Some(API_KEY), Some(json!({"intents": []}))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn retrieve_knowledge_base_non_post_returns_405() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let uri = format!("/knowledgebases('base1')/retrieve?api-version={API_VERSION}");
    let (status, _) = call(app, request("GET", &uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn knowledge_base_unknown_subroute_returns_404() {
    let app = app();
    let (status, _) = call(app.clone(), create_base_request("base1", "src1")).await;
    assert_eq!(status, StatusCode::CREATED);
    let uri = format!("/knowledgebases('base1')/nope?api-version={API_VERSION}");
    let (status, _) = call(app, request("GET", &uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn alias_resolves_for_search_and_document_routes() {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([{"@search.action": "upload", "document": {"id": "1", "title": "hello"}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(app.clone(), create_alias_request("alias1", "items")).await;
    assert_eq!(status, StatusCode::CREATED);

    // Search via the alias name resolves to the target index.
    let (status, body) = call(
        app.clone(),
        search_request("alias1", json!({"search": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");

    // Single-document lookup via the alias.
    let uri = format!("/indexes('alias1')/docs('1')?api-version={API_VERSION}");
    let (status, body) = call(app.clone(), request("GET", &uri, Some(API_KEY), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "1");

    // Document count via the alias.
    let uri = format!("/indexes('alias1')/docs/$count?api-version={API_VERSION}");
    let response = must(
        app.clone()
            .oneshot(request("GET", &uri, Some(API_KEY), None))
            .await,
    );
    assert_eq!(response.status(), StatusCode::OK);

    // An alias pointing at a missing index behaves like the missing index.
    let (status, _) = call(app.clone(), create_alias_request("dangling", "missing")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(app, search_request("dangling", json!({"search": "*"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "ResourceNotFound");
}
