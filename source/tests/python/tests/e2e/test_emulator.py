"""E2E tests: official Azure AI Search SDK against the containerised emulator."""

import json
import urllib.error
import urllib.request
from typing import Any

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    SearchField,
    SearchFieldDataType,
    SearchIndex,
)

API_KEY = "test-key"
INDEX_NAME = "e2e-index"
# The emulator's default supported API version (see EMULATOR_API_VERSIONS). The
# SDK defaults to a newer version, so pin it explicitly.
API_VERSION = "2024-07-01"
# AzureKeyCredential sends the `api-key` header (the emulator's auth scheme) and
# avoids the SDK's bearer-token path, which enforces HTTPS.
CREDENTIAL = AzureKeyCredential(API_KEY)


@pytest.fixture()
def index_client(clean_emulator: str) -> SearchIndexClient:
    return SearchIndexClient(
        endpoint=clean_emulator, credential=CREDENTIAL, api_version=API_VERSION
    )


@pytest.fixture()
def search_client(clean_emulator: str) -> SearchClient:
    return SearchClient(
        endpoint=clean_emulator,
        index_name=INDEX_NAME,
        credential=CREDENTIAL,
        api_version=API_VERSION,
    )


@pytest.fixture()
def test_index() -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type=SearchFieldDataType.String, key=True),
            SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
        ],
    )


@pytest.fixture()
def created_index(index_client: SearchIndexClient, test_index: SearchIndex) -> SearchIndex:
    return index_client.create_index(test_index)


def test_health_endpoint(clean_emulator: str) -> None:
    with urllib.request.urlopen(f"{clean_emulator}/health", timeout=5) as resp:
        assert resp.status == 200
        body = resp.read().decode()
        assert "ok" in body


def _raw_get(url: str, api_key: str | None = None) -> tuple[int, Any]:
    """Issue a GET and return (status, parsed JSON body) without raising on 4xx/5xx."""
    headers = {"Accept": "application/json"}
    if api_key is not None:
        headers["api-key"] = api_key
    req = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as exc:
        return exc.code, json.loads(exc.read().decode())


def test_missing_api_key_returns_401(clean_emulator: str) -> None:
    url = f"{clean_emulator}/indexes?api-version={API_VERSION}"
    status, body = _raw_get(url, api_key=None)
    assert status == 401
    assert body["error"]["code"] == "AuthenticationFailed"


def test_empty_api_key_returns_401(clean_emulator: str) -> None:
    url = f"{clean_emulator}/indexes?api-version={API_VERSION}"
    status, body = _raw_get(url, api_key="")
    assert status == 401
    assert body["error"]["code"] == "AuthenticationFailed"


def test_missing_api_version_returns_400(clean_emulator: str) -> None:
    status, body = _raw_get(f"{clean_emulator}/indexes", api_key=API_KEY)
    assert status == 400
    assert body["error"]["code"] == "ApiVersionMissing"


def test_unsupported_api_version_returns_400(clean_emulator: str) -> None:
    client = SearchIndexClient(
        endpoint=clean_emulator, credential=CREDENTIAL, api_version="1900-01-01"
    )
    with pytest.raises(HttpResponseError) as exc_info:
        list(client.list_indexes())
    assert exc_info.value.status_code == 400
    response = exc_info.value.response
    assert response is not None
    assert json.loads(response.text())["error"]["code"] == "ApiVersionUnsupported"


def test_error_body_is_azure_structured(clean_emulator: str) -> None:
    url = f"{clean_emulator}/indexes?api-version={API_VERSION}"
    status, body = _raw_get(url, api_key=None)
    assert status == 401
    assert set(body) == {"error"}
    assert set(body["error"]) == {"code", "message"}
    assert isinstance(body["error"]["message"], str)


def _raw_post(url: str, payload: dict[str, Any], api_key: str = API_KEY) -> tuple[int, Any]:
    """Issue a JSON POST and return (status, parsed JSON body) without raising."""
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"api-key": api_key, "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as exc:
        return exc.code, json.loads(exc.read().decode())


def test_search_facet_count(
    clean_emulator: str,
    index_client: SearchIndexClient,
    search_client: SearchClient,
    test_index: SearchIndex,
) -> None:
    """The ``$count`` facet reports the result-set size as a bare number.

    Exercised over raw HTTP because the pinned SDK types
    ``SearchDocumentsResult.facets`` as ``dict[str, list[FacetResult]]`` and
    cannot deserialize the bare-number ``$count`` entry (the whole response
    falls back to a raw dict). The emulator's shape matches Azure.
    """
    index_client.create_index(test_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "alpha"},
            {"id": "2", "title": "beta"},
            {"id": "3", "title": "gamma"},
        ]
    )
    url = f"{clean_emulator}/indexes('{INDEX_NAME}')/docs/search.post.search?api-version={API_VERSION}"
    status, body = _raw_post(url, {"search": "*", "facets": ["$count"]})
    assert status == 200
    assert body["@search.facets"]["$count"] == 3


