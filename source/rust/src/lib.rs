pub mod api;
pub mod config;
pub mod error;
pub mod filter;
pub mod query;
pub mod service;
pub mod storage;
pub mod sync_util;
pub mod vector;
pub mod version;

/// Shared test helpers (test-only; not compiled into the library).
#[cfg(test)]
pub mod testutil {
    /// Unwraps a `Result`, panicking with the error's debug representation
    /// when it is `Err`.
    ///
    /// # Panics
    ///
    /// Panics when `result` is `Err`.
    pub fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(err) => panic!("expected Ok, got Err: {err:?}"),
        }
    }

    /// Unwraps a `Result`'s error, panicking when it is `Ok`.
    ///
    /// # Panics
    ///
    /// Panics when `result` is `Ok`.
    pub fn err<T, E: std::fmt::Debug>(result: Result<T, E>) -> E {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(err) => err,
        }
    }
}
