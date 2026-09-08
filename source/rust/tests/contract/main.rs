//! HTTP contract tests: the Azure-compatible HTTP surface, including the
//! error paths the SDK cannot exercise (missing/empty api-key, unsupported
//! api-version, operations on a deleted index).

mod admin;
mod common;
mod documents;
mod errors;
mod index_management;
