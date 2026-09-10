# Azure AI Search Local Emulator

A local, HTTP-compatible emulator for [Azure AI Search](https://learn.microsoft.com/azure/search/search-service-intro). It lets existing application code and integration tests run against a local service using the **official Azure AI Search SDKs**, with no application-level changes beyond endpoint configuration.

The emulator is a **compatible test double with a persistent search implementation** — it reproduces the observable API behaviour required by our applications and tests, not the full Azure service. It is written in **Rust** for fast startup and a small, static deployment artifact.

> This is a development and testing tool, not a production replacement for Azure AI Search. Tests that validate real Azure behaviour should continue to run against a live instance.

## Status

The project has completed **Phases 0–2.1**: a runnable, containerised HTTP service implementing the full Phase 2 API surface plus vector indexing and vector/hybrid search (Phase 2.1), exercised by the official Python SDK through testcontainers and probed against Microsoft's reference samples. **Phase 3** (admin API, C# compatibility, Azure comparison, release packaging) is in progress. See [docs/](docs/) for details.

| Phase | Description                                               | Doc                                                                           |
|:------|:----------------------------------------------------------|:------------------------------------------------------------------------------|
| 0     | Repository setup, tooling, scaffolding                    | [phase_0_repository_setup.md](docs/phase_0_repository_setup.md)               |
| 1     | Runnable HTTP service, Docker image, Python SDK e2e tests | [phase_1_scaffold_and_e2e.md](docs/phase_1_scaffold_and_e2e.md)               |
| 2     | Full API surface, query engine, storage, contract tests   | [phase_2_production_api.md](docs/phase_2_production_api.md)                   |
| 2.1   | Vector indexing and vector/hybrid search                  | [phase_2_1_vector_indexing.md](docs/phase_2_1_vector_indexing.md)             |
| 3     | Admin API, C# compatibility, Azure comparison, release    | [phase_3_admin_api_and_remaining.md](docs/phase_3_admin_api_and_remaining.md) |

The overall design, goals, non-goals, and architecture are described in [docs/initial_design.md](docs/initial_design.md).

## How it works

Applications keep using the Azure AI Search SDK. Only the endpoint changes:

```text
SEARCH_ENDPOINT=https://real-service.search.windows.net   # production
SEARCH_ENDPOINT=http://localhost:8080                     # local emulator
```

The SDK constructs requests and interprets responses; the emulator implements the service-side behaviour behind the same HTTP contract. Authentication is a compatibility mechanism, not a security boundary: any non-empty `api-key` header is accepted, and a missing/empty key returns `401` with the Azure error structure.

## Development environment

The toolchain (Rust, Python, Poetry, Docker, etc.) is provided by a Nix dev shell via `direnv`.

Prerequisites: [Nix](https://nixos.org/download/) and [direnv](https://direnv.net/).

```sh
# From the repository root, allow direnv to load the shell
direnv allow
```

Entering the directory activates the shell automatically. It provides `rustc`, `cargo`, `clippy`, `rustfmt`, Python 3.14, `poetry`, `pre-commit`, and `docker`.

> **Nix on macOS — linker note.** The Nix darwin SDK root does not ship `usr/lib/libiconv.tbd`, which breaks the final link step of Rust builds (`ld: library not found for -liconv`). The dev shell in [shell.nix](shell.nix) exports `RUSTFLAGS` with the system SDK lib directory on the linker search path, so `cargo build` / `cargo test` / `cargo clippy` work out of the box. Outside the dev shell, set it manually:
>
> ```sh
> RUSTFLAGS="-L $(xcrun --show-sdk-path)/usr/lib" cargo test --manifest-path source/rust/Cargo.toml
> ```
>
> The Docker musl build is unaffected.

## Running the emulator

### Local (cargo)

```sh
cargo run --manifest-path source/rust/Cargo.toml
```

Serves on `http://localhost:8080` by default. Configure via environment variables:

| Variable                         | Default      | Description                                   |
|:---------------------------------|:-------------|:----------------------------------------------|
| `EMULATOR_PORT`                  | `8080`       | Listen port                                   |
| `EMULATOR_STORAGE__MODE`         | `memory`     | `memory` or `file` (file not yet implemented) |
| `EMULATOR_API_VERSIONS`          | `2024-07-01` | Comma-separated supported API versions        |
| `EMULATOR_LOG_LEVEL`             | `info`       | Log level                                     |
| `EMULATOR_ENABLE_ADMIN`          | `true`       | Enable `/admin/reset`                         |
| `EMULATOR_VECTOR__MAX_DIMENSION` | `3072`       | Max accepted vector field dimension           |

### Docker

```sh
docker build -t aisearch-emulator source/rust
docker run --rm -p 8080:8080 aisearch-emulator
```

The image is a static binary on `distroless/static` (< 20 MB) with a built-in `HEALTHCHECK`.

### E2E tests (Python SDK)

```sh
make test
```

`make test` runs the Python SDK suite from the repository root (the Docker image is built automatically on first use). The manual equivalent:

```sh
docker build -t aisearch-emulator source/rust
cd source/tests/python
poetry install
poetry run pytest tests/e2e -v
```

The E2E suite starts the container via testcontainers and exercises the official `azure-search-documents` SDK: index CRUD, upload, full-text and match-all search, deleted-index error handling, and a RAG-style vector + hybrid search flow.

## Repository layout

```text
docs/                  Design and phased implementation documents
source/rust/           Rust crate and Rust integration tests
source/tests/python/   Python SDK compatibility / e2e harness
source/tests/csharp/   C# SDK compatibility harness — Phase 3
shell.nix              Nix dev shell definition
```

Shortcuts: `make rust` (fmt, clippy, tests), `make docker` (build image), `make help` (all targets). Rust commands run from the repository root with the manifest path:

```sh
cargo build --manifest-path source/rust/Cargo.toml
cargo test --manifest-path source/rust/Cargo.toml
cargo fmt --manifest-path source/rust/Cargo.toml -- --check
cargo clippy --manifest-path source/rust/Cargo.toml --all-targets -- -D warnings
```

The Python harness is installed and checked from its directory:

```sh
cd source/tests/python
poetry install
poetry run pytest --collect-only
```

## Documentation

- [docs/initial_design.md](docs/initial_design.md) — problem statement, requirements, architecture, alternatives, design principles.
- [docs/phase_0_repository_setup.md](docs/phase_0_repository_setup.md) — repository and tooling setup.
- [docs/phase_1_scaffold_and_e2e.md](docs/phase_1_scaffold_and_e2e.md) — minimal service, containerisation, e2e tests.
- [docs/phase_2_production_api.md](docs/phase_2_production_api.md) — full API surface and test suite.
- [docs/phase_3_admin_api_and_remaining.md](docs/phase_3_admin_api_and_remaining.md) — admin API, C# compatibility, release.
- [docs/supported_operations.md](docs/supported_operations.md) — supported-operations matrix: every operation, its SDK method, HTTP request, status codes, and supported/unsupported state.
- [docs/ms_samples_compatibility.md](docs/ms_samples_compatibility.md) — compatibility probe against Microsoft's reference samples: how it runs, per-sample results, and what is skipped and why.
- [docs/known_differences.md](docs/known_differences.md) — accepted behavioural differences from real Azure AI Search, with rationale.
- [docs/decisions/](docs/decisions/) — architecture decision records (HTTP framework, search engine, baseline).
