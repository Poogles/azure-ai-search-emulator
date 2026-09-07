---
status: draft
status_last_reviewed: 2026-09-07
---

# Phase 0 — Repository Setup

## Purpose

Establish the repository, tooling, and project scaffolding so that subsequent phases can be developed, tested, and reviewed consistently. No emulator functionality is implemented in this phase.

The implementation language is **Rust**, chosen to leverage mature existing libraries (HTTP frameworks, JSON handling, testing) and to provide fast startup times and a small, static deployment artifact for the emulator.

## Scope

- Git repository initialisation and baseline configuration.
- Rust workspace setup using Cargo.
- Linting, type safety, and formatting tooling (clippy, rustfmt).
- Test tooling (cargo test) configured but with no meaningful tests yet.
- Directory layout matching the layered architecture from the initial design (HTTP/API, service, query engine, storage).
- Python test harness scaffolding (`e2e-python/`) with Poetry, pytest, and testcontainers-python configured (no tests yet).
- Nix dev shell updated to provide both the Rust toolchain and Python 3.12+ (for the test harness).
- CI skeleton (fmt, clippy, test) that runs on the empty project.
- `docs/decisions.md` recording pinned versions and configuration conventions.
- README with development instructions.

## Out of scope

- Any emulator code, Dockerfile, or e2e tests (Phase 1).
- Full API implementation (Phase 2).
- Admin API (Phase 3).

## Deliverables

1. `Cargo.toml` (workspace) with project metadata and tool configuration.
2. Crate `aisearch-emulator` with a `src/` layout containing empty layer modules:
   ```text
   src/
       main.rs
       lib.rs
       api/          # HTTP / Azure compatibility layer
       service/      # domain / service layer
       query/        # query engine
       storage/      # storage abstraction
   ```
3. Rust test layout:
   ```text
   tests/
       contract/     # HTTP contract tests (integrated tests, Phase 2)
   ```
   Unit tests live inline via `#[cfg(test)]` in each module.
4. Python e2e test harness:
   ```text
   e2e-python/
       pyproject.toml    # Poetry project
       tests/
           e2e/          # end-to-end tests (Phase 1)
       conftest.py       # testcontainers fixture
   ```
5. `rustfmt.toml` and clippy configuration (deny warnings in CI).
6. `pre-commit` configuration (rustfmt, clippy).
7. CI workflow (GitHub Actions or equivalent) running fmt, clippy, and tests.
8. `README.md` with setup and run instructions.
9. `.gitignore` (Cargo, IDE, OS, Python).
10. Updated `shell.nix` providing the Rust toolchain (rustc, cargo, clippy, rustfmt) and Python 3.12+ via nixpkgs.
11. `docs/decisions.md` with pinned versions and configuration conventions (see below).

## Decisions to record

Record the following in `docs/decisions.md` as they are made:

- Rust edition and MSRV (e.g. edition 2021, MSRV matching the nixpkgs toolchain).
- HTTP framework choice (e.g. Axum or Actix) — chosen in Phase 1, but the dependency should be added here if settled.
- JSON handling library (e.g. serde/serde_json).
- **Pinned Azure AI Search API version:** `2024-07-01` (single version, explicitly supported).
- **Pinned Python SDK version:** `azure-search-documents==11.6.0` (exact pin for the e2e harness).
- **Authentication rule:** accept any non-empty `api-key` header; missing or empty key returns `401` with Azure error structure (`{"error":{"code":"AuthenticationFailed","message":"..."}}`).
- **Configuration environment variables:**
  - `EMULATOR_PORT` (default `8080`)
  - `EMULATOR_STORAGE__MODE` (`memory` | `file`, default `memory`)
  - `EMULATOR_API_VERSIONS` (comma-separated, default `2024-07-01`)
  - `EMULATOR_LOG_LEVEL` (`info` default)
  - `EMULATOR_ENABLE_ADMIN` (`true` default; gates `/admin/*` endpoints)
- **Default storage mode:** in-memory (file-backed added in Phase 2).

## Checklist

### Repository

- [ ] Git repository initialised with an initial commit.
- [ ] `.gitignore` present and covering Cargo (`target/`), IDE, OS, and Python (`__pycache__/`, `.venv/`) artifacts.
- [ ] `README.md` explains how to enter the dev shell and run the toolchain.

### Project

- [ ] `Cargo.toml` created; `cargo build` succeeds in the nix shell.
- [ ] Crate `aisearch-emulator` compiles with `lib.rs` and `main.rs`.
- [ ] Layer modules (`api`, `service`, `query`, `storage`) exist and compile.
- [ ] Placeholder test in `tests/` passes via `cargo test`.
- [ ] `e2e-python/` Poetry project created; `poetry install` succeeds; `poetry run pytest --collect-only` runs (no tests yet).

### Tooling

- [ ] `cargo fmt --check` passes.
- [ ] Clippy configured (warnings denied) and `cargo clippy --all-targets` passes.
- [ ] `cargo test` runs and passes.
- [ ] Pre-commit hooks installed and pass on a clean tree.

### Nix shell

- [ ] `shell.nix` provides rustc, cargo, clippy, rustfmt, and Python 3.12+.
- [ ] `direnv allow` applied; toolchain works inside the shell.

### Decisions

- [ ] `docs/decisions.md` created with pinned API version, SDK version, auth rule, and config env var names.

### CI

- [ ] CI workflow runs fmt, clippy, and tests.
- [ ] CI is green on the initial commit.

## Exit criteria

A developer can clone the repository, enter the nix shell, and run `cargo build`, `cargo fmt --check`, `cargo clippy`, and `cargo test` successfully, with CI green. The Python e2e harness (`e2e-python/`) installs cleanly and is ready for Phase 1 tests. `docs/decisions.md` records all pinned versions and configuration conventions.
