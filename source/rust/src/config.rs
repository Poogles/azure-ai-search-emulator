//! Configuration loaded from `EMULATOR_*` environment variables.

use std::env;
use std::fmt;

pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_API_VERSION: &str = "2024-07-01";
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// Default cap on accepted vector dimensions (matches Azure's limit;
/// lowerable for memory-constrained CI via `EMULATOR_VECTOR__MAX_DIMENSION`).
pub const DEFAULT_VECTOR_MAX_DIMENSION: usize = 3072;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    Memory,
    File,
}

impl fmt::Display for StorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            StorageMode::Memory => "memory",
            StorageMode::File => "file",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub storage_mode: StorageMode,
    pub api_versions: Vec<String>,
    pub log_level: String,
    pub enable_admin: bool,
    pub max_vector_dimension: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidPort(String),
    InvalidStorageMode(String),
    InvalidApiVersions(String),
    InvalidBool(String),
    InvalidMaxVectorDimension(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidPort(v) => write!(f, "invalid EMULATOR_PORT value: {v:?}"),
            ConfigError::InvalidStorageMode(v) => {
                write!(
                    f,
                    "invalid EMULATOR_STORAGE__MODE value: {v:?} (expected \"memory\" or \"file\")"
                )
            }
            ConfigError::InvalidApiVersions(v) => {
                write!(f, "invalid EMULATOR_API_VERSIONS value: {v:?}")
            }
            ConfigError::InvalidBool(v) => write!(f, "invalid boolean value: {v}"),
            ConfigError::InvalidMaxVectorDimension(v) => {
                write!(f, "invalid EMULATOR_VECTOR__MAX_DIMENSION value: {v:?}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Loads configuration from `EMULATOR_*` environment variables, applying
    /// defaults for any variable that is unset or empty.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] if a set variable has an invalid value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_values(|key| env::var(key).ok())
    }

    /// Builds configuration from a variable lookup, applying defaults for any
    /// variable that is unset or empty. Split out from [`Config::from_env`] so
    /// parsing is testable without mutating process environment.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] if a set variable has an invalid value.
    pub fn from_values<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        // An empty value is treated as unset so that `VAR=` does not override
        // the default.
        let get = |key: &str| lookup(key).filter(|value| !value.is_empty());
        let port = match get("EMULATOR_PORT") {
            None => DEFAULT_PORT,
            Some(raw) => raw
                .parse::<u16>()
                .map_err(|_| ConfigError::InvalidPort(raw))?,
        };

        let storage_mode = match get("EMULATOR_STORAGE__MODE").as_deref() {
            None | Some("memory") => StorageMode::Memory,
            Some("file") => StorageMode::File,
            Some(other) => return Err(ConfigError::InvalidStorageMode(other.to_owned())),
        };

        let api_versions = match get("EMULATOR_API_VERSIONS") {
            None => vec![DEFAULT_API_VERSION.to_owned()],
            Some(raw) => {
                let versions: Vec<String> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect();
                if versions.is_empty() {
                    return Err(ConfigError::InvalidApiVersions(raw));
                }
                versions
            }
        };

        let log_level = get("EMULATOR_LOG_LEVEL").unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

        let enable_admin = match get("EMULATOR_ENABLE_ADMIN").as_deref() {
            None | Some("true" | "1") => true,
            Some("false" | "0") => false,
            Some(other) => {
                return Err(ConfigError::InvalidBool(format!(
                    "EMULATOR_ENABLE_ADMIN={other:?}"
                )))
            }
        };

        let max_vector_dimension = match get("EMULATOR_VECTOR__MAX_DIMENSION") {
            None => DEFAULT_VECTOR_MAX_DIMENSION,
            Some(raw) => raw
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(ConfigError::InvalidMaxVectorDimension(raw))?,
        };

        Ok(Config {
            port,
            storage_mode,
            api_versions,
            log_level,
            enable_admin,
            max_vector_dimension,
        })
    }

    #[must_use]
    pub fn supports_api_version(&self, version: &str) -> bool {
        self.api_versions.iter().any(|v| v == version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_pairs(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_values(|key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    }

    fn ok(result: Result<Config, ConfigError>) -> Config {
        match result {
            Ok(config) => config,
            Err(err) => panic!("expected Ok, got Err: {err}"),
        }
    }

    fn err(result: Result<Config, ConfigError>) -> ConfigError {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(err) => err,
        }
    }

    #[test]
    fn defaults_when_no_env() {
        let config = ok(from_pairs(&[]));
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.storage_mode, StorageMode::Memory);
        assert_eq!(config.api_versions, vec![DEFAULT_API_VERSION.to_owned()]);
        assert_eq!(config.log_level, DEFAULT_LOG_LEVEL);
        assert!(config.enable_admin);
        assert_eq!(config.max_vector_dimension, DEFAULT_VECTOR_MAX_DIMENSION);
        assert!(config.supports_api_version("2024-07-01"));
        assert!(!config.supports_api_version("1900-01-01"));
    }

    #[test]
    fn overrides_and_multi_api_versions() {
        let config = ok(from_pairs(&[
            ("EMULATOR_PORT", "9090"),
            ("EMULATOR_STORAGE__MODE", "file"),
            ("EMULATOR_API_VERSIONS", "2024-07-01, 2025-03-01"),
            ("EMULATOR_LOG_LEVEL", "debug"),
            ("EMULATOR_ENABLE_ADMIN", "false"),
        ]));
        assert_eq!(config.port, 9090);
        assert_eq!(config.storage_mode, StorageMode::File);
        assert_eq!(
            config.api_versions,
            vec!["2024-07-01".to_owned(), "2025-03-01".to_owned()]
        );
        assert_eq!(config.log_level, "debug");
        assert!(!config.enable_admin);
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let config = ok(from_pairs(&[
            ("EMULATOR_PORT", ""),
            ("EMULATOR_ENABLE_ADMIN", ""),
        ]));
        assert_eq!(config.port, DEFAULT_PORT);
        assert!(config.enable_admin);
    }

    #[test]
    fn vector_max_dimension_override() {
        let config = ok(from_pairs(&[("EMULATOR_VECTOR__MAX_DIMENSION", "128")]));
        assert_eq!(config.max_vector_dimension, 128);
        assert!(matches!(
            err(from_pairs(&[("EMULATOR_VECTOR__MAX_DIMENSION", "0")])),
            ConfigError::InvalidMaxVectorDimension(_)
        ));
        assert!(matches!(
            err(from_pairs(&[("EMULATOR_VECTOR__MAX_DIMENSION", "huge")])),
            ConfigError::InvalidMaxVectorDimension(_)
        ));
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(matches!(
            err(from_pairs(&[("EMULATOR_PORT", "not-a-port")])),
            ConfigError::InvalidPort(_)
        ));
        assert!(matches!(
            err(from_pairs(&[("EMULATOR_STORAGE__MODE", "disk")])),
            ConfigError::InvalidStorageMode(_)
        ));
        assert!(matches!(
            err(from_pairs(&[("EMULATOR_API_VERSIONS", " , ")])),
            ConfigError::InvalidApiVersions(_)
        ));
        let config_error = err(from_pairs(&[("EMULATOR_ENABLE_ADMIN", "yes")]));
        assert!(matches!(config_error, ConfigError::InvalidBool(_)));
        assert!(config_error.to_string().contains("EMULATOR_ENABLE_ADMIN"));
    }
}
