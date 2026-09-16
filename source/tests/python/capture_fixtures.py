"""Capture HTTP fixtures from the Azure AI Search Python SDK.

Run against a live emulator instance:
    python capture_fixtures.py --endpoint http://localhost:8080

Outputs sanitized request/response pairs to fixtures/.
"""

import argparse
import json
import re
from pathlib import Path
from typing import Any, Self

from azure.core.credentials import AzureKeyCredential
from azure.core.pipeline.transport import (
    HttpRequest,
    HttpResponse,
    HttpTransport,
    RequestsTransport,
)
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    SearchField,
    SearchFieldDataType,
    SearchIndex,
)

API_KEY = "fixture-key"
CREDENTIAL = AzureKeyCredential(API_KEY)
INDEX_NAME = "fixture-index"
# The emulator's default supported API version (see EMULATOR_API_VERSIONS).
API_VERSION = "2024-07-01"

# The pinned SDK (azure-search-documents==12.0.0) uses the endpoint URL verbatim,
# so the plain-HTTP local emulator needs no endpoint shim.
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


class RecordingTransport(HttpTransport[HttpRequest, HttpResponse]):
    """Delegates to the default transport and records every exchange."""

    def __init__(self) -> None:
        super().__init__()
        self._inner: RequestsTransport | None = None
        # TODO: replace Any with a TypedDict describing the recorded exchange shape.
        self.records: list[Any] = []

    def open(self) -> None:
        self._inner = RequestsTransport()
        self._inner.open()

    def close(self) -> None:
        if self._inner is not None:
            self._inner.close()
            self._inner = None

    def __enter__(self) -> Self:
        self.open()
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def send(self, request: HttpRequest, **kwargs: Any) -> HttpResponse:
        assert self._inner is not None
        # RequestsTransport subclasses the unparameterised HttpTransport, so its
        # inherited send() is typed to return Any; the runtime type is HttpResponse.
        response: HttpResponse = self._inner.send(request, **kwargs)
        request_body = request.data
        if isinstance(request_body, bytes):
            request_body = request_body.decode("utf-8", errors="replace")
        response_body = response.body().decode("utf-8", errors="replace")
        self.records.append(
            {
                "method": request.method,
                "url": request.url,
                "headers": dict(request.headers),
                "request_body": request_body,
                "status_code": response.status_code,
                "response_headers": dict(response.headers),
                "response_body": response_body,
            }
        )
        return response


def _strip_dynamic_headers(headers: dict[str, str]) -> dict[str, str]:
    """Drop per-request dynamic headers (request IDs, timestamps)."""
    return {
        k: v
        for k, v in headers.items()
        if k.lower() not in {"x-ms-client-request-id", "date"}
    }


def sanitize(record: dict[str, Any]) -> dict[str, Any]:
    """Strip dynamic values from a recorded exchange.

    Removes per-request values (client request IDs, timestamps, UUIDs) and
    normalises environment-dependent values (endpoint host, user agent) so the
    committed fixtures are deterministic across machines and runs.
    """
    rec = dict(record)
    rec["headers"] = _strip_dynamic_headers(rec["headers"])
    rec["response_headers"] = _strip_dynamic_headers(rec["response_headers"])
    if "User-Agent" in rec["headers"]:
        rec["headers"]["User-Agent"] = "<user-agent>"
    rec["url"] = UUID_RE.sub("<UUID>", rec["url"])
    rec["url"] = re.sub(r"^https?://[^/]+", "http://<endpoint>", rec["url"])
    return rec


