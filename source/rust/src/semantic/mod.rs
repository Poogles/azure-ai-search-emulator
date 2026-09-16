//! Semantic search post-processing: extractive answers, captions, and
//! reranker score normalization. No model inference — all operations are
//! deterministic text processing over the full-text result set.
//!
//! Wire formats (see `docs/phase_2_3_semantic_search.md`):
//! - Index schema: the SDK `prioritizedFields` format (title, content, and
//!   keywords fields) and the canonical `priorities` / `sources` format are
//!   both accepted and normalized to field lists for answers and captions.
//! - Search request: the flat SDK format (`queryType: "semantic"` with
//!   `semanticConfiguration`, `answers` / `captions` compound strings) and
//!   the nested `semantic` object are both accepted.
//! - Search response: top-level `@search.answers`, per-document
//!   `@search.captions`, and per-document `@search.rerankerScore` — the
//!   shapes the official SDKs deserialize.

pub mod answers;
pub mod captions;
pub mod reranker;

use serde_json::Value;

use crate::error::{ApiError, ErrorCode};
use crate::storage::FieldDefinition;

/// Default answers count when answers are requested without an explicit count.
pub const DEFAULT_ANSWERS_COUNT: usize = 3;

/// Default captions count when captions are requested without an explicit count.
pub const DEFAULT_CAPTIONS_COUNT: usize = 1;

/// Default cap on accepted `answers.count` values
/// (`EMULATOR_SEMANTIC__MAX_ANSWERS`).
pub const DEFAULT_MAX_ANSWERS: usize = 5;

/// Default cap on accepted `captions.count` values
/// (`EMULATOR_SEMANTIC__MAX_CAPTIONS`).
pub const DEFAULT_MAX_CAPTIONS: usize = 3;

/// Caps on accepted semantic `count` values, from
/// `EMULATOR_SEMANTIC__MAX_ANSWERS` / `EMULATOR_SEMANTIC__MAX_CAPTIONS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticLimits {
    /// Maximum accepted `answers.count` (nested validation, flat clamping).
    pub max_answers: usize,
    /// Maximum accepted `captions.count` (nested validation).
    pub max_captions: usize,
}

impl Default for SemanticLimits {
    fn default() -> Self {
        Self {
            max_answers: DEFAULT_MAX_ANSWERS,
            max_captions: DEFAULT_MAX_CAPTIONS,
        }
    }
}

// ---------------------------------------------------------------------------
// Index schema types
// ---------------------------------------------------------------------------

/// The `semantic` block of an index definition (parsed at index creation).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticConfig {
    pub configurations: Vec<SemanticConfiguration>,
}

/// One named semantic configuration, normalized to the field lists that feed
/// answer and caption extraction (in priority order).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticConfiguration {
    pub name: String,
    /// Fields (in priority order) that feed answer extraction.
    pub answers_fields: Vec<String>,
    /// Fields (in priority order) that feed caption extraction.
    pub captions_fields: Vec<String>,
}

/// Looks up a top-level or complex-path field (`Address/City`) in the index
/// schema. Mirrors [`crate::storage::IndexDefinition::field_path`], which is
/// unavailable while the definition is still being parsed.
fn find_field<'a>(fields: &'a [FieldDefinition], path: &str) -> Option<&'a FieldDefinition> {
    let mut segments = path.split('/');
    let first = segments.next().unwrap_or("");
    if first.is_empty() {
        return None;
    }
    let mut current = fields.iter().find(|f| f.name == first)?;
    for segment in segments {
        current = current.subfields.iter().find(|f| f.name == segment)?;
    }
    Some(current)
}

/// Whether the field can supply answer/caption text: a string (or string
/// collection, for keywords-style fields).
fn is_text_field(field: &FieldDefinition) -> bool {
    matches!(
        field.field_type,
        crate::storage::FieldType::String | crate::storage::FieldType::CollectionString
    )
}

/// Pushes `field` onto `list` unless it is already present (first occurrence
/// wins, preserving priority order).
fn push_unique(list: &mut Vec<String>, field: &str) {
    if !list.iter().any(|f| f == field) {
        list.push(field.to_owned());
    }
}

