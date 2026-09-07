---
status: draft
status_last_reviewed: 2026-09-07
---

# Phase 2 — Full API to Production Usage Standard

## Purpose

Expand the Phase 1 scaffold into a complete, production-usage-standard implementation of the Azure AI Search API surface required by our applications, with a full contract test suite and SDK compatibility tests.

## Scope

### API surface discovery (first task)

Before implementing, complete the discovery process from the initial design:

1. Inventory all Azure AI Search client classes and methods used by our applications.
2. Capture the HTTP requests/responses those methods generate.
3. Record the required error conditions.
4. Record the result as a supported-operations matrix (operation, SDK method, HTTP request, status, supported/unsupported).

Everything below implements that matrix. Unsupported operations must fail explicitly with a clear error, never silently approximate.

### Index management

- Create, get, list, update (supported properties), delete indexes.
- Full schema validation at creation time: field name, type, key, searchable, filterable, sortable, facetable, collection/complex fields.
- Reject unsupported field types and capabilities explicitly.
- Distinguish index-not-found from empty index in responses and errors.
- Synchronous completion of operations (asynchronous behaviour only where required for compatibility).

### Document management

- Upload, merge, merge-or-upload, delete, and batch operations.
- Document validation against the index schema (types, key presence, collection shapes).
- Per-document error reporting in batch responses (successful vs failed items) matching SDK expectations.
- Atomicity at the level the API exposes.

### Query engine

- Internal query representation decoupled from the HTTP representation.
- Full-text search: simple terms, field-specific search, boolean operators as required by the matrix.
- Filter parser producing an internal expression tree:
  - `and` / `or` / `not`, parentheses.
  - `eq`, `ne`, `gt`, `ge`, `lt`, `le`.
  - String, numeric, and boolean comparisons; collection filtering (`any`/`all`) where required.
  - Unsupported filter syntax rejected with a clear error.
- Ordering (`$orderby`) on sortable fields.
- Pagination: top/offset and continuation tokens (`@odata.nextLink`). Token scheme: `base64(json{filter, orderby, skip, state_version})` where `state_version` is a monotonically increasing counter incremented on every document mutation. Stale tokens (mismatched `state_version`) return `400` with a "stale continuation token" error.
- Select/projection.
- Facets where required by the matrix.
- Result counts (`count=true`).
- Deterministic ranking; documented that relevance is not Azure-equivalent.

### Storage

- `Storage` abstraction with an in-memory implementation (default) and a file-backed implementation for local development persistence.
- Isolation between indexes.
- Locking/transaction semantics so concurrent requests cannot corrupt state.
- Immediate consistency: newly indexed documents are immediately searchable (documented).
- Service reset capability for test isolation.

### Error handling

- Consistent internal-error → Azure-compatible-response mapping covering: invalid request, invalid index, index not found, document not found, invalid document, unsupported operation, invalid query, internal failure.
- Correct HTTP status codes and Azure error structure for each category.

### API versioning

- Version adapter isolating version-specific behaviour from the service model.
- One or more explicitly supported API versions; unsupported versions produce a clear error.

### Observability

- Structured logging sufficient to diagnose compatibility problems (method, endpoint, API version, index, operation, validation failures, query parse failures, internal exceptions).
- Optional debug logging of sanitised request information.
- Request bodies never logged by default.

### Test suite

- Unit tests (inline `#[cfg(test)]`): query parser, filter parser, storage, domain logic.
- HTTP contract tests (Rust integrated tests) organised by capability:
  ```text
  tests/contract/
      index_management.rs
      document_management.rs
      search.rs
      filtering.rs
      pagination.rs
      errors.rs
  ```
- SDK compatibility tests using the official Python SDK for every supported operation (`e2e-python/tests/sdk/`).
- E2E suite extended to cover the full supported-operations matrix (`e2e-python/tests/e2e/`).
- Concurrency tests: parallel document writes and searches do not corrupt state.
- HTTP fixtures from Phase 1 extended to cover all matrix operations; committed to `e2e-python/fixtures/` for Phase 3 C# replay.

## Out of scope

- Full admin API surface (Phase 3; `POST /admin/reset` already available from Phase 1).
- C# SDK compatibility (Phase 3; HTTP fixtures are captured in this phase for C# replay).
- Azure comparison test suite (Phase 3).
- Vector/semantic search, suggest, scoring profiles — unless the operations matrix requires them.

## Deliverables

1. Supported-operations matrix document (`docs/supported_operations.md`).
2. Complete implementation of the matrix (indexes, documents, query, storage, errors, versioning).
3. File-backed storage implementation.
4. Full unit, contract, and SDK compatibility test suites.
5. Extended e2e suite covering the matrix.
6. Extended HTTP fixtures in `e2e-python/fixtures/` covering all matrix operations.
7. `docs/known_differences.md` — created as gaps are discovered during implementation (relevance ordering, filter edge cases, etc.).
8. Updated README: supported operations, known limitations, configuration reference.

## Checklist

### Discovery

- [ ] Supported-operations matrix written and reviewed.
- [ ] Every matrix entry has at least one contract or SDK test.

### Index management

- [ ] Create/get/list/update/delete indexes work through the Python SDK.
- [ ] Schema validation rejects unsupported field types with explicit errors.
- [ ] Index-not-found and empty-index cases are distinguished correctly.

### Document management

- [ ] Upload, merge, merge-or-upload, delete, and batch operations work through the Python SDK.
- [ ] Invalid documents produce per-document errors in batch responses.
- [ ] Merge semantics match Azure (field-level merge, collection behaviour).

### Query engine

- [ ] Full-text search: simple, field-specific, and boolean queries return correct results.
- [ ] Filter parser handles the required operator set; unsupported syntax errors clearly.
- [ ] Ordering, pagination (top/offset and continuation tokens), select, facets, and count work.
- [ ] Continuation tokens are deterministic and do not expose internal identifiers.
- [ ] Ranking is deterministic; limitation documented.

### Storage

- [ ] In-memory and file-backed implementations pass the same test suite.
- [ ] Concurrent writes/searches do not corrupt state (concurrency test passes).
- [ ] Newly indexed documents are immediately searchable.
- [ ] Service reset works and is used by the test suite for isolation.

### Errors and versioning

- [ ] Every error category returns the correct status code and Azure error structure.
- [ ] Unsupported operations return explicit "unsupported" errors.
- [ ] Supported API versions work; unsupported versions error clearly.

### Observability

- [ ] Logs identify method, endpoint, API version, index, and operation.
- [ ] Validation and query parse failures are logged with diagnostic detail.
- [ ] Request bodies are not logged.

### Quality gates

- [ ] Unit, contract, SDK compatibility, and e2e suites all pass.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] CI is green.
- [ ] README documents supported operations and known limitations.
- [ ] `docs/known_differences.md` exists and lists all accepted differences.

## Exit criteria

Existing application code can point its Azure AI Search endpoint at the emulator and run its normal index, document, and search operations through the unmodified Python SDK, with every supported operation covered by contract and SDK tests, and unsupported operations failing explicitly.