def main() -> None:
    parser = argparse.ArgumentParser(description="Capture SDK HTTP fixtures")
    parser.add_argument("--endpoint", default="http://localhost:8080")
    parser.add_argument("--output", default="fixtures")
    args = parser.parse_args()

    transport = RecordingTransport()
    transport.open()

    index_client = SearchIndexClient(
        endpoint=args.endpoint,
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )
    search_client = SearchClient(
        endpoint=args.endpoint,
        index_name=INDEX_NAME,
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )

    # Create index
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
            ],
        )
    )

    # Upload documents
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "hello world"},
            {"id": "2", "title": "foo bar"},
        ]
    )

    # Get document
    search_client.get_document(key="1")

    # Search
    list(search_client.search(search_text="hello", top=10))

    # Delete index
    index_client.delete_index(INDEX_NAME)

    # Vector flow (Phase 2.1): a second index exercising the vector
    # wire format for Phase 3 C# replay.
    from azure.search.documents.indexes.models import (
        HnswAlgorithmConfiguration,
        HnswParameters,
        VectorSearch,
        VectorSearchProfile,
    )
    from azure.search.documents.models import VectorizedQuery

    vector_client = SearchClient(
        endpoint=args.endpoint,
        index_name="fixture-vector-index",
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )
    index_client.create_index(
        SearchIndex(
            name="fixture-vector-index",
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
                SearchField(
                    name="content_vector",
                    type="Collection(Edm.Single)",
                    searchable=True,
                    vector_search_dimensions=3,
                    vector_search_profile_name="cos",
                ),
            ],
            vector_search=VectorSearch(
                algorithms=[
                    HnswAlgorithmConfiguration(
                        name="hnsw-1",
                        parameters=HnswParameters(m=4, metric="cosine"),
                    ),
                ],
                profiles=[
                    VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1"),
                ],
            ),
        )
    )
    vector_client.upload_documents(
        documents=[
            {"id": "1", "title": "hello world", "content_vector": [1.0, 0.0, 0.0]},
            {"id": "2", "title": "foo bar", "content_vector": [0.0, 1.0, 0.0]},
        ]
    )
    list(
        vector_client.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[1.0, 0.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
                )
            ]
        )
    )
    list(
        vector_client.search(
            search_text="hello",
            vector_queries=[
                VectorizedQuery(
                    vector=[0.0, 1.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
                )
            ],
        )
    )
    index_client.delete_index("fixture-vector-index")

    # Phase 2.2 wire formats: synonym maps, minimumCoverage, debug, and filter
    # functions — captured for C# replay.
    from azure.search.documents.indexes.models import (
        SearchableField,
        SimpleField,
        SynonymMap,
    )

    phase22_client = SearchClient(
        endpoint=args.endpoint,
        index_name="fixture-phase22-index",
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )
    index_client.create_synonym_map(
        SynonymMap(name="fixture-sm", synonyms=["wa, washington"])
    )
    index_client.create_index(
        SearchIndex(
            name="fixture-phase22-index",
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchableField(
                    name="title",
                    type=SearchFieldDataType.String,
                    synonym_map_names=["fixture-sm"],
                ),
                SimpleField(
                    name="published",
                    # Wire-format string: mypy infers the
                    # `SearchFieldDataType.DateTimeOffset` member as a generic
                    # `Enum` (Azure SDK custom metaclass), incompatible with
                    # `SimpleField`'s `str | SearchFieldDataType` annotation.
                    type="Edm.DateTimeOffset",
                    filterable=True,
                ),
            ],
        )
    )
    phase22_client.upload_documents(
        documents=[
            {
                "id": "1",
                "title": "stays in Washington",
                "published": "2024-03-15T10:30:00Z",
            },
            {
                "id": "2",
                "title": "hotels in Portland",
                "published": "2024-05-01T12:00:00Z",
            },
        ]
    )
    # Synonym expansion + date filter + minimumCoverage + debug in one request.
    list(
        phase22_client.search(
            search_text="wa hotels",
            search_mode="any",
            minimum_coverage=0.5,
            filter="year(published) eq 2024",
            debug="vector",
        )
    )
    index_client.delete_index("fixture-phase22-index")
    index_client.delete_synonym_map("fixture-sm")

    # Phase 2.3 wire formats: semantic index, flat-SDK semantic search, and
    # the nested `semantic` object — captured for C# replay.
    from azure.search.documents.indexes.models import (
        SemanticConfiguration,
        SemanticField,
        SemanticPrioritizedFields,
        SemanticSearch,
    )

    semantic_client = SearchClient(
        endpoint=args.endpoint,
        index_name="fixture-semantic-index",
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )
    index_client.create_index(
        SearchIndex(
            name="fixture-semantic-index",
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchableField(name="title", type=SearchFieldDataType.String),
                SearchableField(name="content", type=SearchFieldDataType.String),
                SimpleField(
                    name="price",
                    type="Edm.Double",
                    filterable=True,
                    sortable=True,
                ),
            ],
            semantic_search=SemanticSearch(
                configurations=[
                    SemanticConfiguration(
                        name="default",
                        prioritized_fields=SemanticPrioritizedFields(
                            title_field=SemanticField(field_name="title"),
                            content_fields=[SemanticField(field_name="content")],
                        ),
                    )
                ]
            ),
        )
    )
    semantic_client.upload_documents(
        documents=[
            {
                "id": "1",
                "title": "Azure AI Search Overview",
                "content": (
                    "Azure AI Search is a cloud service. It provides rich text "
                    "search capabilities. It also supports vector search. The "
                    "service is highly available."
                ),
                "price": 10.0,
            },
            {
                "id": "2",
                "title": "Vector Search Guide",
                "content": (
                    "Vector search uses embeddings. Neural networks create the "
                    "embeddings. The search finds similar documents. This is "
                    "useful for semantic matching."
                ),
                "price": 20.0,
            },
        ]
    )
    # Flat SDK wire format: queryType=semantic + semanticConfiguration +
    # compound answers/captions strings.
    list(
        semantic_client.search(
            search_text="vector search embeddings",
            query_type="semantic",
            semantic_configuration_name="default",
            query_answer="extractive",
            query_answer_count=3,
            query_caption="extractive",
            semantic_error_mode="fail",
        )
    )
    # Nested `semantic` object (queryContext, captions.answers,
    # semanticErrorHandling): the pinned SDK does not send this shape, so it is
    # captured over raw HTTP and appended to the recording.
    import urllib.error
    import urllib.request

    url = (
        f"{args.endpoint}/indexes('fixture-semantic-index')/docs/search.post.search"
        f"?api-version={API_VERSION}"
    )
    payload = {
        "search": "vector search",
        "semantic": {
            "semanticConfiguration": "default",
            "queryContext": {"questions": ["What is vector search used for?"]},
            "answers": {"count": 3, "type": "extractive"},
            "captions": {
                "count": 1,
                "type": "extractive",
                "answers": {"count": 1, "type": "extractive"},
            },
            "semanticErrorHandling": "returnPartialResults",
        },
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"api-key": API_KEY, "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            status = resp.status
            body = resp.read().decode()
    except urllib.error.HTTPError as exc:
        status = exc.code
        body = exc.read().decode()
    transport.records.append(
        {
            "method": "POST",
            "url": url,
            "headers": {"api-key": API_KEY, "Content-Type": "application/json"},
            "request_body": json.dumps(payload),
            "status_code": status,
            "response_headers": {},
            "response_body": body,
        }
    )
    index_client.delete_index("fixture-semantic-index")

    # Phase 2.4 wire formats: a vectorizer index (vectorizers nested in
    # `vectorSearch`, the field associated through the profile's vectorizer),
    # document upload WITHOUT explicit vectors, and `kind: "text"` queries —
    # captured for C# replay.
    from azure.search.documents.indexes.models import (
        AzureOpenAIVectorizer,
        AzureOpenAIVectorizerParameters,
    )
    from azure.search.documents.models import VectorizableTextQuery

    vectorizer_client = SearchClient(
        endpoint=args.endpoint,
        index_name="fixture-vectorizer-index",
        credential=CREDENTIAL,
        api_version=API_VERSION,
        transport=transport,
    )
    index_client.create_index(
        SearchIndex(
            name="fixture-vectorizer-index",
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
                SearchField(name="content", type=SearchFieldDataType.String, searchable=True),
                SearchField(
                    name="content_vector",
                    type="Collection(Edm.Single)",
                    searchable=True,
                    vector_search_dimensions=8,
                    vector_search_profile_name="cos",
                ),
            ],
            vector_search=VectorSearch(
                algorithms=[
                    HnswAlgorithmConfiguration(
                        name="hnsw-1",
                        parameters=HnswParameters(m=4, metric="cosine"),
                    ),
                ],
                profiles=[
                    VectorSearchProfile(
                        name="cos",
                        algorithm_configuration_name="hnsw-1",
                        vectorizer_name="embedder",
                    ),
                ],
                vectorizers=[
                    AzureOpenAIVectorizer(
                        vectorizer_name="embedder",
                        parameters=AzureOpenAIVectorizerParameters(
                            resource_url="https://example-resource.openai.azure.com",
                            deployment_name="text-embedding-ada-002",
                        ),
                    ),
                ],
            ),
        )
    )
    # No explicit vectors: the vectorizer generates them from the text fields.
    vectorizer_client.upload_documents(
        documents=[
            {"id": "1", "title": "quantum computing", "content": "quantum applications"},
            {"id": "2", "title": "classical physics", "content": "classical mechanics"},
        ]
    )
    list(
        vectorizer_client.search(
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", k_nearest_neighbors=2, fields="content_vector"
                )
            ]
        )
    )
    list(
        vectorizer_client.search(
            search_text="quantum",
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", k_nearest_neighbors=2, fields="content_vector"
                )
            ],
        )
    )
    index_client.delete_index("fixture-vectorizer-index")

    transport.close()

    # Write sanitized fixtures
    out_dir = Path(args.output)
    out_dir.mkdir(parents=True, exist_ok=True)
    for i, record in enumerate(transport.records):
        sanitized = sanitize(record)
        path = out_dir / f"{i:02d}_{record['method']}_{INDEX_NAME}.json"
        path.write_text(json.dumps(sanitized, indent=2) + "\n")
        print(f"  {path}")  # noqa: T201 - CLI script, stdout is the interface

    print(f"\nCaptured {len(transport.records)} exchanges -> {out_dir}/")  # noqa: T201


if __name__ == "__main__":
    main()
