"""E2E tests: official Azure AI Search SDK against the containerised emulator."""

import urllib.request

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
def index_client(clean_emulator) -> SearchIndexClient:
    return SearchIndexClient(
        endpoint=clean_emulator, credential=CREDENTIAL, api_version=API_VERSION
    )


@pytest.fixture()
def search_client(clean_emulator) -> SearchClient:
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
def created_index(index_client, test_index) -> SearchIndex:
    return index_client.create_index(test_index)


def test_health_endpoint(clean_emulator):
    with urllib.request.urlopen(f"{clean_emulator}/health", timeout=5) as resp:
        assert resp.status == 200
        body = resp.read().decode()
        assert "ok" in body


def test_create_index(created_index):
    assert created_index.name == INDEX_NAME
    assert len(created_index.fields) == 2


def test_create_duplicate_index_fails(index_client, test_index):
    index_client.create_index(test_index)
    with pytest.raises(HttpResponseError) as exc_info:
        index_client.create_index(test_index)
    assert exc_info.value.status_code == 409


def test_list_indexes(index_client, test_index):
    index_client.create_index(test_index)
    names = [index.name for index in index_client.list_indexes()]
    assert names == [INDEX_NAME]


def test_update_index(index_client, test_index):
    index_client.create_index(test_index)
    updated = test_index
    updated.fields[1].searchable = False
    index_client.create_or_update_index(updated)
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.fields[1].searchable is False


def test_delete_index(index_client, test_index):
    index_client.create_index(test_index)
    index_client.delete_index(INDEX_NAME)
    with pytest.raises(ResourceNotFoundError):
        index_client.get_index(INDEX_NAME)


def test_upload_and_search(index_client, search_client, test_index):
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


def test_search_match_all(index_client, search_client, test_index):
    index_client.create_index(test_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "alpha"},
            {"id": "2", "title": "beta"},
        ]
    )

    results = list(search_client.search(search_text="*", top=10))
    assert len(results) == 2


def test_operations_on_deleted_index_fail(index_client, search_client, test_index):
    index_client.create_index(test_index)
    index_client.delete_index(INDEX_NAME)

    # search() returns a lazy paged iterator; iterate to trigger the request.
    with pytest.raises(ResourceNotFoundError):
        list(search_client.search(search_text="test", top=1))