def test_create_index(created_index: SearchIndex) -> None:
    assert created_index.name == INDEX_NAME
    assert len(created_index.fields) == 2


def test_create_duplicate_index_fails(index_client: SearchIndexClient, test_index: SearchIndex) -> None:
    index_client.create_index(test_index)
    with pytest.raises(HttpResponseError) as exc_info:
        index_client.create_index(test_index)
    assert exc_info.value.status_code == 409


def test_list_indexes(index_client: SearchIndexClient, test_index: SearchIndex) -> None:
    index_client.create_index(test_index)
    names = [index.name for index in index_client.list_indexes()]
    assert names == [INDEX_NAME]


def test_update_index(index_client: SearchIndexClient, test_index: SearchIndex) -> None:
    index_client.create_index(test_index)
    updated = test_index
    updated.fields[1].searchable = False
    index_client.create_or_update_index(updated)
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.fields[1].searchable is False


def test_delete_index(index_client: SearchIndexClient, test_index: SearchIndex) -> None:
    index_client.create_index(test_index)
    index_client.delete_index(INDEX_NAME)
    with pytest.raises(ResourceNotFoundError):
        index_client.get_index(INDEX_NAME)


def test_upload_and_search(
    index_client: SearchIndexClient, search_client: SearchClient, test_index: SearchIndex
) -> None:
    index_client.create_index(test_index)

    docs = [
        {"id": "1", "title": "hello world"},
        {"id": "2", "title": "foo bar"},
        {"id": "3", "title": "hello there"},
    ]
    results = search_client.upload_documents(documents=docs)
    assert len(results) == 3
    assert all(r.succeeded for r in results)

    search_results = search_client.search(search_text="hello", top=10)
    found = list(search_results)
    assert len(found) == 2
    ids = {doc["id"] for doc in found}
    assert ids == {"1", "3"}


def test_search_match_all(
    index_client: SearchIndexClient, search_client: SearchClient, test_index: SearchIndex
) -> None:
    index_client.create_index(test_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "alpha"},
            {"id": "2", "title": "beta"},
        ]
    )

    results = list(search_client.search(search_text="*", top=10))
    assert len(results) == 2


def test_operations_on_deleted_index_fail(
    index_client: SearchIndexClient, search_client: SearchClient, test_index: SearchIndex
) -> None:
    index_client.create_index(test_index)
    index_client.delete_index(INDEX_NAME)

    # search() returns a lazy paged iterator; iterate to trigger the request.
    with pytest.raises(ResourceNotFoundError):
        list(search_client.search(search_text="test", top=1))


def test_rag_style_vector_and_hybrid_flow(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    """RAG-style flow: text + vector fields, embedding upload, vector and
    hybrid retrieval with ordering (never exact scores)."""
    from azure.search.documents.indexes.models import (
        HnswAlgorithmConfiguration,
        HnswParameters,
        VectorSearch,
        VectorSearchProfile,
    )
    from azure.search.documents.models import VectorizedQuery

    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
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
                        parameters=HnswParameters(metric="cosine"),
                    ),
                ],
                profiles=[
                    VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1"),
                ],
            ),
        )
    )
    docs = [
        {"id": "1", "title": "azure search", "content_vector": [1.0, 0.0, 0.0]},
        {"id": "2", "title": "azure emulators", "content_vector": [0.0, 1.0, 0.0]},
        {"id": "3", "title": "unrelated", "content_vector": [0.0, 0.0, 1.0]},
    ]
    results = search_client.upload_documents(documents=docs)
    assert all(r.succeeded for r in results)

    # Vector-only retrieval: nearest first.
    found = list(
        search_client.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[1.0, 0.0, 0.0], k_nearest_neighbors=2, fields="content_vector"
                )
            ]
        )
    )
    assert [doc["id"] for doc in found] == ["1", "2"]

    # Hybrid retrieval: union of the full-text ("azure" → 1, 2) and vector
    # ([0,0,1] → 3) sides.
    found = list(
        search_client.search(
            search_text="azure",
            vector_queries=[
                VectorizedQuery(
                    vector=[0.0, 0.0, 1.0], k_nearest_neighbors=1, fields="content_vector"
                )
            ],
        )
    )
    assert {doc["id"] for doc in found} == {"1", "2", "3"}
