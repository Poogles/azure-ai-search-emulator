//! Named-resource HTTP handlers (`synonymmaps`, `aliases`,
//! `knowledgesources`, `knowledgebases`).
//!
//! Move-only split of [`crate::api`]: the collection/PUT/GET/DELETE handlers
//! plus the `OData` segment parsers and body validators live here; index,
//! document, and search routes stay in the parent module.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use super::AppState;
use crate::error::ApiError;
use crate::service::ResourceKind;

/// `POST /synonymmaps` — Create Synonym Map. The name comes from the body.
pub(crate) async fn create_synonym_map(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let definition = super::parse_body(&body)?;
    let name = definition.get("name").and_then(Value::as_str).unwrap_or("");
    let format = definition
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("");
    let synonyms = definition
        .get("synonyms")
        .and_then(Value::as_str)
        .unwrap_or("");
    let map = state.service.create_synonym_map(name, format, synonyms)?;
    Ok((StatusCode::CREATED, Json(map.to_value())))
}

/// `GET /synonymmaps` — List Synonym Maps.
pub(crate) async fn list_synonym_maps(State(state): State<AppState>) -> Json<Value> {
    let maps = state
        .service
        .list_synonym_maps()
        .into_iter()
        .map(|map| map.to_value())
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "value": maps }))
}

/// `POST /aliases` — Create Alias. The name comes from the body.
pub(crate) async fn create_alias(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    create_named_resource(&state, ResourceKind::Alias, &body)
}

/// `GET /aliases` — List Aliases.
pub(crate) async fn list_aliases(State(state): State<AppState>) -> Json<Value> {
    list_named_resources(&state, ResourceKind::Alias)
}

/// `POST /knowledgesources` — Create Knowledge Source. The name comes from the body.
pub(crate) async fn create_knowledge_source(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    create_named_resource(&state, ResourceKind::KnowledgeSource, &body)
}

/// `GET /knowledgesources` — List Knowledge Sources.
pub(crate) async fn list_knowledge_sources(State(state): State<AppState>) -> Json<Value> {
    list_named_resources(&state, ResourceKind::KnowledgeSource)
}

/// `POST /knowledgebases` — Create Knowledge Base. The name comes from the body.
pub(crate) async fn create_knowledge_base(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    create_named_resource(&state, ResourceKind::KnowledgeBase, &body)
}

/// `GET /knowledgebases` — List Knowledge Bases.
pub(crate) async fn list_knowledge_bases(State(state): State<AppState>) -> Json<Value> {
    list_named_resources(&state, ResourceKind::KnowledgeBase)
}

/// Validates a named resource body and creates or replaces the resource,
/// returning its JSON representation.
pub(crate) fn create_or_update_named_resource(
    state: &AppState,
    kind: ResourceKind,
    name: &str,
    definition: &Value,
) -> Result<Value, ApiError> {
    if kind == ResourceKind::SynonymMap {
        // The synonym-map mismatch message is capitalized (a pre-existing
        // quirk); the other kinds use the lowercase label via
        // `validate_named_body`.
        let body_name = definition.get("name").and_then(Value::as_str).unwrap_or("");
        if body_name != name {
            return Err(ApiError::bad_request(
                kind.invalid_code(),
                format!("Synonym map name in path ({name:?}) does not match name in body."),
            ));
        }
        let format = definition
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("");
        let synonyms = definition
            .get("synonyms")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Ok(state
            .service
            .create_or_update_synonym_map(name, format, synonyms)?
            .to_value());
    }
    validate_named_body(name, definition, kind.label(), kind.invalid_code())?;
    validate_named_resource_body(kind, definition)?;
    Ok(state
        .service
        .create_or_update_named_resource(kind, name, definition)?
        .to_value())
}

/// Validates a named resource body's kind-specific shape.
pub(crate) fn validate_named_resource_body(
    kind: ResourceKind,
    definition: &Value,
) -> Result<(), ApiError> {
    match kind {
        ResourceKind::Alias => validate_alias(definition),
        ResourceKind::KnowledgeSource => validate_knowledge_source(definition),
        ResourceKind::KnowledgeBase => validate_knowledge_base(definition),
        ResourceKind::SynonymMap => Ok(()),
    }
}

/// Shared `POST` handler for the named resource collections (aliases,
/// knowledge sources, knowledge bases): the name comes from the body.
fn create_named_resource(
    state: &AppState,
    kind: ResourceKind,
    body: &axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let definition = super::parse_body(body)?;
    let name = body_name(&definition, kind.label(), kind.invalid_code())?;
    validate_named_resource_body(kind, &definition)?;
    let resource = state
        .service
        .create_named_resource(kind, &name, &definition)?;
    Ok((StatusCode::CREATED, Json(resource.to_value())))
}

