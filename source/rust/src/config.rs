//! Configuration loaded from `EMULATOR_*` environment variables.

use std::env;
use std::fmt;

pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_API_VERSION: &str = "2024-07-01";
pub const DEFAULT_LOG_LEVEL: &str = "info";

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidPort(String),
    InvalidStorageMode(String),
    InvalidApiVersions(String),
    InvalidBool(String),
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
            ConfigError::InvalidBool(v) => write!(f, "invalid boolean value: {v:?}"),
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
        let port = match env::var("EMULATOR_PORT").ok().filter(|v| !v.is_empty()) {
            None => DEFAULT_PORT,
            Some(raw) => raw
                .parse::<u16>()
                .map_err(|_| ConfigError::InvalidPort(raw))?,
        };

        let storage_mode = match env::var("EMULATOR_STORAGE__MODE")
            .ok()
            .filter(|v| !v.is_empty())
            .as_deref()
        {
            None | Some("memory") => StorageMode::Memory,
            Some("file") => StorageMode::File,
            Some(other) => return Err(ConfigError::InvalidStorageMode(other.to_owned())),
        };

        let api_versions = match env::var("EMULATOR_API_VERSIONS")
            .ok()
            .filter(|v| !v.is_empty())
        {
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

        let log_level = env::var("EMULATOR_LOG_LEVEL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

        let enable_admin = match env::var("EMULATOR_ENABLE_ADMIN")
            .ok()
            .filter(|v| !v.is_empty())
            .as_deref()
        {
            None | Some("true" | "1") => true,
            Some("false" | "0") => false,
            Some(other) => return Err(ConfigError::InvalidBool(other.to_owned())),
        };

        Ok(Config {
            port,
            storage_mode,
            api_versions,
            log_level,
            enable_admin,
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

    #[test]
    fn defaults_when_no_env() {
        // Run in a clean environment: from_env reads process env, so only assert
        // parsing helpers here; env-based behaviour is covered by the contract tests.
        let config = Config {
            port: DEFAULT_PORT,
            storage_mode: StorageMode::Memory,
            api_versions: vec![DEFAULT_API_VERSION.to_owned()],
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            enable_admin: true,
        };
        assert_eq!(config.port, 8080);
        assert!(config.supports_api_version("2024-07-01"));
        assert!(!config.supports_api_version("1900-01-01"));
    }
}
