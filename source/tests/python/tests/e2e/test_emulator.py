"""E2E tests: official Azure AI Search SDK against the containerised emulator."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    SearchField,
    SearchFieldDataType,
    SearchIndex,
)

API_KEY = "test-key"
INDEX_NAME = "e2e-index"


def _index_client(endpoint: str) -> SearchIndexClient:
    return SearchIndexClient(endpoint=endpoint, credential=API_KEY)


def _search_client(endpoint: str) -> SearchClient:
    return SearchClient(endpoint=endpoint, index_name=INDEX_NAME, credential=API_KEY)


def _test_index() -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type=SearchFieldDataType.STRING, key=True),
            SearchField(name="title", type=SearchFieldDataType.STRING, searchable=True),
        ],
    )


class TestHealth:
    def test_health_endpoint(self, clean_emulator):
        import urllib.request

        with urllib.request.urlopen(f"{clean_emulator}/health", timeout=5) as resp:
            assert resp.status == 200
            body = resp.read().decode()
            assert "ok" in body


class TestIndexLifecycle:
    def test_create_index(self, clean_emulator):
        client = _index_client(clean_emulator)
        client.create_index(_test_index())
        created = client.get_index(INDEX_NAME)
        assert created.name == INDEX_NAME
        assert len(created.fields) == 2

    def test_create_duplicate_index_fails(self, clean_emulator):
        client = _index_client(clean_emulator)
        client.create_index(_test_index())
        with pytest.raises(Exception):
            client.create_index(_test_index())

    def test_delete_index(self, clean_emulator):
        client = _index_client(clean_emulator)
        client.create_index(_test_index())
        client.delete_index(INDEX_NAME)
        with pytest.raises(ResourceNotFoundError):
            client.get_index(INDEX_NAME)


class TestDocuments:
    def test_upload_and_search(self, clean_emulator):
        index_client = _index_client(clean_emulator)
        index_client.create_index(_test_index())

        search_client = _search_client(clean_emulator)
        docs = [
            {"id": "1", "title": "hello world"},
            {"id": "2", "title": "foo bar"},
            {"id": "3", "title": "hello there"},
        ]
        results = search_client.upload_documents(documents=docs)
        assert len(results) == 3
        assert all(r.status for r in results)

        search_results = search_client.search(search_text="hello", top=10)
        found = list(search_results)
        assert len(found) == 2
        ids = {doc["id"] for doc in found}
        assert ids == {"1", "3"}

    def test_search_match_all(self, clean_emulator):
        index_client = _index_client(clean_emulator)
        index_client.create_index(_test_index())

        search_client = _search_client(clean_emulator)
        search_client.upload_documents(
            documents=[
                {"id": "1", "title": "alpha"},
                {"id": "2", "title": "beta"},
            ]
        )

        results = list(search_client.search(search_text="*", top=10))
        assert len(results) == 2

    def test_operations_on_deleted_index_fail(self, clean_emulator):
        index_client = _index_client(clean_emulator)
        index_client.create_index(_test_index())
        index_client.delete_index(INDEX_NAME)

        search_client = _search_client(clean_emulator)
        with pytest.raises(ResourceNotFoundError):
            search_client.search(search_text="test", top=1)
