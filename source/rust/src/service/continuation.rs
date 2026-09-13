//! Opaque continuation tokens for paging.

use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE as BASE64_URL_SAFE};
use base64::Engine as _;

/// A continuation token: the opaque URL-safe `base64(json{filter, orderby,
/// skip, vector_query_hash?})` value carried in `@odata.nextLink` and returned
/// by the client in the `continuation` request parameter. URL-safe encoding
/// keeps the token intact inside query strings (`+`/`/` would otherwise be
/// mangled); decoding still accepts the legacy standard alphabet. Tokens remain
/// valid across document mutations (like Azure; results may shift).
/// `vector_query_hash` binds the token to the `vectorQueries` +
/// `vectorFilterMode` identity when vector search is active.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContinuationToken {
    pub filter: Option<String>,
    pub orderby: Option<String>,
    pub skip: u64,
    #[serde(default)]
    pub vector_query_hash: Option<u64>,
}

impl ContinuationToken {
    #[must_use]
    pub fn encode(&self) -> String {
        // Serialization of this plain-data struct cannot fail; fall back to an
        // empty object (which decodes but carries no paging state) rather than
        // panicking in request handling.
        let json = serde_json::to_string(self).unwrap_or_default();
        BASE64_URL_SAFE.encode(json)
    }

    /// # Errors
    ///
    /// Returns an error string when the token is not valid base64 JSON with
    /// the expected shape.
    pub fn decode(raw: &str) -> Result<Self, String> {
        let bytes = BASE64_URL_SAFE
            .decode(raw.as_bytes())
            .or_else(|_| BASE64.decode(raw.as_bytes()))
            .map_err(|e| format!("continuation token is not valid base64: {e}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|e| format!("continuation token is not valid UTF-8: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("continuation token is not valid JSON: {e}"))
    }
}
