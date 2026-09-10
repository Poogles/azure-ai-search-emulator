# AGENTS.md

Local HTTP-compatible emulator for Azure AI Search (Rust: axum + tantivy + hnsw_rs). It is a test double for apps and tests using the official SDKs — not a production service. Phase 3 (admin API, C# compatibility, release packaging) is in progress.

## Environment

- Toolchain comes from the Nix dev shell via direnv: run `direnv allow` once at the repo root. Provides cargo/clippy/rustfmt, Python 3.14, poetry, pre-commit, docker.
- `.local.env` (loaded by direnv) sets `POETRY_VIRTUALENVS_IN_PROJECT=true` — the Python venv lives at `source/tests/python/.venv`.
- The Python test suites require Docker (testcontainers).
- macOS: `shell.nix` exports `RUSTFLAGS="-L $(xcrun --show-sdk-path)/usr/lib"` to work around a missing `libiconv.tbd`; outside the dev shell you must set it manually or Rust linking fails.

## Commands (from repo root)

- `make` / `make test` — Python SDK E2E suite (`tests/e2e` only). Fails fast if the venv is missing — run `make setup` first. The `aisearch-emulator` Docker image is built automatically on first use.
- `make rust` — `cargo fmt --check`, clippy with `-D warnings`, `cargo test --all-targets`.
- `make docker` — build the image. `make all` — rust + docker + e2e.
- `make ms-samples` — populate the sparse `Azure/azure-sdk-for-python` submodule (required before `make test-ms`).
- `make test-ms` — Microsoft reference-samples compatibility probe.
- `make clean` — removes the venv **and the committed `fixtures/`**; do not run casually.

There is no workspace at the repo root; every cargo command needs `--manifest-path source/rust/Cargo.toml`:

- Single Rust test: `cargo test --manifest-path source/rust/Cargo.toml --test contract <filter>` — all HTTP contract tests live in one `contract` integration target (`source/rust/tests/contract/`, one module per API area).
- Single Python test: `cd source/tests/python && ./venv/bin/python -m pytest tests/e2e/test_emulator.py::<test> -v`

`make test` and CI run **only** `tests/e2e`. `tests/sdk` (full SDK operation coverage) and `tests/ms_samples` are separate suites — run them explicitly (`pytest tests/sdk`, `make test-ms`).

## Layout

- `source/rust/` — the emulator crate: `api/` (axum router), `service/`, `query/`, `filter/`, `storage/` (in-memory only; `file` mode fails fast), `vector/`, `config.rs`, `error.rs`.
- `source/tests/python/` — e2e harness (official `azure-search-documents==12.0.0` SDK + testcontainers). `conftest.py` builds the image if missing, starts the container, and resets state before each test via `POST /admin/reset`.
- `source/tests/python/fixtures/` — captured, sanitized SDK HTTP exchanges; the source of truth for endpoint shapes (replayed by the Phase 3 C# harness). Regenerate with `capture_fixtures.py --endpoint http://localhost:8080` against a running emulator.
- `source/tests/csharp/` — Phase 3, empty.
- `docs/supported_operations.md` — the contract matrix; every entry must be backed by a test. `docs/known_differences.md` — accepted divergences from Azure; anything not listed there that diverges is a defect.

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