impl SemanticConfig {
    /// Parses the `semantic` block from an index definition JSON value,
    /// validating it against the index `fields`. Returns `None` when the
    /// block is absent. The index-level `defaultConfiguration` is accepted
    /// but inert (every semantic query must still name its configuration).
    ///
    /// # Errors
    ///
    /// Returns an error string (surfaced as `400 InvalidIndex`) when the
    /// block is present but malformed.
    pub fn from_json(value: &Value, fields: &[FieldDefinition]) -> Result<Option<Self>, String> {
        let Some(obj) = value.as_object() else {
            return Err("The 'semantic' property must be a JSON object.".to_owned());
        };
        let Some(configs_raw) = obj.get("configurations") else {
            return Err("The 'semantic' property requires a 'configurations' array.".to_owned());
        };
        let configs_arr = configs_raw
            .as_array()
            .ok_or_else(|| "The 'semantic.configurations' property must be an array.".to_owned())?;
        if configs_arr.is_empty() {
            return Err("Semantic configuration must have at least one entry.".to_owned());
        }
        let mut configurations = Vec::with_capacity(configs_arr.len());
        for entry in configs_arr {
            configurations.push(SemanticConfiguration::from_json(entry, fields)?);
        }
        let mut names = configurations
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        let mut previous: Option<&str> = None;
        for name in names {
            if Some(name) == previous {
                return Err(format!("Duplicate semantic configuration name {name:?}."));
            }
            previous = Some(name);
        }
        Ok(Some(Self { configurations }))
    }

    /// Looks up a configuration by name.
    #[must_use]
    pub fn configuration(&self, name: &str) -> Option<&SemanticConfiguration> {
        self.configurations.iter().find(|c| c.name == name)
    }
}

