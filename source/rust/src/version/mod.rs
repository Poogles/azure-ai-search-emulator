//! API version adapter.
//!
//! Isolates API-version handling from the service model: the adapter owns the
//! set of supported versions and the validation applied to every
//! Azure-surface request. Version-specific behaviour lives here so that the
//! service layer stays version-agnostic. Today the behaviour is identical
//! across all accepted versions (see `docs/known_differences.md`); the adapter
//! is the single place to introduce per-version differences if a future
//! version requires them.

use crate::error::ApiError;

/// Validates `api-version` query parameters against the supported set.
#[derive(Debug, Clone)]
pub struct VersionAdapter {
    supported: Vec<String>,
}

impl VersionAdapter {
    #[must_use]
    pub fn new(supported: Vec<String>) -> Self {
        Self { supported }
    }

    /// The supported API versions, in configuration order.
    #[must_use]
    pub fn supported_versions(&self) -> &[String] {
        &self.supported
    }

    /// Returns `true` when `version` is in the supported set.
    #[must_use]
    pub fn is_supported(&self, version: &str) -> bool {
        self.supported.iter().any(|v| v == version)
    }

    /// Validates a request's `api-version` query parameter.
    ///
    /// # Errors
    ///
    /// Returns `400 ApiVersionMissing` when the parameter is absent and
    /// `400 ApiVersionUnsupported` when it is not in the supported set.
    pub fn check(&self, version: Option<&str>) -> Result<(), ApiError> {
        match version {
            None => Err(ApiError::bad_request(
                "ApiVersionMissing",
                "The 'api-version' query parameter is required.",
            )),
            Some(v) if !self.is_supported(v) => Err(ApiError::bad_request(
                "ApiVersionUnsupported",
                format!(
                    "API version {v} is not supported. Supported versions: {}.",
                    self.supported.join(", ")
                ),
            )),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> VersionAdapter {
        VersionAdapter::new(vec!["2024-07-01".to_owned(), "2025-03-01".to_owned()])
    }

    #[test]
    fn accepts_supported_versions() {
        let adapter = adapter();
        assert!(adapter.check(Some("2024-07-01")).is_ok());
        assert!(adapter.check(Some("2025-03-01")).is_ok());
        assert!(adapter.is_supported("2024-07-01"));
    }

    #[test]
    fn rejects_missing_version() {
        let adapter = adapter();
        let err = match adapter.check(None) {
            Ok(()) => panic!("expected ApiVersionMissing error"),
            Err(err) => err,
        };
        assert_eq!(err.code, "ApiVersionMissing");
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_unsupported_version_and_lists_supported() {
        let adapter = adapter();
        let err = match adapter.check(Some("1900-01-01")) {
            Ok(()) => panic!("expected ApiVersionUnsupported error"),
            Err(err) => err,
        };
        assert_eq!(err.code, "ApiVersionUnsupported");
        assert!(err.message.contains("2024-07-01"));
        assert!(err.message.contains("2025-03-01"));
    }
}
