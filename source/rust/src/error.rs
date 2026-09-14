//! Azure-compatible error responses.
//!
//! All client-facing errors use the Azure error structure:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// The Azure error `code` for a client-facing error. The wire spelling is the
/// variant name; [`ErrorCode::as_str`] is the single source of truth so the
/// response body and any echo can never drift from the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    AuthenticationFailed,
    ResourceNotFound,
    InternalError,
    InvalidQuery,
    InvalidIndex,
    InvalidIndexName,
    InvalidRequest,
    InvalidDocuments,
    InvalidAlias,
    InvalidSynonymMap,
    InvalidKnowledgeSource,
    InvalidKnowledgeBase,
    IndexAlreadyExists,
    SynonymMapAlreadyExists,
    AliasAlreadyExists,
    KnowledgeSourceAlreadyExists,
    KnowledgeBaseAlreadyExists,
    UnsupportedQuery,
    UnsupportedAction,
    ApiVersionMissing,
    ApiVersionUnsupported,
}

crate::string_enum!(ErrorCode as_str {
    AuthenticationFailed => "AuthenticationFailed",
    ResourceNotFound => "ResourceNotFound",
    InternalError => "InternalError",
    InvalidQuery => "InvalidQuery",
    InvalidIndex => "InvalidIndex",
    InvalidIndexName => "InvalidIndexName",
    InvalidRequest => "InvalidRequest",
    InvalidDocuments => "InvalidDocuments",
    InvalidAlias => "InvalidAlias",
    InvalidSynonymMap => "InvalidSynonymMap",
    InvalidKnowledgeSource => "InvalidKnowledgeSource",
    InvalidKnowledgeBase => "InvalidKnowledgeBase",
    IndexAlreadyExists => "IndexAlreadyExists",
    SynonymMapAlreadyExists => "SynonymMapAlreadyExists",
    AliasAlreadyExists => "AliasAlreadyExists",
    KnowledgeSourceAlreadyExists => "KnowledgeSourceAlreadyExists",
    KnowledgeBaseAlreadyExists => "KnowledgeBaseAlreadyExists",
    UnsupportedQuery => "UnsupportedQuery",
    UnsupportedAction => "UnsupportedAction",
    ApiVersionMissing => "ApiVersionMissing",
    ApiVersionUnsupported => "ApiVersionUnsupported",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn authentication_failed(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: ErrorCode::AuthenticationFailed,
            message: message.into(),
        }
    }

    pub fn bad_request(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::bad_request(ErrorCode::InvalidQuery, message)
    }

    pub fn invalid_index(message: impl Into<String>) -> Self {
        Self::bad_request(ErrorCode::InvalidIndex, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            code: ErrorCode::ResourceNotFound,
            message: message.into(),
        }
    }

    pub fn conflict(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::CONFLICT,
            code,
            message: message.into(),
        }
    }

    pub fn unsupported(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: ErrorCode::InternalError,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
            }
        }));
        (self.status, body).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.status,
            self.code.as_str(),
            self.message
        )
    }
}

impl std::error::Error for ApiError {}

/// Serializes a plain-data response struct to a [`Value`], falling back to
/// `Null` rather than panicking in request handling. Serialization of these
/// structs cannot fail in practice; the fallback keeps the (infallible)
/// `to_value` converters total without `unwrap`/`expect` (both denied).
#[must_use]
pub fn to_value_or_null<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

impl From<crate::storage::StorageError> for ApiError {
    fn from(error: crate::storage::StorageError) -> Self {
        match &error {
            // Storage reads/writes that fail through `?` only ever produce
            // `IndexNotFound`; preserve the storage message verbatim.
            crate::storage::StorageError::IndexNotFound(_) => Self::not_found(error.to_string()),
            // Never produced through `?` today (`create_index` matches
            // explicitly), but map it to the same conflict the explicit arm
            // produces so a future `?` cannot turn it into a 404.
            crate::storage::StorageError::IndexAlreadyExists(name) => Self::conflict(
                ErrorCode::IndexAlreadyExists,
                format!("An index with name {name:?} already exists."),
            ),
        }
    }
}
