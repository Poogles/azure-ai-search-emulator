# AGENTS.md

Local HTTP-compatible emulator for Azure AI Search (Rust: axum + tantivy + hnsw_rs). It is a test double for apps and tests using the official SDKs — not a production service. Phase 3 (admin API, C# compatibility, release packaging) is in progress.

## Environment

- Toolchain comes from the Nix dev shell via direnv: run `direnv allow` once at the repo root. Provides cargo/clippy/rustfmt, Python 3.14, poetry, pre-commit, docker.
- `.local.env` (loaded by direnv) sets `POETRY_VIRTUALENVS_IN_PROJECT=true` — the Python venv lives at `source/tests/python/.venv`.
- The Python test suites require Docker (testcontainers).
- macOS: `shell.nix` exports `RUSTFLAGS="-L $(xcrun --show-sdk-path)/usr/lib"` to work around a missing `libiconv.tbd`; outside the dev shell you must set it manually or Rust linking fails.

## Commands (from repo root)

- `make` / `make test` — Python SDK E2E suite (`tests/e2e` only). Fails fast if the venv is missing — run `make setup` first. The `aisearch-emulator` Docker image is built automatically on first use.
- `make test-csharp` — C# SDK compatibility suite (xunit, .NET 10, Testcontainers). Builds the image if missing.
- `make rust` — `cargo fmt --check`, clippy with `-D warnings`, `cargo test --all-targets`.
- `make docker` — build the image. `make all` — rust + docker + e2e.
- `make ms-samples` — populate the sparse `Azure/azure-sdk-for-python` submodule (required before `make test-ms`).
- `make test-ms` — Microsoft reference-samples compatibility probe.
- `make clean` — removes the venv **and the committed `fixtures/`**; do not run casually.

There is no workspace at the repo root; every cargo command needs `--manifest-path source/rust/Cargo.toml`:

- Single Rust test: `cargo test --manifest-path source/rust/Cargo.toml --test contract <filter>` — all HTTP contract tests live in one `contract` integration target (`source/rust/tests/contract/`, one module per API area).
- Single Python test: `cd source/tests/python && ./venv/bin/python -m pytest tests/e2e/test_emulator.py::<test> -v`
- Single C# test: `cd source/tests/csharp && dotnet test --filter "FullyQualifiedName~<TestName>"`

`make test` and CI run **only** `tests/e2e`. `tests/sdk` (full SDK operation coverage) and `tests/ms_samples` are separate suites — run them explicitly (`pytest tests/sdk`, `make test-ms`).

## Layout

- `source/rust/` — the emulator crate: `api/` (axum router), `service/`, `query/`, `filter/`, `storage/` (in-memory only; `file` mode fails fast), `vector/`, `config.rs`, `error.rs`.
- `source/tests/python/` — e2e harness (official `azure-search-documents==12.0.0` SDK + testcontainers). `conftest.py` builds the image if missing, starts the container, and resets state before each test via `POST /admin/reset`.
- `source/tests/python/fixtures/` — captured, sanitized SDK HTTP exchanges; the source of truth for endpoint shapes (replayed by the Phase 3 C# harness). Regenerate with `capture_fixtures.py --endpoint http://localhost:8080` against a running emulator.
- `source/tests/csharp/` — C# xunit suite (official `Azure.Search.Documents==12.0.0` + Testcontainers 4.15, .NET 10). Run with `make test-csharp`. See the dedicated section below.
- `docs/supported_operations.md` — the contract matrix; every entry must be backed by a test. `docs/known_differences.md` — accepted divergences from Azure; anything not listed there that diverges is a defect.

## C# test suite (`source/tests/csharp/`)

A second, independent compatibility harness that drives the **official .NET SDK** (`Azure.Search.Documents==12.0.0`) against the containerised emulator. It mirrors the Python suites one-for-one so both SDKs are held to the same contract. Single project `Emulator.Tests.csproj` (xunit 2.9, .NET 10, Testcontainers 4.15, coverlet).

Test classes (all derive from `Infrastructure/EmulatorTestBase`):

- `E2ETests.cs` — mirrors `python/tests/e2e/test_emulator.py`: health, auth (401 on missing/empty key), api-version validation (400), Azure-structured error body, index CRUD, upload+search, match-all, ops-on-deleted-index, and a RAG-style vector+hybrid flow.
- `SdkTests.cs` — mirrors `python/tests/sdk/test_sdk.py`: full operation coverage (index CRUD, upload/merge/merge-or-upload/delete, filter, orderby, select, facets incl. `count:`/`top:`/`*`/`$count`, search fields, paging, count, get-document, geography point, complex + collection-of-complex types, service statistics, analyze-text, boolean query operators, suggest/autocomplete, synonym maps, aliases, knowledge sources/bases, knowledge-base retrieve, per-document error reporting, unsupported-option rejection).
- `VectorSearchTests.cs` — mirrors `python/tests/sdk/test_vectors.py`: vector index CRUD, nearest-first ordering, exhaustive KNN, hybrid union, pre/post filter modes, multiple-query union, dotProduct/euclidean metrics, non-retrievable vector omission, orderby precedence over vector rank.
- `FixtureReplayTests.cs` — replays the Python-captured fixtures in `source/tests/python/fixtures/*.json` (in filename order, against a fresh emulator) and asserts each exchange's status code. This is the wire-format probe: the exact request shapes the Python SDK emits must be accepted.

Infrastructure (`Infrastructure/`):

- `EmulatorEndpoint.cs` — per-collection fixture owning the container for the whole suite. Builds the `aisearch-emulator` image via the Docker CLI if missing, then runs it through Testcontainers on a random host port and waits on `/health`. The .NET equivalent of the Python `emulator_image` + `emulator_endpoint` fixtures.
- `EmulatorCollection.cs` — xunit collection so all emulator-backed classes share one container and run serially (xunit parallelises across collections, not within one).
- `EmulatorTestBase.cs` — pins api-version `2024-07-01`, resets state before each test via `POST /admin/reset` (the .NET `clean_emulator`), and provides SDK client factories plus raw-HTTP helpers for the cases the SDK can't express (health, auth/version errors, the bare-number `$count` facet).
- `HttpSchemeRewritingTransport.cs` — **test-harness only.** The pinned .NET SDK rejects any non-TLS endpoint in the client constructor (`AssertHttpsScheme`), stricter than the Python SDK. The harness hands the SDK an `https://` URL (satisfying the check) and this transport transparently downgrades it to the emulator's plain-HTTP URL on send. Do not replicate in the emulator.
- `Docker.cs` — minimal Docker CLI helpers (image-exists/build, repo-root discovery by walking up to `source/rust/Cargo.toml`).
- `TestData.cs` — shared index/doc shapes mirroring the Python `full_index`/`priced_docs` fixtures so both suites exercise identical data.

## Conventions and gotchas

- Clippy `pedantic` is on and `unwrap_used`/`expect_used` are **denied** — no `unwrap()`/`expect()`. rustfmt: `max_width = 100`.
- Unsupported operations must fail explicitly with an Azure-structured error (`{"error": {"code", "message"}}`); never silently approximate. Auth is a compatibility mechanism: any non-empty `api-key` is accepted, missing/empty → `401`.
- Implement the SDK wire format, not the REST docs: document operations go to `/docs/search.index` (upload) and `/docs/search.post.search` (search), not `/docs/index` and `/docs/search`.
- tantivy is built with `default-features = false` — the C `zstd` dependency fails to link in the Nix environment. Do not re-enable default features.
- The Docker image must stay under 20 MB (CI enforces); static musl binary on distroless.
- Env config uses double-underscore nesting: `EMULATOR_PORT`, `EMULATOR_STORAGE__MODE`, `EMULATOR_API_VERSIONS` (default `2024-07-01`), `EMULATOR_VECTOR__MAX_DIMENSION`.
- e2e pins api-version `2024-07-01`; the ms_samples suite runs samples unmodified with the SDK default (`2026-04-01`), so that emulator is started with both versions.
- The ms_samples suite is green *while gaps exist*: `KNOWN_ISSUES` in `tests/ms_samples/test_ms_samples.py` classifies each sample (pass/gap/falsepass/skip). It turns red when a gap is closed or a new upstream sample appears — triage the registry rather than just making it pass.
- `conftest.py` shims the SDK's HTTPS enforcement for the plain-HTTP local endpoint — test-harness only; do not replicate it in the emulator.
- When changing API behaviour: update `docs/supported_operations.md` (and `docs/known_differences.md` if the divergence is accepted) and add/adjust contract + e2e coverage in the same change.
