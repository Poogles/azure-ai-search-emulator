//! Vectorizer support (Phase 2.4): a deterministic, in-process text-to-vector
//! function plus parsing of the index-level `vectorizers` configuration.
//!
//! The emulator has no embedding model. Instead it maps text to a fixed-dimension
//! vector with a signed FNV-1a hash over the full-text analyzer's tokens, so the
//! same text always yields the same vector and texts sharing analyzed tokens
//! yield more similar vectors (lexical, not semantic, similarity — see
//! `docs/known_differences.md`). The same function runs at document-index time
//! (when a vectorizer-backed vector field is omitted) and at query time
//! (`kind: "text"` vector queries), keeping the two sides self-consistent.
//!
//! The `vectorizers` array is a wire-format placeholder: the configured
//! `parameters.uri` (or equivalent) is stored and echoed but **never called**.

use serde_json::Value;

use crate::query::analyzers::analyze;

/// FNV-1a 64-bit offset basis and prime.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash of a byte slice. Pure integer arithmetic, no
/// dependencies.
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Converts `text` to a deterministic `dimensions`-length unit vector.
///
/// The text is tokenized with the full-text (English) analyzer — lowercasing,
/// punctuation splitting, stopword removal, and stemming — so `"Running"` and
/// `"run"` contribute the same token. Each token is FNV-1a hashed; the hash
/// selects a bucket (`hash % dimensions`) and a sign (the hash's high bit),
/// and the sign is accumulated into that bucket. The result is L2-normalized
/// to a unit vector so cosine similarity is well-defined. Empty text, or text
/// whose tokens all cancel (e.g. stopword-only input), yields a zero vector.
///
/// # Panics
///
/// Never; `dimensions` of `0` yields an empty vector (callers validate that a
/// vector field's dimensions are positive before calling).
#[must_use]
pub fn text_to_vector(text: &str, dimensions: usize) -> Vec<f32> {
    if dimensions == 0 {
        return Vec::new();
    }
    let mut vector = vec![0.0f32; dimensions];
    for token in analyze(text) {
        let hash = fnv1a_64(token.as_bytes());
        // `hash % dimensions` is in `[0, dimensions)`, so it fits a `usize`.
        #[allow(clippy::cast_possible_truncation)]
        let bucket = (hash % dimensions as u64) as usize;
        let sign = if hash >> 63 == 0 { 1.0f32 } else { -1.0f32 };
        vector[bucket] += sign;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut vector {
            *x /= norm;
        }
    }
    vector
}

/// One parsed `vectorizers[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorizerConfig {
    /// The vectorizer's `name` (referenced by vector fields' `vectorizer`).
    pub name: String,
    /// The `sourceContext.fields` text source, when a `sourceContext` is
    /// present. `None` means no `sourceContext` was configured: the text
    /// source falls back to all searchable string fields (in schema order).
    /// `Some(vec![])` means a `sourceContext` was present but named no fields
    /// (the text source is empty, yielding a zero vector).
    pub source_fields: Option<Vec<String>>,
}

/// The vectorizer `kind`s the emulator accepts. All are stored opaquely and
/// echoed; the emulator never calls the configured endpoint. The pinned SDKs
/// (Python/C# `12.0.0`) emit `azureOpenAI`, `aml`, and `customWebApi`; `uri`
/// is the legacy spelling accepted for wire-format compatibility.
const SUPPORTED_VECTORIZER_KINDS: &[&str] = &["uri", "customWebApi", "azureOpenAI", "aml"];

