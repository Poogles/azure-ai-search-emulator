"""SDK compatibility tests for vector search (Phase 2.1)."""

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    ExhaustiveKnnAlgorithmConfiguration,
    ExhaustiveKnnParameters,
    HnswAlgorithmConfiguration,
    HnswParameters,
    SearchField,
    SearchIndex,
    VectorSearch,
    VectorSearchProfile,
)
from azure.search.documents.models import VectorizedQuery

API_KEY = "test-key"
INDEX_NAME = "vector-index"
API_VERSION = "2024-07-01"
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
def vector_index() -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type="Edm.String", key=True),
            SearchField(name="title", type="Edm.String", searchable=True, filterable=True),
            SearchField(name="category", type="Edm.String", filterable=True),
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
                    kind="hnsw",
                    parameters=HnswParameters(
                        m=4, ef_construction=40, ef_search=20, metric="cosine"
                    ),
                ),
                ExhaustiveKnnAlgorithmConfiguration(
                    name="eknn-1",
                    kind="exhaustiveKnn",
                    parameters=ExhaustiveKnnParameters(metric="cosine"),
                ),
            ],
            profiles=[
                VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1"),
            ],
        ),
    )


@pytest.fixture()
def vector_docs(search_client, index_client, vector_index):
    index_client.create_index(vector_index)
    results = search_client.upload_documents(
        documents=[
            {
                "id": "1",
                "title": "azure search",
                "category": "tech",
                "content_vector": [1.0, 0.0, 0.0],
            },
            {
                "id": "2",
                "title": "azure emulators",
                "category": "tech",
                "content_vector": [0.0, 1.0, 0.0],
            },
            {
                "id": "3",
                "title": "other",
                "category": "misc",
                "content_vector": [0.0, 0.0, 1.0],
            },
            {
                "id": "4",
                "title": "azure mixed",
                "category": "misc",
                "content_vector": [0.7, 0.7, 0.0],
            },
        ]
    )
    assert all(r.succeeded for r in results)
    return search_client


def test_vector_index_crud(index_client, vector_index):
    created = index_client.create_index(vector_index)
    assert created.name == INDEX_NAME
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.name == INDEX_NAME
    index_client.delete_index(INDEX_NAME)


def test_vector_search_returns_nearest_first(vector_docs):
    found = list(
        vector_docs.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[1.0, 0.0, 0.0], k_nearest_neighbors=2, fields="content_vector"
                )
            ]
        )
    )
    assert [doc["id"] for doc in found] == ["1", "4"]
    scores = [doc["@search.score"] for doc in found]
    assert scores[0] >= scores[1]


def test_vector_search_exhaustive(vector_docs):
    found = list(
        vector_docs.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[0.6, 0.6, 0.0],
                    k_nearest_neighbors=4,
                    fields="content_vector",
                    exhaustive=True,
                )
            ]
        )
    )
    assert len(found) == 4


def test_hybrid_search_returns_union(vector_docs):
    found = list(
        vector_docs.search(
            search_text="azure",
            vector_queries=[
                VectorizedQuery(
                    vector=[0.0, 0.0, 1.0], k_nearest_neighbors=2, fields="content_vector"
                )
            ],
        )
    )
    ids = {doc["id"] for doc in found}
    # Full-text matches 1, 2, 4; vector matches 3 — the union has all four.
    assert ids == {"1", "2", "3", "4"}


def test_vector_filter_modes(vector_docs):
    queries = [
        VectorizedQuery(
            vector=[1.0, 0.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
        )
    ]
    post = list(
        vector_docs.search(
            filter="category eq 'misc'",
            vector_queries=queries,
            vector_filter_mode="postFilter",
        )
    )
    # Top-1 by vector is doc 1 (tech), filtered out afterwards.
    assert post == []
    pre = list(
        vector_docs.search(
            filter="category eq 'misc'",
            vector_queries=queries,
            vector_filter_mode="preFilter",
        )
    )
    # Candidates are the misc docs first; top-1 is doc 4.
    assert [doc["id"] for doc in pre] == ["4"]