/// Shared `GET` handler for the named resource collections.
fn list_named_resources(state: &AppState, kind: ResourceKind) -> Json<Value> {
    let value = state
        .service
        .list_named_resources(kind)
        .into_iter()
        .map(|resource| resource.to_value())
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "value": value }))
}

/// The quoted name inside an `OData` segment fragment: strips the surrounding
/// single quotes and rejects an empty name.
pub(crate) fn unquote_name(fragment: &str) -> Option<&str> {
    fragment
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .filter(|s| !s.is_empty())
}

/// Splits an OData-style path segment `{prefix}'name')` into its name:
/// strips the prefix and closing paren, then the surrounding quotes.
fn split_odata_segment<'a>(raw: &'a str, prefix: &str) -> Option<&'a str> {
    raw.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(')'))
        .and_then(unquote_name)
}

/// Parses an OData-style path segment (`indexes('name')`, `docs('key')`,
/// `aliases('name')`, ...) into its name. `prefix` is the segment's resource
/// prefix (e.g. `aliases(`); `kind_label` and `code` shape the error for a
/// malformed segment.
pub(crate) fn parse_odata_segment(
    raw: &str,
    prefix: &str,
    kind_label: &str,
    code: &str,
) -> Result<String, ApiError> {
    split_odata_segment(raw, prefix)
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::bad_request(
                code,
                format!("Invalid {kind_label} path segment {raw:?}; expected {prefix}'name')."),
            )
        })
}

/// Parses an OData-style named resource path segment
/// (`synonymmaps('name')`, `aliases('name')`, `knowledgesources('name')`,
/// `knowledgebases('name')`) into its kind and name. Returns `Ok(None)` when
/// the segment is not a named resource (i.e. an index segment).
pub(crate) fn parse_named_resource_segment(
    raw: &str,
) -> Result<Option<(ResourceKind, String)>, ApiError> {
    for kind in ResourceKind::ALL {
        if raw.starts_with(kind.path_prefix()) {
            let name = split_odata_segment(raw, kind.path_prefix())
                .map(str::to_owned)
                .ok_or_else(|| kind.invalid_segment_error(raw))?;
            return Ok(Some((kind, name)));
        }
    }
    Ok(None)
}

/// Extracts a non-empty `name` from a resource body.
fn body_name(definition: &Value, kind: &str, code: &str) -> Result<String, ApiError> {
    definition
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::bad_request(code, format!("The {kind} name is required.")))
}

/// Checks that the body's `name` matches the name parsed from the path.
fn validate_named_body(
    path_name: &str,
    definition: &Value,
    kind: &str,
    code: &str,
) -> Result<(), ApiError> {
    let body_name = definition.get("name").and_then(Value::as_str).unwrap_or("");
    if body_name.is_empty() || body_name != path_name {
        return Err(ApiError::bad_request(
            code,
            format!("{kind} name in path ({path_name:?}) does not match name in body."),
        ));
    }
    Ok(())
}

/// Validates an alias body: a non-empty `indexes` array.
fn validate_alias(definition: &Value) -> Result<(), ApiError> {
    let indexes = definition
        .get("indexes")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "InvalidAlias",
                "An alias must define a non-empty \"indexes\" array.",
            )
        })?;
    for index in indexes {
        if !index.is_string() || index.as_str().is_some_and(str::is_empty) {
            return Err(ApiError::bad_request(
                "InvalidAlias",
                "Alias \"indexes\" entries must be non-empty index name strings.",
            ));
        }
    }
    Ok(())
}

/// Validates a knowledge source body: a non-empty `kind` discriminator.
fn validate_knowledge_source(definition: &Value) -> Result<(), ApiError> {
    let kind = definition
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "InvalidKnowledgeSource",
                "A knowledge source must define a non-empty \"kind\".",
            )
        })?;
    // Other kinds (azureBlob, indexedOneLake, web) are accepted opaquely:
    // the emulator stores and echoes them without modeling their parameters.
    if kind == "searchIndex" {
        let index_name = definition
            .get("searchIndexParameters")
            .and_then(|p| p.get("searchIndexName"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if index_name.is_none() {
            return Err(ApiError::bad_request(
                "InvalidKnowledgeSource",
                "A searchIndex knowledge source must define \
                 \"searchIndexParameters.searchIndexName\".",
            ));
        }
    }
    Ok(())
}

/// Validates a knowledge base body: a non-empty `knowledgeSources` array.
fn validate_knowledge_base(definition: &Value) -> Result<(), ApiError> {
    let sources = definition
        .get("knowledgeSources")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request(
                "InvalidKnowledgeBase",
                "A knowledge base must define a non-empty \"knowledgeSources\" array.",
            )
        })?;
    for source in sources {
        if source
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(ApiError::bad_request(
                "InvalidKnowledgeBase",
                "Each \"knowledgeSources\" entry must have a non-empty \"name\".",
            ));
        }
    }
    Ok(())
}
