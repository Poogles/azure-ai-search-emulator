//! API version adapter.
//!
//! Isolates API-version handling from the service model: the adapter owns the
//! set of supported versions and the validation applied to every
//! Azure-surface request. Version-specific behaviour lives here so that the
//! service layer stays version-agnostic. Behaviour is identical across all
//! accepted versions (see `docs/known_differences.md`); the adapter is the
//! single place to introduce per-version differences if a future version
//! requires them.
//!
//! Acceptance is floor-based: any version on or after the earliest configured
//! version is accepted, so newer SDK defaults keep working without
//! reconfiguration. Versions below the floor are rejected explicitly.

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

    /// The earliest supported version (the acceptance floor), if any
    /// configured version carries a `YYYY-MM-DD` date prefix.
    fn floor(&self) -> Option<&str> {
        self.supported
            .iter()
            .filter_map(|v| version_date(v).map(|d| (d, v.as_str())))
            .min_by(|a, b| a.0.cmp(b.0))
            .map(|(_, v)| v)
    }

    /// Returns `true` when `version` is accepted: an exact configured match,
    /// or any well-formed version on or after the acceptance floor.
    #[must_use]
    pub fn is_supported(&self, version: &str) -> bool {
        if self.supported.iter().any(|v| v == version) {
            return true;
        }
        let (Some(candidate), Some(floor)) =
            (version_date(version), self.floor().and_then(version_date))
        else {
            return false;
        };
        candidate >= floor
    }

    /// Validates a request's `api-version` query parameter.
    ///
    /// # Errors
    ///
    /// Returns `400 ApiVersionMissing` when the parameter is absent and
    /// `400 ApiVersionUnsupported` when it is below the acceptance floor.
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

/// Extracts the `YYYY-MM-DD` date prefix of an API version, if well-formed
/// (with month `01`-`12` and day `01`-`31`). Suffixes (e.g. `-preview`) are
/// ignored so preview versions compare by their date.
pub(crate) fn version_date(version: &str) -> Option<&str> {
    let date = version.get(..10)?;
    let bytes = date.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && date[..4].chars().all(|c| c.is_ascii_digit())
        && date[5..7].chars().all(|c| c.is_ascii_digit())
        && date[8..10].chars().all(|c| c.is_ascii_digit())
    {
        let month: u32 = date[5..7].parse().ok()?;
        let day: u32 = date[8..10].parse().ok()?;
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            return Some(date);
        }
    }
    None
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

    #[test]
    fn accepts_versions_on_or_after_the_floor() {
        let adapter = adapter();
        // Newer than every configured version: accepted by the floor rule.
        assert!(adapter.check(Some("2026-04-01")).is_ok());
        assert!(adapter.is_supported("2026-04-01"));
        // Between configured versions: accepted.
        assert!(adapter.check(Some("2024-10-01")).is_ok());
        // Preview-suffixed versions compare by date.
        assert!(adapter.check(Some("2025-03-01-preview")).is_ok());
        // Below the floor: rejected.
        assert!(adapter.check(Some("2024-06-30")).is_err());
        assert!(!adapter.is_supported("2024-06-30"));
        // Malformed versions are rejected.
        assert!(adapter.check(Some("not-a-version")).is_err());
        assert!(!adapter.is_supported("not-a-version"));
    }

    #[test]
    fn rejects_versions_with_impossible_dates() {
        let adapter = adapter();
        // Well-shaped but calendrically impossible dates are not versions.
        for version in [
            "2024-13-01",
            "2024-00-10",
            "2024-07-32",
            "2024-07-00",
            "9999-99-99",
        ] {
            assert!(
                !adapter.is_supported(version),
                "expected rejection: {version}"
            );
            assert!(
                adapter.check(Some(version)).is_err(),
                "expected rejection: {version}"
            );
        }
    }
}
