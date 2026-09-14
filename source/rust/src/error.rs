//! Azure-compatible error responses.
//!
//! All client-facing errors use the Azure error structure:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

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