impl SemanticConfiguration {
    fn from_json(value: &Value, fields: &[FieldDefinition]) -> Result<Self, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "Each semantic configuration must be a JSON object.".to_owned())?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "Each semantic configuration requires a non-empty 'name' string.".to_owned()
            })?
            .to_owned();
        let has_canonical = obj.get("priorities").is_some() || obj.get("sources").is_some();
        let (answers_fields, captions_fields) = if has_canonical {
            Self::canonical_fields(obj, fields)?
        } else if let Some(raw) = obj.get("prioritizedFields") {
            Self::sdk_fields(raw, fields)?
        } else {
            (Vec::new(), Vec::new())
        };
        if let Some(raw) = obj.get("reranker") {
            if !raw.is_null() {
                let reranker_name = raw.get("name").and_then(Value::as_str).ok_or_else(|| {
                    "The 'reranker' property requires a 'name' string.".to_owned()
                })?;
                if reranker_name != "standard" {
                    return Err(format!(
                        "Unknown reranker {reranker_name:?}; only 'standard' is supported."
                    ));
                }
            }
        }
        if let Some(raw) = obj.get("rescorers") {
            let empty = raw.as_array().is_some_and(Vec::is_empty);
            if !empty {
                return Err("Rescorers are not supported.".to_owned());
            }
        }
        Ok(Self {
            name,
            answers_fields,
            captions_fields,
        })
    }

    /// Parses the canonical `priorities` / `sources` format into
    /// `(answers_fields, captions_fields)`.
    fn canonical_fields(
        obj: &serde_json::Map<String, Value>,
        fields: &[FieldDefinition],
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let mut answers_fields = Vec::new();
        let mut captions_fields = Vec::new();
        if let Some(raw) = obj.get("priorities") {
            let priorities = raw
                .as_object()
                .ok_or_else(|| "The 'priorities' property must be a JSON object.".to_owned())?;
            let mut present = false;
            for (key, list) in [
                ("answers", &mut answers_fields),
                ("captions", &mut captions_fields),
            ] {
                if let Some(raw) = priorities.get(key) {
                    let names = raw.as_array().ok_or_else(|| {
                        format!("Semantic priorities {key:?} must be an array of field names.")
                    })?;
                    if names.is_empty() {
                        return Err(format!(
                            "Semantic priorities {key:?} must be a non-empty array of field names."
                        ));
                    }
                    for entry in names {
                        let field_name =
                            entry.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                                format!(
                                    "Semantic priorities {key:?} must be an array of field names."
                                )
                            })?;
                        let field = find_field(fields, field_name);
                        if field.is_none_or(|f| !f.retrievable) {
                            return Err(format!(
                                "Semantic priority field {field_name:?} must be retrievable."
                            ));
                        }
                        push_unique(list, field_name);
                    }
                    present = true;
                }
            }
            if !present {
                return Err(
                    "Semantic priorities must contain a non-empty 'answers' and/or 'captions' array."
                        .to_owned(),
                );
            }
        }
        if let Some(raw) = obj.get("sources") {
            let sources = raw
                .as_array()
                .ok_or_else(|| "The 'sources' property must be an array.".to_owned())?;
            for entry in sources {
                let source = entry
                    .as_object()
                    .ok_or_else(|| "Each semantic source must be a JSON object.".to_owned())?;
                let source_name = source
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "Each semantic source requires a non-empty 'name' string.".to_owned()
                    })?;
                let source_type = source.get("type").and_then(Value::as_str).unwrap_or("text");
                if source_type != "text" {
                    return Err(format!(
                        "Semantic source {source_name:?} has unsupported type {source_type:?}; only 'text' is supported."
                    ));
                }
                let field_name = source
                    .get("field")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        format!("Semantic source {source_name:?} requires a 'field' string.")
                    })?;
                let field = find_field(fields, field_name);
                if field.is_none_or(|f| !f.searchable || !is_text_field(f)) {
                    return Err(format!(
                        "Semantic source field {field_name:?} must be searchable."
                    ));
                }
            }
        }
        Ok((answers_fields, captions_fields))
    }

    /// Normalizes the SDK `prioritizedFields` format into
    /// `(answers_fields, captions_fields)`: the title field feeds answers and
    /// captions (context role), content fields feed answers and captions, and
    /// keywords fields feed answers. Validation is lenient: referenced fields
    /// must exist, without the canonical searchable/retrievable checks.
    fn sdk_fields(
        value: &Value,
        fields: &[FieldDefinition],
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "The 'prioritizedFields' property must be a JSON object.".to_owned())?;
        let mut answers_fields = Vec::new();
        let mut captions_fields = Vec::new();
        let check_exists = |field_name: &str| -> Result<(), String> {
            if find_field(fields, field_name).is_none() {
                return Err(format!(
                    "Semantic priority field {field_name:?} must be retrievable."
                ));
            }
            Ok(())
        };
        if let Some(title) = obj.get("titleField") {
            if !title.is_null() {
                let field_name = title
                    .get("fieldName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "titleField requires a non-empty 'fieldName' string.".to_owned()
                    })?;
                check_exists(field_name)?;
                push_unique(&mut answers_fields, field_name);
                push_unique(&mut captions_fields, field_name);
            }
        }
        for (key, answers, captions) in [
            ("prioritizedContentFields", true, true),
            ("prioritizedKeywordsFields", true, false),
        ] {
            if let Some(raw) = obj.get(key) {
                let entries = raw
                    .as_array()
                    .ok_or_else(|| format!("The {key:?} property must be an array."))?;
                for entry in entries {
                    let field_name = entry
                        .get("fieldName")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            format!("Each {key:?} entry requires a non-empty 'fieldName' string.")
                        })?;
                    check_exists(field_name)?;
                    if answers {
                        push_unique(&mut answers_fields, field_name);
                    }
                    if captions {
                        push_unique(&mut captions_fields, field_name);
                    }
                }
            }
        }
        Ok((answers_fields, captions_fields))
    }
}

// ---------------------------------------------------------------------------
// Request-side types
// ---------------------------------------------------------------------------

