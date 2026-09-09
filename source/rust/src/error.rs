//! Azure-compatible error responses.
//!
//! All client-facing errors use the Azure error structure:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn authentication_failed(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "AuthenticationFailed".to_owned(),
            message: message.into(),
        }
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            code: "ResourceNotFound".to_owned(),
            message: message.into(),
        }
    }

    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::CONFLICT,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn unsupported(code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "InternalError".to_owned(),
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        }));
        (self.status, body).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}
