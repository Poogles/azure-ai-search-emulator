# 0002 — HTTP framework and JSON library

## Decision

- HTTP framework: **Axum 0.8** (Rust).
- JSON serialisation: **serde / serde_json**.
- Async runtime: **Tokio** (multi-threaded).
- Logging: **tracing** + **tracing-subscriber** (JSON output).

## Rationale

Axum provides a type-safe, composable routing model with first-class middleware support, which maps cleanly onto the Azure AI Search endpoint structure (path parameters for index names, query parameters for API version, header-based auth). Its `Router` + `State` pattern gives a single application factory without global state.

serde/serde_json is the de-facto standard for JSON in Rust. The Azure API surface is JSON-only, and `serde_json::Value` provides the dynamic access needed for pass-through of index schemas and document payloads without over-constraining the type system at the HTTP boundary.

Tokio is required by Axum and provides the signal handling needed for graceful shutdown.

## Alternatives considered

- **Actix-web**: comparable performance, but its actor-based model adds conceptual overhead for a stateless HTTP service and its middleware system is less ergonomic for the simple auth/logging layers needed here.
- **Warp**: filter-based composition is elegant but less predictable for routing; Axum's explicit route table is easier to audit against the Azure endpoint spec.
- **jsonwebtoken / custom JSON**: no benefit over serde_json for this use case.
