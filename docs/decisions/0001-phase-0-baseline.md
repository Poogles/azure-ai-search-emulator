# 0001 — Phase 0 baseline: toolchain, versions, and configuration conventions

This decision record establishes the repository-wide versions and compatibility conventions adopted during Phase 0. Subsequent decisions are recorded as additional numbered documents in this folder.

## Rust

- Edition: Rust 2021.
- MSRV: Rust 1.95, matching the toolchain provided by the pinned Nix development shell (raised from 1.85 when Tantivy was adopted; the Tantivy 0.26 dependency tree requires a current toolchain).
- Workspace location: `source/rust`.
- Linting: Clippy warnings are denied in CI with `-D warnings`; `unwrap_used` and `expect_used` are denied in the crate configuration.
- Formatting: rustfmt configuration is stored in the repository root and applies to the Rust workspace.

## Test Harnesses

- Python harness location: `source/tests/python`.
- Python version: 3.12 or newer.
- Python dependency manager: Poetry.
- Python test runner: pytest.
- Container integration library: testcontainers.
- Azure Python SDK: `azure-search-documents==11.6.0`.
- C# compatibility tests will be added under `source/tests/csharp` in Phase 3.

## Azure AI Search Compatibility

- Supported API version: `2024-07-01`.
- Authentication rule: accept any non-empty `api-key` header. Missing or empty keys return HTTP 401 with `{"error":{"code":"AuthenticationFailed","message":"..."}}`.

## Configuration

- `EMULATOR_PORT`: default `8080`.
- `EMULATOR_STORAGE__MODE`: `memory` or `file`, default `memory`.
- `EMULATOR_API_VERSIONS`: comma-separated list, default `2024-07-01`.
- `EMULATOR_LOG_LEVEL`: default `info`.
- `EMULATOR_ENABLE_ADMIN`: default `true`; gates `/admin/*` endpoints.
- Default storage is in-memory. File-backed storage is added in Phase 2.
