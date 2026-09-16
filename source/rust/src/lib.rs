pub mod api;
pub mod config;
pub mod error;
pub mod filter;
pub mod query;
pub mod semantic;
pub mod service;
pub mod storage;
pub mod sync_util;
pub mod vector;
pub mod version;

/// Generates `parse`/`as_str` for a string-backed enum from a single
/// variant→spelling table, so the two can never drift apart. The `both` arm
/// emits `parse` and `as_str` (both `pub`, since `as_str` may be exercised
/// only from tests and a `pub(crate)` method would be dead code in the
/// non-test build); the `parse` arm emits only `parse` at the given
/// visibility (for enums that are never serialized back to their wire
/// spelling).
#[macro_export]
macro_rules! string_enum {
    ($name:ident both { $($variant:ident => $str:literal),+ $(,)? }) => {
        impl $name {
            pub fn parse(raw: &str) -> Option<Self> {
                match raw {
                    $($str => Some(Self::$variant),)+
                    _ => None,
                }
            }
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $str,)+
                }
            }
        }
    };
    ($vis:vis $name:ident parse { $($variant:ident => $str:literal),+ $(,)? }) => {
        impl $name {
            $vis fn parse(raw: &str) -> Option<Self> {
                match raw {
                    $($str => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
    ($name:ident as_str { $($variant:ident => $str:literal),+ $(,)? }) => {
        impl $name {
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $str,)+
                }
            }
        }
    };
}

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
