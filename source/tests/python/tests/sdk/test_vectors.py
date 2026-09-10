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
    SimpleField,
    VectorSearch,
    VectorSearchProfile,
)
from azure.search.documents.models import VectorizedQuery, VectorQuery

API_KEY = "test-key"
INDEX_NAME = "vector-index"
API_VERSION = "2024-07-01"
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
                    parameters=HnswParameters(
                        m=4, ef_construction=40, ef_search=20, metric="cosine"
                    ),
                ),
                ExhaustiveKnnAlgorithmConfiguration(
                    name="eknn-1",
                    parameters=ExhaustiveKnnParameters(metric="cosine"),
                ),
            ],
            profiles=[
                VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1"),
            ],
        ),
    )


@pytest.fixture()
def vector_docs(
    search_client: SearchClient, index_client: SearchIndexClient, vector_index: SearchIndex
) -> SearchClient:
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


def test_vector_index_crud(index_client: SearchIndexClient, vector_index: SearchIndex) -> None:
    created = index_client.create_index(vector_index)
    assert created.name == INDEX_NAME
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.name == INDEX_NAME
    index_client.delete_index(INDEX_NAME)


def test_vector_search_returns_nearest_first(vector_docs: SearchClient) -> None:
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


def test_vector_search_exhaustive(vector_docs: SearchClient) -> None:
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


def test_hybrid_search_returns_union(vector_docs: SearchClient) -> None:
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


def test_vector_filter_modes(vector_docs: SearchClient) -> None:
    queries: list[VectorQuery] = [
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


def test_multiple_vector_queries_union(vector_docs: SearchClient) -> None:
    found = list(
        vector_docs.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[1.0, 0.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
                ),
                VectorizedQuery(
                    vector=[0.0, 1.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
                ),
            ]
        )
    )
    assert {doc["id"] for doc in found} == {"1", "2"}


def _metric_index(metric: str) -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type="Edm.String", key=True),
            SearchField(
                name="v",
                type="Collection(Edm.Single)",
                searchable=True,
                vector_search_dimensions=2,
                vector_search_profile_name="p",
            ),
        ],
        vector_search=VectorSearch(
            algorithms=[
                HnswAlgorithmConfiguration(
                    name="hnsw-1",
                    parameters=HnswParameters(m=4, ef_construction=40, ef_search=20, metric=metric),
                )
            ],
            profiles=[VectorSearchProfile(name="p", algorithm_configuration_name="hnsw-1")],
        ),
    )


def test_dot_product_metric(index_client: SearchIndexClient, search_client: SearchClient) -> None:
    index_client.create_index(_metric_index("dotProduct"))
    search_client.upload_documents(
        documents=[
            {"id": "neg", "v": [-3.0, 0.0]},
            {"id": "zero", "v": [0.0, 0.0]},
            {"id": "big", "v": [100.0, 100.0]},
            {"id": "unit", "v": [1.0, 0.0]},
        ]
    )
    found = list(
        search_client.search(
            vector_queries=[
                VectorizedQuery(vector=[1.0, 1.0], k_nearest_neighbors=4, fields="v")
            ]
        )
    )
    # Raw inner products: 200, 1, 0, -3.
    assert [doc["id"] for doc in found] == ["big", "unit", "zero", "neg"]
    assert found[0]["@search.score"] == 200.0


def test_euclidean_metric(index_client: SearchIndexClient, search_client: SearchClient) -> None:
    index_client.create_index(_metric_index("euclidean"))
    search_client.upload_documents(
        documents=[
            {"id": "a", "v": [0.0, 0.0]},
            {"id": "b", "v": [1.0, 0.0]},
            {"id": "c", "v": [3.0, 0.0]},
        ]
    )
    found = list(
        search_client.search(
            vector_queries=[
                VectorizedQuery(vector=[0.0, 0.0], k_nearest_neighbors=3, fields="v")
            ]
        )
    )
    # Scores are 1/(1+l2): 1.0, 0.5, 0.25.
    assert [doc["id"] for doc in found] == ["a", "b", "c"]
    assert [doc["@search.score"] for doc in found] == [1.0, 0.5, 0.25]


def test_non_retrievable_vector_omitted_unless_selected(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type="Edm.String", key=True),
                SearchField(
                    name="content_vector",
                    type="Collection(Edm.Single)",
                    searchable=True,
                    retrievable=False,
                    vector_search_dimensions=3,
                    vector_search_profile_name="cos",
                ),
                SearchField(
                    name="flat_vector",
                    type="Collection(Edm.Single)",
                    searchable=True,
                    vector_search_dimensions=3,
                    vector_search_profile_name="eknn",
                ),
            ],
            vector_search=VectorSearch(
                algorithms=[
                    HnswAlgorithmConfiguration(
                        name="hnsw-1",
                        parameters=HnswParameters(m=4, ef_construction=40, ef_search=20),
                    ),
                    ExhaustiveKnnAlgorithmConfiguration(
                        name="eknn-1",
                        parameters=ExhaustiveKnnParameters(),
                    ),
                ],
                profiles=[
                    VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1"),
                    VectorSearchProfile(name="eknn", algorithm_configuration_name="eknn-1"),
                ],
            ),
        )
    )
    search_client.upload_documents(
        documents=[
            {
                "id": "1",
                "content_vector": [1.0, 0.0, 0.0],
                "flat_vector": [1.0, 0.0, 0.0],
            },
            {
                "id": "2",
                "content_vector": [0.0, 1.0, 0.0],
                "flat_vector": [0.0, 1.0, 0.0],
            },
        ]
    )
    queries: list[VectorQuery] = [
        VectorizedQuery(
            vector=[1.0, 0.0, 0.0], k_nearest_neighbors=1, fields="content_vector"
        )
    ]
    found = list(search_client.search(vector_queries=queries))
    assert found[0]["id"] == "1"
    assert "content_vector" not in found[0]
    assert "flat_vector" in found[0]

    found = list(
        search_client.search(select=["id", "content_vector"], vector_queries=queries)
    )
    assert found[0]["id"] == "1"
    assert "content_vector" in found[0]


def test_vector_search_with_orderby_orders_by_field(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type="Edm.String", key=True),
                SimpleField(name="price", type="Edm.Double", sortable=True),
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
                        parameters=HnswParameters(m=4, ef_construction=40, ef_search=20),
                    )
                ],
                profiles=[
                    VectorSearchProfile(name="cos", algorithm_configuration_name="hnsw-1")
                ],
            ),
        )
    )
    search_client.upload_documents(
        documents=[
            {"id": "1", "price": 5.0, "content_vector": [1.0, 0.0, 0.0]},
            {"id": "2", "price": 1.0, "content_vector": [0.0, 1.0, 0.0]},
        ]
    )
    # The vector query ranks doc 2 first, but orderby takes precedence.
    found = list(
        search_client.search(
            vector_queries=[
                VectorizedQuery(
                    vector=[0.0, 1.0, 0.0], k_nearest_neighbors=2, fields="content_vector"
                )
            ],
            order_by=["price asc"],
        )
    )
    assert [doc["id"] for doc in found] == ["2", "1"]