/// Parses the index-level `vectorizers` array (or its absence) into
/// [`VectorizerConfig`]s, validating structure: each entry needs a non-empty
/// unique `name`, a supported `kind`, and — for `uri`/`customWebApi` kinds — a
/// non-empty endpoint URI. A `sourceContext`, when present, must use
/// `sourceType: "field"`.
///
/// Field-reference validation (that `sourceContext.fields` name searchable
/// string fields, and that vector fields' `vectorizer` references resolve) is
/// done by the service layer, which has the full schema.
///
/// # Errors
///
/// Returns an error string (surfaced as `400 InvalidIndex`) when the array or
/// any entry is malformed.
pub fn parse_vectorizers(raw: Option<&Value>) -> Result<Vec<VectorizerConfig>, String> {
    let Some(value) = raw else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries = value
        .as_array()
        .ok_or_else(|| "Index \"vectorizers\" must be an array.".to_owned())?;
    let mut configs = Vec::with_capacity(entries.len());
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        let obj = entry
            .as_object()
            .ok_or_else(|| "Vectorizer entries must be JSON objects.".to_owned())?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Vectorizer must have a non-empty name.".to_owned())?;
        if !seen.insert(name) {
            return Err(format!("Duplicate vectorizer name {name:?}."));
        }
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Vectorizer {name:?} is missing a \"kind\"."))?;
        if !SUPPORTED_VECTORIZER_KINDS.contains(&kind) {
            return Err(format!(
                "Unsupported vectorizer kind {kind:?} on vectorizer {name:?}; \
                 supported kinds: \"uri\", \"customWebApi\", \"azureOpenAI\", \"aml\"."
            ));
        }
        // The endpoint URI is a wire-format placeholder (never called); require
        // it for the kinds that define one so a misconfigured index fails fast.
        // The SDKs may serialize the parameters object snake_case.
        let uri = match kind {
            "uri" => obj
                .get("parameters")
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str),
            "customWebApi" => obj
                .get("customWebApiParameters")
                .or_else(|| obj.get("custom_web_api_parameters"))
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str),
            _ => None,
        };
        if matches!(kind, "uri" | "customWebApi") && uri.is_none_or(str::is_empty) {
            return Err(format!(
                "Vectorizer {name:?} (kind {kind:?}) must have a non-empty endpoint URI."
            ));
        }
        let source_fields = match obj.get("sourceContext") {
            None | Some(Value::Null) => None,
            Some(context) => {
                let ctx = context.as_object().ok_or_else(|| {
                    format!("Vectorizer {name:?} sourceContext must be a JSON object.")
                })?;
                let source_type = ctx
                    .get("sourceType")
                    .and_then(Value::as_str)
                    .unwrap_or("field");
                if source_type != "field" {
                    return Err(format!(
                        "Unsupported sourceContext sourceType {source_type:?} on vectorizer \
                         {name:?}; only \"field\" is supported."
                    ));
                }
                let fields = ctx
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| {
                                item.as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned)
                                    .ok_or_else(|| {
                                        "Vectorizer sourceContext fields entries must be \
                                         non-empty strings."
                                            .to_owned()
                                    })
                            })
                            .collect::<Result<Vec<String>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                Some(fields)
            }
        };
        configs.push(VectorizerConfig {
            name: name.to_owned(),
            source_fields,
        });
    }
    Ok(configs)
}

/// Looks up a vectorizer by name in a parsed `vectorizers` list.
#[must_use]
pub fn find_vectorizer<'a>(
    configs: &'a [VectorizerConfig],
    name: &str,
) -> Option<&'a VectorizerConfig> {
    configs.iter().find(|c| c.name == name)
}

/// Resolves the vectorizer a vector field uses, if any. The pinned SDKs
/// associate a vectorizer with a field through its profile (the field's
/// `vectorSearchProfile` → the profile's `vectorizer`); a field-level
/// `vectorizer` property, when present, takes precedence (emulator leniency).
/// Returns `None` for raw-vector fields.
#[must_use]
pub fn field_vectorizer(
    definition: &crate::storage::IndexDefinition,
    field: &crate::storage::FieldDefinition,
) -> Option<String> {
    if let Some(name) = &field.vectorizer {
        return Some(name.clone());
    }
    let Some(profile) = &field.vector_search_profile else {
        return None;
    };
    let Ok(config) = super::config::parse_vector_search(definition.vector_search.as_ref()) else {
        return None;
    };
    config.profile_vectorizers.get(profile).cloned()
}

