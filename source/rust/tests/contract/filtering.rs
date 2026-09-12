use axum::http::StatusCode;
use serde_json::json;

use super::common::*;

async fn app_with_priced_docs() -> axum::Router {
    let app = app();
    let (status, _) = create_index(&app, "items").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "items",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "cheap", "price": 5.0}},
                {"@search.action": "upload", "document": {"id": "2", "title": "mid", "price": 50.0}},
                {"@search.action": "upload", "document": {"id": "3", "title": "expensive", "price": 500.0}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn filter_comparison_narrows_results() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "price ge 50"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = &body["value"];
    assert_eq!(value.as_array().map(Vec::len), Some(2));
    assert_eq!(value[0]["id"], "2");
    assert_eq!(value[1]["id"], "3");
}

#[tokio::test]
async fn filter_logical_operators_combine() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "*", "filter": "price lt 10 or price gt 100"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "1");
    assert_eq!(body["value"][1]["id"], "3");

    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "filter": "not (price lt 100)"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "3");
}

#[tokio::test]
async fn filter_combines_with_full_text() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "mid", "filter": "price gt 10"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "2");
}

#[tokio::test]
async fn filter_invalid_syntax_returns_400() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "price eq"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn filter_unknown_field_returns_400() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request("items", json!({"search": "*", "filter": "missing eq 1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn filter_non_filterable_field_returns_400() {
    // `title` is searchable but not filterable in the shared test index.
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "filter": "title eq 'cheap'"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

async fn app_with_tagged_docs() -> axum::Router {
    let app = app();
    let (status, _) = call(
        app.clone(),
        request(
            "PUT",
            &format!("/indexes('tagged')?api-version={API_VERSION}"),
            Some(API_KEY),
            Some(json!({
                "name": "tagged",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "tagged",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "tags": ["red", "blue"]}},
                {"@search.action": "upload", "document": {"id": "2", "tags": ["green"]}},
                {"@search.action": "upload", "document": {"id": "3", "tags": ["red", "green"]}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    app
}

#[tokio::test]
async fn filter_odata_lambda_any_all() {
    let app = app_with_tagged_docs().await;

    // OData lambda `any`: `tags/any(t: t eq 'red')`.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/any(t: t eq 'red')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["value"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|d| d["id"].as_str().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(ids, vec!["1", "3"]);

    // OData lambda `all`: `tags/all(t: t ne 'green')` matches only doc 1.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/all(t: t ne 'green')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["value"][0]["id"], "1");

    // The space-separated form still works alongside the OData form.
    let (status, body) = call(
        app.clone(),
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags any t eq 'red'"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));

    // A malformed lambda (missing the ':' separator) is rejected.
    let (status, body) = call(
        app,
        search_request(
            "tagged",
            json!({"search": "*", "filter": "tags/any(t eq 'red')"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}

#[tokio::test]
async fn filter_in_operator_matches_list_members() {
    let app = app_with_priced_docs().await;
    let (status, body) = call(
        app.clone(),
        search_request(
            "items",
            json!({"search": "*", "filter": "price in (5.0, 500.0)"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["value"][0]["id"], "1");
    assert_eq!(body["value"][1]["id"], "3");

    // No member matches.
    let (status, body) = call(
        app,
        search_request(
            "items",
            json!({"search": "*", "filter": "price in (1.0, 2.0)"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn filter_string_functions_match_substrings() {
    let app = app();
    // A dedicated index: `title` and `tags` are filterable here (the shared
    // definition leaves `title` unfilterable).
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({
                "name": "books",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                    {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true},
                    {"name": "pages", "type": "Edm.Int32", "filterable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "books",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "hello world", "tags": ["red", "green"], "pages": 100}},
                {"@search.action": "upload", "document": {"id": "2", "title": "goodbye moon", "tags": ["blue"], "pages": 200}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (filter, expected) in [
        ("startswith(title, 'hello')", vec!["1"]),
        ("endswith(title, 'moon')", vec!["2"]),
        ("contains(title, 'o w')", vec!["1"]),
        ("contains(tags, 'een')", vec!["1"]),
        ("title in ('hello world', 'other')", vec!["1"]),
        ("startswith(title, 'zzz')", vec![]),
    ] {
        let (status, body) = call(
            app.clone(),
            search_request("books", json!({"search": "*", "filter": filter})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "filter {filter:?}");
        let ids: Vec<&str> = body["value"]
            .as_array()
            .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(ids, expected, "filter {filter:?}");
    }

    // String functions on non-string fields are rejected explicitly, as are
    // unsupported functions.
    for filter in ["startswith(pages, '1')", "length(title) gt 2"] {
        let (status, body) = call(
            app.clone(),
            search_request("books", json!({"search": "*", "filter": filter})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "filter {filter:?}");
        assert_eq!(body["error"]["code"], "InvalidQuery");
    }
}

#[tokio::test]
async fn filter_date_functions_compare_dates() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({
                "name": "articles",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                    {"name": "published", "type": "Edm.DateTimeOffset", "filterable": true},
                    {"name": "archived", "type": "Edm.DateTimeOffset", "filterable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "articles",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "spring release", "published": "2024-03-15T10:30:00Z", "archived": "2024-03-10T10:30:00Z"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "winter recap", "published": "2023-12-02T08:00:00Z", "archived": "2023-11-01T08:00:00Z"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (filter, expected) in [
        ("datepart(year, published) eq 2024", vec!["1"]),
        ("datepart(month, published) eq 12", vec!["2"]),
        ("datepart(dayofweek, published) eq 5", vec!["1"]),
        (
            "dateadd(day, 1, published) gt utcdatetime('2024-03-15T10:30:00Z')",
            vec!["1"],
        ),
        ("datediff(day, archived, published) eq 5", vec!["1"]),
        ("datediff(day, archived, published) gt 10", vec!["2"]),
        (
            "published gt utcdatetime('2024-01-01T00:00:00Z')",
            vec!["1"],
        ),
        (
            "utcdatetime('2024-01-01T00:00:00Z') lt published",
            vec!["1"],
        ),
        ("datepart(year, published) eq 1999", Vec::<&str>::new()),
    ] {
        let (status, body) = call(
            app.clone(),
            search_request("articles", json!({"search": "*", "filter": filter})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "filter {filter:?}: {body}");
        let ids: Vec<&str> = body["value"]
            .as_array()
            .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(ids, expected, "filter {filter:?}");
    }

    // Malformed date filters are rejected explicitly.
    for filter in [
        "datepart(century, published) eq 21",
        "published gt utcdatetime('not a date')",
        "datepart(year, title) eq 2024",
    ] {
        let (status, body) = call(
            app.clone(),
            search_request("articles", json!({"search": "*", "filter": filter})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "filter {filter:?}");
        assert_eq!(body["error"]["code"], "InvalidQuery");
    }
}

#[tokio::test]
async fn filter_search_functions_match() {
    let app = app();
    let uri = format!("/indexes?api-version={API_VERSION}");
    let (status, _) = call(
        app.clone(),
        request(
            "POST",
            &uri,
            Some(API_KEY),
            Some(json!({
                "name": "docs",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                    {"name": "subtitle", "type": "Edm.String", "filterable": true}
                ]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        app.clone(),
        upload_request(
            "docs",
            json!([
                {"@search.action": "upload", "document": {"id": "1", "title": "Azure Search"}},
                {"@search.action": "upload", "document": {"id": "2", "title": "Other", "subtitle": ""}},
                {"@search.action": "upload", "document": {"id": "3", "title": "More"}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (filter, expected) in [
        ("search.ismatch('azure', title)", vec!["1"]),
        ("search.ismatch('SEARCH', title)", vec!["1"]),
        ("search.ismatch('azure', 'title,subtitle')", vec!["1"]),
        ("search.ismatchscoring('other', title)", vec!["2"]),
        ("search.isempty(subtitle)", vec!["1", "2", "3"]),
        ("search.isnull(subtitle)", vec!["1", "3"]),
        ("search.ismatch('zzz', title)", Vec::<&str>::new()),
    ] {
        let (status, body) = call(
            app.clone(),
            search_request("docs", json!({"search": "*", "filter": filter})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "filter {filter:?}: {body}");
        let ids: Vec<&str> = body["value"]
            .as_array()
            .map(|items| items.iter().filter_map(|d| d["id"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(ids, expected, "filter {filter:?}");
    }

    // Unknown search functions are rejected explicitly.
    let (status, body) = call(
        app,
        search_request(
            "docs",
            json!({"search": "*", "filter": "search.unknown(title)"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "InvalidQuery");
}
