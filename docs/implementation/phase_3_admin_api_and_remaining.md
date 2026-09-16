---
status: draft
status_last_reviewed: 2026-09-07
---

# Phase 3 — Admin API and Remaining Elements

## Purpose

Complete the emulator with the operational/admin API, cross-language (C#) compatibility, Azure comparison testing, and the remaining polish items, bringing the project to a releasable state.

## Scope

### Admin API

Operational endpoints, deliberately separate from the Azure-compatible surface (not Azure-shaped, not versioned):

- `GET /health` — liveness/readiness (carried over from Phase 1, formalised here).
- `GET /admin/state` — inspect emulator state: indexes, document counts, storage mode, API versions.
- `POST /admin/reset` — reset complete service state for test isolation (carried over from Phase 1).
- Optional: `GET /admin/metrics` — request counts, error counts, query parse failures.

Admin endpoints should be:

- Gated by `EMULATOR_ENABLE_ADMIN` (default `true`); disabled in non-local environments.
- Clearly documented as non-Azure-compatible.
- Covered by dedicated tests.

### C# SDK compatibility

- Treat the C# SDK as an independent consumer of the HTTP contract (per the initial design).
- **Fixture replay:** replay the HTTP fixtures captured in Phase 1/2 (`source/tests/python/fixtures/`) through the C# SDK to verify the emulator handles the same wire format. This catches serialisation/header differences without requiring a live Azure instance.
- Identify HTTP differences between Python and C# SDK behaviour (headers, serialisation, error handling, API version usage).
- Add C# compatibility tests (a .NET xunit test project in `source/tests/csharp/`) exercising the same supported-operations matrix.
- Fix emulator behaviour where C# reveals gaps; keep the Python suite green.
- Document which behaviours are identical across clients and which differ.

### Azure comparison testing

- A documented, smaller test suite that runs against a real Azure AI Search instance.
- Comparison tests for ambiguous behaviours: response shapes, status codes, error structures, pagination, filter semantics.
- Extend `docs/known_differences.md` (created in Phase 2) with any additional accepted differences found through direct Azure comparison, with rationale.
- CI integration: comparison suite runs on a schedule or manually (requires Azure credentials), clearly separated from the always-green local suite.

### Packaging and release

- Static binary release (cross-compiled for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, `aarch64-darwin`) published as GitHub Release artifacts.
- `cargo install aisearch-emulator` support via crates.io (or internal registry).
- Docker image publishing workflow to GHCR (tagged releases, multi-arch manifest).
- Versioned releases with changelog (`CHANGELOG.md`).

### Documentation and polish

- `README.md` final: quickstart (local, Docker, testcontainers), configuration reference, supported operations, known limitations.
- `docs/known_differences.md` extended with findings from Azure comparison testing (created in Phase 2).
- `docs/decisions/` — record all decisions deferred from earlier phases (SDK versions, API versions, storage defaults).
- Open questions from the initial design resolved or explicitly deferred with rationale.
- Performance sanity check: document baseline throughput/latency for typical operations (no production-scale guarantees, per non-goals).

## Out of scope

- New Azure API surface beyond the Phase 2 matrix (add via the discovery process if requirements change).
- Vector/semantic search, suggest, scoring profiles — unless explicitly required.
- Distributed or highly available operation.
- `POST /admin/reset` (already delivered in Phase 1).

## Deliverables

1. Admin API with tests and configuration controls.
2. C# compatibility test suite (`source/tests/csharp/`) with fixture replay and emulator fixes it drives.
3. Azure comparison test suite and updated `docs/known_differences.md`.
4. Packaged release (static binaries + `cargo install`) and published multi-arch Docker image (GHCR).
5. Final documentation set.

## Checklist

### Admin API

- [ ] `GET /health` reports liveness and readiness.
- [ ] `GET /admin/state` reports indexes, document counts, storage mode, and API versions.
- [ ] `POST /admin/reset` clears all state; verified by test (carried over from Phase 1).
- [ ] `GET /admin/metrics` (if implemented) reports request counts, error counts, query parse failures.
- [ ] Admin endpoints are gated by `EMULATOR_ENABLE_ADMIN` and documented as non-Azure-compatible.
- [ ] Admin endpoints are covered by tests.

### C# compatibility

- [x] C# test project (`source/tests/csharp/`) set up and runs against the emulator image (66 xunit tests, `make test-csharp`).
- [x] HTTP fixtures from Phase 1/2 replayed through the C# SDK successfully (`FixtureReplayTests`).
- [x] C# suite covers the supported-operations matrix (indexes, documents, search, filters, pagination, errors, vectors, knowledge).
- [x] HTTP differences between Python and C# SDKs documented (see below).
- [x] Emulator fixes for C#-discovered gaps landed; Python suite still green.

C#-discovered HTTP differences (all addressed):
- `@search.facets: null` (emitted when no facets requested) breaks the .NET SDK's `SearchResults` deserializer, which calls `EnumerateObject` on the value. The emulator now omits the member instead (see `docs/supported_operations.md`).
- The .NET SDK rejects plain-HTTP endpoints in the client constructor (`AssertHttpsScheme`), unlike the Python SDK for api-key auth. The C# harness passes an https:// URL and transparently downgrades to http via a test-only `HttpPipelineTransport` (`HttpSchemeRewritingTransport`); the wire contract is unchanged.
- Vector index fields must use the REST names `dimensions` / `vectorSearchProfile` in raw JSON (the SDK property names `vectorSearchDimensions` / `vectorSearchProfileName` are not parsed from raw payloads).

### Azure comparison

- [ ] Comparison suite runs against a real Azure AI Search instance.
- [ ] Comparison covers response shapes, status codes, error structures, pagination, and filter semantics.
- [ ] `docs/known_differences.md` lists accepted differences with rationale.
- [ ] Comparison suite is separated from the always-green local CI (scheduled/manual, credentials required).

### Packaging and release

- [ ] Static binaries published for all target platforms (GitHub Release).
- [ ] `cargo install aisearch-emulator` works from crates.io (or internal registry).
- [ ] Multi-arch Docker image published to GHCR with release tags.
- [ ] Release process documented (versioning, changelog).

### Documentation

- [ ] README quickstart works as written (local, Docker, testcontainers).
- [ ] Configuration reference complete.
- [ ] Supported operations and known limitations documented.
- [ ] Open questions from the initial design resolved or explicitly deferred.
- [ ] Baseline performance characteristics documented.

### Quality gates

- [ ] All suites pass: unit, contract, SDK (Python), C#, e2e.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] CI is green.
- [ ] A tagged release builds and publishes successfully (binary + Docker image).

## Exit criteria

The emulator is releasable: an application can use it locally (`cargo run` or installed binary), in Docker (GHCR image), and in CI from both Python and C# SDKs; operational state can be inspected and reset via the admin API; a documented comparison suite continues to validate behaviour against real Azure AI Search; and the documentation set is complete and accurate.