/// Builds the text source for a vectorizer from a document's field map: the
/// values of the vectorizer's `sourceContext.fields` (in order) joined by a
/// space, or — when the vectorizer has no `sourceContext` — the values of all
/// `fallback_fields` (the index's searchable string fields, in schema order)
/// joined by a space. Missing or non-string values contribute nothing.
#[must_use]
pub fn vectorizer_source_text(
    config: &VectorizerConfig,
    fields: &serde_json::Map<String, Value>,
    fallback_fields: &[String],
) -> String {
    let names: Vec<&str> = match &config.source_fields {
        Some(named) => named.iter().map(String::as_str).collect(),
        None => fallback_fields.iter().map(String::as_str).collect(),
    };
    let mut parts = Vec::new();
    for name in names {
        if let Some(value) = fields.get(name).and_then(Value::as_str) {
            if !value.is_empty() {
                parts.push(value);
            }
        }
    }
    parts.join(" ")
}

/// The index's searchable scalar-string field names, in schema order: the
/// text-source fallback for vectorizers configured without a `sourceContext`.
#[must_use]
pub fn searchable_string_fields(fields: &[crate::storage::FieldDefinition]) -> Vec<String> {
    fields
        .iter()
        .filter(|f| f.searchable && f.field_type == crate::storage::FieldType::String)
        .map(|f| f.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn deterministic_same_text_same_vector() {
        assert_eq!(
            text_to_vector("quantum computing applications", 8),
            text_to_vector("quantum computing applications", 8)
        );
    }

    #[test]
    fn dimension_correctness() {
        for dimensions in [1usize, 8, 1536, 3072] {
            assert_eq!(text_to_vector("hello world", dimensions).len(), dimensions);
        }
        assert!(text_to_vector("hello", 0).is_empty());
    }

    #[test]
    fn token_sensitivity() {
        let a = text_to_vector("hello world", 64);
        let same = text_to_vector("hello world", 64);
        let shared = text_to_vector("hello there", 64);
        let disjoint = text_to_vector("goodbye moon", 64);
        assert!((cosine(&a, &same) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &shared) > 0.5);
        assert!(cosine(&a, &disjoint) < 0.5);
    }

    #[test]
    fn analyzer_consistency_stemming_and_stopwords() {
        // Stemming: "Running" and "run" share the same analyzed token.
        assert_eq!(text_to_vector("Running", 16), text_to_vector("run", 16));
        // Stopword removal: "The cat" and "cat" produce the same vector.
        assert_eq!(text_to_vector("The cat", 16), text_to_vector("cat", 16));
    }

    #[test]
    fn zero_vector_for_empty_and_stopword_only() {
        let empty = text_to_vector("", 8);
        assert!(empty.iter().all(|x| *x == 0.0));
        let stopwords = text_to_vector("the and a", 8);
        assert!(stopwords.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn non_zero_vector_is_unit_norm() {
        let v = text_to_vector("the quick brown fox jumps over the lazy dog", 128);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn signed_hashing_produces_both_signs() {
        let text = (0..200)
            .map(|i| format!("token{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let v = text_to_vector(&text, 256);
        assert!(v.iter().any(|x| *x > 0.0));
        assert!(v.iter().any(|x| *x < 0.0));
    }

    #[test]
    fn large_input_vectorizes_quickly() {
        // Sanity check (no hard performance assertion): a 1000-token text to
        // 1536 dimensions must complete well within a generous bound.
        let text = (0..1000)
            .map(|i| format!("token{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let start = std::time::Instant::now();
        let vector = text_to_vector(&text, 1536);
        assert_eq!(vector.len(), 1536);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "vectorization took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn fnv1a_64_known_vector() {
        // FNV-1a 64-bit of the empty string is the offset basis.
        assert_eq!(fnv1a_64(b""), FNV_OFFSET_BASIS);
        // A stable non-trivial value (guards against accidental reordering).
        assert_ne!(fnv1a_64(b"hello"), FNV_OFFSET_BASIS);
    }

    #[test]
    fn parse_vectorizers_validates() {
        let raw = Value::Array(vec![serde_json::json!({
            "name": "embedder",
            "kind": "uri",
            "parameters": {"uri": "https://example.com/embed"},
            "sourceContext": {"sourceType": "field", "fields": ["content"]}
        })]);
        let configs = parse_vectorizers(Some(&raw)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "embedder");
        assert_eq!(
            configs[0].source_fields.as_deref(),
            Some(&["content".to_owned()][..])
        );

        // Absent / null → empty.
        assert!(parse_vectorizers(None)
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty());
        assert!(parse_vectorizers(Some(&Value::Null))
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty());

        // No sourceContext → None (fallback).
        let raw = Value::Array(vec![serde_json::json!({
            "name": "e", "kind": "azureOpenAI", "parameters": {"resourceName": "r"}
        })]);
        let configs = parse_vectorizers(Some(&raw)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(configs[0].source_fields, None);

        // Missing name.
        let bad = Value::Array(vec![
            serde_json::json!({"kind": "uri", "parameters": {"uri": "u"}}),
        ]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // Duplicate name.
        let bad = Value::Array(vec![
            serde_json::json!({"name": "a", "kind": "uri", "parameters": {"uri": "u"}}),
            serde_json::json!({"name": "a", "kind": "uri", "parameters": {"uri": "u"}}),
        ]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // Unsupported kind.
        let bad = Value::Array(vec![serde_json::json!({
            "name": "a", "kind": "none", "parameters": {"uri": "u"}
        })]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // uri kind missing parameters.uri.
        let bad = Value::Array(vec![serde_json::json!({"name": "a", "kind": "uri"})]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // customWebApi missing customWebApiParameters.uri.
        let bad = Value::Array(vec![serde_json::json!({
            "name": "a", "kind": "customWebApi", "customWebApiParameters": {}
        })]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // Bad sourceType.
        let bad = Value::Array(vec![serde_json::json!({
            "name": "a", "kind": "uri", "parameters": {"uri": "u"},
            "sourceContext": {"sourceType": "document", "fields": []}
        })]);
        assert!(parse_vectorizers(Some(&bad)).is_err());
        // Not an array.
        assert!(parse_vectorizers(Some(&serde_json::json!({"name": "a"}))).is_err());
    }

    #[test]
    fn source_text_uses_configured_or_fallback_fields() {
        use serde_json::{Map, Value as V};
        let mut fields = Map::new();
        fields.insert("title".to_owned(), V::String("Hello".to_owned()));
        fields.insert("content".to_owned(), V::String("World".to_owned()));
        fields.insert("other".to_owned(), V::String("Ignored".to_owned()));

        let configured = VectorizerConfig {
            name: "e".to_owned(),
            source_fields: Some(vec!["title".to_owned(), "content".to_owned()]),
        };
        assert_eq!(
            vectorizer_source_text(&configured, &fields, &["other".to_owned()]),
            "Hello World"
        );

        let fallback = VectorizerConfig {
            name: "e".to_owned(),
            source_fields: None,
        };
        assert_eq!(
            vectorizer_source_text(
                &fallback,
                &fields,
                &["title".to_owned(), "other".to_owned()]
            ),
            "Hello Ignored"
        );

        // Empty source fields → empty text.
        let empty = VectorizerConfig {
            name: "e".to_owned(),
            source_fields: Some(Vec::new()),
        };
        assert_eq!(
            vectorizer_source_text(&empty, &fields, &["title".to_owned()]),
            ""
        );
    }
}