/// The semantic search options of a search request (from the nested `semantic`
/// object or the flat SDK parameters).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticQuery {
    pub configuration: String,
    /// Conversational context (`queryContext.questions`); biases answer
    /// selection. The flat SDK format has no equivalent (its `semanticQuery`
    /// is inert).
    pub questions: Vec<String>,
    pub answers: Option<SemanticQueryAnswers>,
    pub captions: Option<SemanticQueryCaptions>,
    pub error_handling: SemanticErrorHandling,
}

/// The `answers` sub-block: how many extractive answers to return.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticQueryAnswers {
    pub count: usize,
}

/// The `captions` sub-block: how many captions to return, with optional
/// nested per-caption answers.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticQueryCaptions {
    pub count: usize,
    pub answers: Option<SemanticQueryAnswers>,
}

/// The `semanticErrorHandling` mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SemanticErrorHandling {
    /// Return an error when semantic processing fails (default).
    #[default]
    ThrowError,
    /// Return partial results when semantic processing fails.
    ReturnPartialResults,
}

impl SemanticQuery {
    /// Parses the nested `semantic` object from a search request body.
    /// Returns `None` when the property is absent or null.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`400 InvalidQuery`) when the property is
    /// present but malformed.
    pub fn from_json(value: &Value, limits: &SemanticLimits) -> Result<Option<Self>, ApiError> {
        if value.is_null() {
            return Ok(None);
        }
        let Some(obj) = value.as_object() else {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "The 'semantic' property must be a JSON object.",
            ));
        };
        let configuration = obj
            .get("semanticConfiguration")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ApiError::bad_request(
                    ErrorCode::InvalidQuery,
                    "The 'semantic' property requires a 'semanticConfiguration' string.",
                )
            })?
            .to_owned();
        let questions = match obj.get("queryContext") {
            None | Some(Value::Null) => Vec::new(),
            Some(raw) => {
                let context = raw.as_object().ok_or_else(|| {
                    ApiError::bad_request(
                        ErrorCode::InvalidQuery,
                        "The 'semantic.queryContext' property must be a JSON object.",
                    )
                })?;
                match context.get("questions") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(raw) => raw
                        .as_array()
                        .ok_or_else(|| {
                            ApiError::bad_request(
                                ErrorCode::InvalidQuery,
                                "The 'semantic.queryContext.questions' property must be an array of strings.",
                            )
                        })?
                        .iter()
                        .map(|q| {
                            q.as_str().map(str::to_owned).ok_or_else(|| {
                                ApiError::bad_request(
                                    ErrorCode::InvalidQuery,
                                    "The 'semantic.queryContext.questions' property must be an array of strings.",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                }
            }
        };
        let answers = match obj.get("answers") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(Self::parse_answers_block(raw, "semantic.answers", limits)?),
        };
        let captions = match obj.get("captions") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(Self::parse_captions_block(raw, limits)?),
        };
        let error_handling = Self::parse_error_handling(obj.get("semanticErrorHandling"))?;
        // `semanticMaxWaitInMilliseconds` is accepted but inert (operations
        // are synchronous; no timeout is needed).
        Ok(Some(Self {
            configuration,
            questions,
            answers,
            captions,
            error_handling,
        }))
    }

    /// Parses the `captions` sub-block (`{count, type, answers}`).
    fn parse_captions_block(
        raw: &Value,
        limits: &SemanticLimits,
    ) -> Result<SemanticQueryCaptions, ApiError> {
        let block = raw.as_object().ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "The 'semantic.captions' property must be a JSON object.",
            )
        })?;
        let caption_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("extractive");
        if caption_type != "extractive" {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                format!(
                    "Unsupported semantic caption type {caption_type:?}; only 'extractive' is supported."
                ),
            ));
        }
        let count = Self::parse_count(
            block.get("count"),
            "semantic.captions.count",
            DEFAULT_CAPTIONS_COUNT,
            limits.max_captions,
            "Semantic captions count",
        )?;
        let nested = match block.get("answers") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(Self::parse_answers_block(
                raw,
                "semantic.captions.answers",
                limits,
            )?),
        };
        Ok(SemanticQueryCaptions {
            count,
            answers: nested,
        })
    }

    /// Parses an `answers`-shaped block (`{count, type}`), used for top-level
    /// `answers` and nested `captions.answers`.
    fn parse_answers_block(
        raw: &Value,
        label: &str,
        limits: &SemanticLimits,
    ) -> Result<SemanticQueryAnswers, ApiError> {
        let block = raw.as_object().ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidQuery,
                format!("The {label:?} property must be a JSON object."),
            )
        })?;
        let answer_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("extractive");
        if answer_type != "extractive" {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                format!(
                    "Unsupported semantic answer type {answer_type:?}; only 'extractive' is supported."
                ),
            ));
        }
        let count = Self::parse_count(
            block.get("count"),
            label,
            DEFAULT_ANSWERS_COUNT,
            limits.max_answers,
            "Semantic answers count",
        )?;
        Ok(SemanticQueryAnswers { count })
    }

    /// Parses a `count` value: defaults when absent, rejects non-integers and
    /// values outside `1..=max`.
    fn parse_count(
        raw: Option<&Value>,
        label: &str,
        default: usize,
        max: usize,
        message: &str,
    ) -> Result<usize, ApiError> {
        match raw {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Number(n)) => {
                let count = n
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| {
                        ApiError::bad_request(
                            ErrorCode::InvalidQuery,
                            format!("{message} must be 1-{max}."),
                        )
                    })?;
                if (1..=max).contains(&count) {
                    Ok(count)
                } else {
                    Err(ApiError::bad_request(
                        ErrorCode::InvalidQuery,
                        format!("{message} must be 1-{max}."),
                    ))
                }
            }
            Some(_) => Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                format!("The {label:?} property must be a positive integer, 1-{max}."),
            )),
        }
    }

    /// Parses `semanticErrorHandling` / `semanticErrorMode`: `throwError` /
    /// `fail` (default) select [`SemanticErrorHandling::ThrowError`],
    /// `returnPartialResults` / `partial` select
    /// [`SemanticErrorHandling::ReturnPartialResults`].
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`400 InvalidQuery`) for unknown modes or
    /// non-string values.
    pub fn parse_error_handling(raw: Option<&Value>) -> Result<SemanticErrorHandling, ApiError> {
        match raw {
            None | Some(Value::Null) => Ok(SemanticErrorHandling::ThrowError),
            Some(Value::String(mode)) => match mode.as_str() {
                "throwError" | "fail" => Ok(SemanticErrorHandling::ThrowError),
                "returnPartialResults" | "partial" => {
                    Ok(SemanticErrorHandling::ReturnPartialResults)
                }
                _ => Err(ApiError::bad_request(
                    ErrorCode::InvalidQuery,
                    format!("Invalid semanticErrorHandling {mode:?}."),
                )),
            },
            Some(_) => Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "The 'semanticErrorHandling' property must be a string.",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// A single top-level extractive answer: the text, its source document, and
/// its normalized score.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticAnswer {
    pub key: String,
    pub text: String,
    pub score: f32,
    pub highlights: String,
}

/// A nested per-caption answer (from `captions.answers`).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticCaptionAnswer {
    pub text: String,
    pub score: f32,
    pub highlights: String,
}

/// A single caption: representative text with highlights and optional nested
/// answers.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticCaption {
    pub text: String,
    pub highlights: String,
    pub answers: Vec<SemanticCaptionAnswer>,
}

/// The semantic post-processing result for one document: captions and the
/// reranker score. Answers are response-level (see [`SemanticResult`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocumentSemanticResult {
    pub captions: Vec<SemanticCaption>,
    pub reranker_score: f32,
    /// Whether the document was part of a semantic search (controls
    /// `@search.rerankerScore` emission; always true when constructed by
    /// the semantic pipeline).
    pub is_semantic: bool,
}

/// The semantic post-processing result for a search response: top-level
/// answers plus per-document captions and reranker scores.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticResult {
    /// The global top-N answers, in score order.
    pub answers: Vec<SemanticAnswer>,
    /// Per-document semantic results, keyed by document key.
    pub documents: std::collections::BTreeMap<String, DocumentSemanticResult>,
}
