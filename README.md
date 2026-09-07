# Azure AI Search Local Emulator

A local, HTTP-compatible emulator for [Azure AI Search](https://learn.microsoft.com/azure/search/search-service-intro). It lets existing application code and integration tests run against a local service using the **official Azure AI Search SDKs**, with no application-level changes beyond endpoint configuration.

The emulator is a **compatible test double with a persistent search implementation** — it reproduces the observable API behaviour required by our applications and tests, not the full Azure service. It is written in **Rust** for fast startup and a small, static deployment artifact.

> This is a development and testing tool, not a production replacement for Azure AI Search. Tests that validate real Azure behaviour should continue to run against a live instance.

## Status

The project is in **Phase 0 — Repository Setup**. The design and phased implementation plan are in place; the emulator itself is not yet implemented. See [docs/](docs/) for details.

| Phase | Description | Doc |
|-------|-------------|-----|
| 0 | Repository setup, tooling, scaffolding | [phase_0_repository_setup.md](docs/phase_0_repository_setup.md) |
| 1 | Runnable HTTP service, Docker image, Python SDK e2e tests | [phase_1_scaffold_and_e2e.md](docs/phase_1_scaffold_and_e2e.md) |
| 2 | Full API surface, query engine, storage, contract tests | [phase_2_production_api.md](docs/phase_2_production_api.md) |
| 3 | Admin API, C# compatibility, Azure comparison, release | [phase_3_admin_api_and_remaining.md](docs/phase_3_admin_api_and_remaining.md) |

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

## Repository layout

```text
docs/          Design and phased implementation documents
source/        Emulator source (Rust) — to be populated in Phase 1
e2e-python/    Python SDK compatibility / e2e harness (Poetry + pytest + testcontainers) — Phase 1
e2e-csharp/    C# SDK compatibility harness — Phase 3
shell.nix      Nix dev shell definition
```

## Documentation

- [docs/initial_design.md](docs/initial_design.md) — problem statement, requirements, architecture, alternatives, design principles.
- [docs/phase_0_repository_setup.md](docs/phase_0_repository_setup.md) — repository and tooling setup.
- [docs/phase_1_scaffold_and_e2e.md](docs/phase_1_scaffold_and_e2e.md) — minimal service, containerisation, e2e tests.
- [docs/phase_2_production_api.md](docs/phase_2_production_api.md) — full API surface and test suite.
- [docs/phase_3_admin_api_and_remaining.md](docs/phase_3_admin_api_and_remaining.md) — admin API, C# compatibility, release.

Additional documents (`docs/decisions.md`, `docs/supported_operations.md`, `docs/known_differences.md`) are created as the corresponding phases are implemented.
