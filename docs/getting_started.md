---
status: complete
status_last_reviewed: 2026-09-10
---

# Getting Started

A practical guide to running the emulator for local development and to wiring it into
test suites with testcontainers (Python and .NET). For the full behavioural contract see
[supported_operations.md](supported_operations.md); for accepted divergences from Azure see
[known_differences.md](known_differences.md).

## What you get

A local, HTTP-compatible stand-in for Azure AI Search. Your application keeps using the
official Azure AI Search SDK; only the endpoint and key change:

```text
endpoint:  https://my-service.search.windows.net   ->   http://localhost:8080
api-key:   <real admin/query key>                  ->   any non-empty string
```

Authentication is a compatibility mechanism, not a security boundary: any non-empty
`api-key` header is accepted, and a missing/empty key returns `401` with the Azure error
structure. No Azure credentials or network access are required.

## Prerequisites

- **Docker** — required for the containerised runs and the testcontainers suites below.
- **Nix + direnv** — only if you are building the Rust binary from source (see
  [README.md](../README.md#development-environment)). Not needed if you only run the
  pre-built image or the test suites (they build the image for you).

## Running the emulator

### Docker (recommended)

```sh
docker build -t aisearch-emulator source/rust
docker run --rm -p 8080:8080 aisearch-emulator
```

The image is a static binary on `distroless/static` (< 20 MB) with a built-in
`HEALTHCHECK`. It serves on port `8080`.

### Local (cargo)

```sh
cargo run --manifest-path source/rust/Cargo.toml
```

Serves on `http://localhost:8080` by default.

### Configuration

| Variable                         | Default      | Description                                   |
|:---------------------------------|:-------------|:----------------------------------------------|
| `EMULATOR_PORT`                  | `8080`       | Listen port                                   |
| `EMULATOR_STORAGE__MODE`         | `memory`     | `memory` or `file` (file not yet implemented) |
| `EMULATOR_API_VERSIONS`          | `2024-07-01` | Comma-separated supported API versions        |
| `EMULATOR_LOG_LEVEL`             | `info`       | Log level                                     |
| `EMULATOR_ENABLE_ADMIN`          | `true`       | Enable `POST /admin/reset`                    |
| `EMULATOR_VECTOR__MAX_DIMENSION` | `3072`       | Max accepted vector field dimension           |

## Smoke test

```sh
# Liveness (no auth required)
curl -s http://localhost:8080/health
# -> {"status":"ok"}

# Create an index
curl -s -X POST "http://localhost:8080/indexes?api-version=2024-07-01" \
  -H "api-key: dev" -H "Content-Type: application/json" \
  -d '{"name":"docs","fields":[
        {"name":"id","type":"Edm.String","key":true},
        {"name":"title","type":"Edm.String","searchable":true}]}'

# Upload a document
curl -s -X POST "http://localhost:8080/indexes('docs')/docs/search.index?api-version=2024-07-01" \
  -H "api-key: dev" -H "Content-Type: application/json" \
  -d '{"value":[{"@search.action":"upload","id":"1","title":"hello world"}]}'

# Search
curl -s -X POST "http://localhost:8080/indexes('docs')/docs/search.post.search?api-version=2024-07-01" \
  -H "api-key: dev" -H "Content-Type: application/json" \
  -d '{"search":"hello"}'
```

Note the SDK wire-format routes: document operations go to `/docs/search.index` (upload)
and `/docs/search.post.search` (search), not the `/docs/index` and `/docs/search` forms
shown in some Azure REST documentation.

## Resetting state

The emulator is in-memory; state is lost when the process exits. For test isolation while
a process is running, clear all indexes and documents with:

```sh
curl -s -X POST http://localhost:8080/admin/reset
# -> {"status":"reset"}
```

`GET /health` and `POST /admin/reset` are outside the Azure surface and require no
credentials. `POST /admin/reset` is gated by `EMULATOR_ENABLE_ADMIN` (default `true`).

## Using testcontainers in Python

The e2e harness (`source/tests/python/`) drives the official `azure-search-documents`
SDK against a containerised emulator. The reusable pieces live in
`source/tests/python/conftest.py`:

- `emulator_image` (session) — builds the `aisearch-emulator` image via the Docker CLI if
  it is not already present, then yields the image name.
- `emulator_endpoint` (session) — starts a `DockerContainer` on a random host port, waits
  on `/health`, and yields the base URL.
- `clean_emulator` (function) — calls `POST /admin/reset` before each test so every test
  starts from a clean state.

Minimal standalone usage:

```python
import os
import time
import urllib.request
from testcontainers.core.container import DockerContainer

os.environ.setdefault("TESTCONTAINERS_RYUK_DISABLED", "true")  # see note below

IMAGE = "aisearch-emulator"
PORT = 8080

def wait_for_health(url: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=2) as resp:
                if resp.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.5)
    raise RuntimeError("emulator did not become healthy")

container = DockerContainer(IMAGE).with_exposed_ports(PORT)
with container:
    endpoint = f"http://{container.get_container_host_ip()}:{container.get_exposed_port(PORT)}"
    wait_for_health(endpoint)

    from azure.search.documents import SearchClient
    from azure.search.documents.indexes import SearchIndexClient

    index_client = SearchIndexClient(
        endpoint=endpoint,
        credential="dev",
        api_version="2024-07-01",
    )
    index_client.create_index(
        {"name": "docs", "fields": [
            {"name": "id", "type": "Edm.String", "key": True},
            {"name": "title", "type": "Edm.String", "searchable": True},
        ]}
    )
    client = SearchClient(endpoint=endpoint, index_name="docs", credential="dev",
                          api_version="2024-07-01")
    client.upload_documents([{"id": "1", "title": "hello world"}])
    results = list(client.search("hello"))
    assert results[0]["id"] == "1"
```

Notes:

- **Ryuk is disabled** (`TESTCONTAINERS_RYUK_DISABLED=true`). The Ryuk cleanup container
  fails to mount the Docker socket on Docker Desktop for macOS and is unnecessary here —
  the `with container:` context manager stops and removes the container on exit.
- **HTTPS shim.** The pinned SDK's bearer-token auth policy rejects non-TLS URLs. The
  harness monkeypatches `_authentication._enforce_https` to a no-op so a plain-HTTP
  endpoint is reachable. This is test-harness-only; with `api-key` auth the endpoint URL
  is used verbatim and no shim is strictly required. Do not replicate this in the emulator.
- **API version.** Pin `api_version="2024-07-01"` (the emulator's default). The SDK
  otherwise defaults to a newer version the emulator does not accept.

Run the full Python e2e suite from the repository root:

```sh
make setup   # first time only: creates the venv and installs dependencies
make test    # builds the image on first use, then runs tests/e2e
```

## Using testcontainers in .NET

The C# harness (`source/tests/csharp/`) drives the official `Azure.Search.Documents`
SDK against the same containerised emulator. The reusable pieces live in
`source/tests/csharp/Infrastructure/`:

- `EmulatorEndpoint` — per-collection fixture owning the container for the whole suite.
  Builds the image via the Docker CLI if missing, then runs it through Testcontainers on
  a random host port and waits on `/health`.
- `EmulatorCollection` — an xunit collection so all emulator-backed classes share one
  container and run serially (xunit parallelises across collections, not within one).
- `EmulatorTestBase` — pins api-version `2024-07-01`, resets state before each test via
  `POST /admin/reset`, and provides SDK client factories plus raw-HTTP helpers.

Minimal standalone usage:

```csharp
using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Indexes.Models;
using DotNet.Testcontainers.Buildainers;
using DotNet.Testcontainers.Containers;
using DotNet.Testcontainers.Images;

const int Port = 8080;
const string Image = "aisearch-emulator";

await using var container = new ContainerBuilder(new DockerImage(Image))
    .WithPortBinding(Port, assignRandomHostPort: true)
    .Build();
container.StartAsync().GetAwaiter().GetResult();
var port = container.GetMappedPublicPort(Port);
var baseUrl = $"http://127.0.0.1:{port}";

// Wait for /health to return 200 (see EmulatorEndpoint.WaitForHealth).

var options = new SearchClientOptions(SearchClientOptions.ServiceVersion.V2024_07_01)
{
    Transport = new HttpSchemeRewritingTransport(),  // see note below
};
var indexClient = new SearchIndexClient(
    new Uri(baseUrl.Replace("http://", "https://")),
    new AzureKeyCredential("dev"),
    options);
indexClient.CreateIndex(new SearchIndex("docs")
{
    Fields =
    {
        new SearchField("id", SearchFieldDataType.String) { IsKey = true },
        new SearchableField("title"),
    },
});
var client = new SearchClient(
    new Uri(baseUrl.Replace("http://", "https://")),
    "docs",
    new AzureKeyCredential("dev"),
    options);
client.IndexDocuments(new[] { new { id = "1", title = "hello world" } });
var results = client.Search<string>("hello").GetResults().ToList();
```

Notes:

- **HTTPS scheme rewriting.** The pinned .NET SDK rejects any non-TLS endpoint in the
  client constructor (`AssertHttpsScheme`), stricter than the Python SDK. The harness
  hands the SDK an `https://` URL (satisfying the check) and
  `HttpSchemeRewritingTransport` transparently downgrades it to the emulator's plain-HTTP
  URL on send. This is test-harness-only; do not replicate it in the emulator.
- **API version.** Pin `SearchClientOptions.ServiceVersion.V2024_07_01` (the emulator's
  default); the SDK otherwise defaults to a newer version the emulator does not accept.
- **State isolation.** Reset before each test with `POST {baseUrl}/admin/reset` (the
  .NET equivalent of the Python `clean_emulator` fixture).

Run the full C# suite from the repository root:

```sh
make test-csharp   # builds the image if missing, then runs dotnet test
```

## Where to go next

- [supported_operations.md](supported_operations.md) — the full contract: every operation,
  its SDK method, HTTP request, status codes, and supported/unsupported state.
- [known_differences.md](known_differences.md) — accepted behavioural differences from
  Azure, with rationale. Anything not listed there that diverges is a defect.
- [ms_samples_compatibility.md](ms_samples_compatibility.md) — compatibility probe against
  Microsoft's reference samples.
- [initial_design.md](initial_design.md) — problem statement, requirements, architecture.
