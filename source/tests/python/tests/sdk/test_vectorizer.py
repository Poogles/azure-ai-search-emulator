"""SDK compatibility tests for vectorizer queries (Phase 2.4).

The pinned Python SDK (``azure-search-documents==12.0.0``) models vectorizers
as the ``VectorSearchVectorizer`` family (``azureOpenAI`` / ``customWebApi`` /
``aml``) and ``kind: "text"`` queries as ``VectorizableTextQuery``. The SDK has
no ``sourceContext`` property, so the emulator falls back to all searchable
string fields as the text source.
"""

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    AzureOpenAIVectorizer,
    AzureOpenAIVectorizerParameters,
    HnswAlgorithmConfiguration,
    HnswParameters,
    SearchField,
    SearchIndex,
    VectorSearch,
    VectorSearchProfile,
    VectorSearchVectorizer,
    WebApiVectorizer,
    WebApiVectorizerParameters,
)
from azure.search.documents.models import VectorizableTextQuery, VectorizedQuery

API_KEY = "test-key"
INDEX_NAME = "vectorizer-index"
API_VERSION = "2024-07-01"
CREDENTIAL = AzureKeyCredential(API_KEY)
DIMENSIONS = 64


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


def vectorizer_index(vectorizer: VectorSearchVectorizer | None = None) -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type="Edm.String", key=True),
            SearchField(name="title", type="Edm.String", searchable=True, filterable=True),
            SearchField(name="content", type="Edm.String", searchable=True),
            SearchField(name="category", type="Edm.String", filterable=True),
            SearchField(
                name="content_vector",
                type="Collection(Edm.Single)",
                searchable=True,
                vector_search_dimensions=DIMENSIONS,
                vector_search_profile_name="cos",
            ),
        ],
        vector_search=VectorSearch(
            algorithms=[
                HnswAlgorithmConfiguration(
                    name="hnsw-1",
                    parameters=HnswParameters(m=4, ef_construction=40, ef_search=20, metric="cosine"),
                ),
            ],
            # The vectorizer is associated with the field through the profile
            # (the pinned SDKs' wire format).
            profiles=[
                VectorSearchProfile(
                    name="cos", algorithm_configuration_name="hnsw-1", vectorizer_name="embedder"
                )
            ],
            # The pinned SDKs nest `vectorizers` inside `vectorSearch`.
            vectorizers=[
                vectorizer
                or AzureOpenAIVectorizer(
                    vectorizer_name="embedder",
                    parameters=AzureOpenAIVectorizerParameters(
                        resource_url="https://example-resource.openai.azure.com",
                        deployment_name="text-embedding-ada-002",
                    ),
                )
            ],
        ),
    )


@pytest.fixture()
def vectorizer_docs(
    search_client: SearchClient, index_client: SearchIndexClient
) -> SearchClient:
    """Creates the index and uploads documents WITHOUT explicit vectors."""
    index_client.create_index(vectorizer_index())
    results = search_client.upload_documents(
        documents=[
            {
                "id": "1",
                "title": "quantum computing",
                "content": "quantum computing applications",
                "category": "tech",
            },
            {
                "id": "2",
                "title": "classical physics",
                "content": "classical mechanics",
                "category": "tech",
            },
            {
                "id": "3",
                "title": "quantum mechanics",
                "content": "quantum physics",
                "category": "misc",
            },
        ]
    )
    assert all(r.succeeded for r in results)
    return search_client


def test_create_index_with_vectorizer(index_client: SearchIndexClient) -> None:
    created = index_client.create_index(vectorizer_index())
    assert created.name == INDEX_NAME
    assert created.vector_search is not None
    assert created.vector_search.vectorizers is not None
    assert created.vector_search.vectorizers[0].vectorizer_name == "embedder"
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.vector_search is not None
    assert fetched.vector_search.vectorizers is not None
    index_client.delete_index(INDEX_NAME)


def test_web_api_vectorizer_accepted(index_client: SearchIndexClient) -> None:
    """The ``customWebApi`` kind (with a URI) is accepted and echoed."""
    index = vectorizer_index(
        WebApiVectorizer(
            vectorizer_name="embedder",
            web_api_parameters=WebApiVectorizerParameters(url="https://example.com/embed"),
        )
    )
    created = index_client.create_index(index)
    assert created.vector_search is not None
    assert created.vector_search.vectorizers is not None
    assert created.vector_search.vectorizers[0].vectorizer_name == "embedder"
    index_client.delete_index(INDEX_NAME)


def test_upload_without_vector_generates_vector(
    search_client: SearchClient, index_client: SearchIndexClient
) -> None:
    index_client.create_index(vectorizer_index())
    results = search_client.upload_documents(
        documents=[{"id": "1", "title": "quantum computing", "content": "applications"}]
    )
    assert all(r.succeeded for r in results)
    doc = search_client.get_document(key="1")
    assert "content_vector" in doc
    assert len(doc["content_vector"]) == DIMENSIONS
    # A non-empty source text yields a non-zero vector.
    assert any(v != 0.0 for v in doc["content_vector"])


def test_text_query_orders_by_token_overlap(vectorizer_docs: SearchClient) -> None:
    found = list(
        vectorizer_docs.search(
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=3
                )
            ]
        )
    )
    ids = [doc["id"] for doc in found]
    # Doc 1 shares both query tokens; doc 3 shares "quantum"; doc 2 shares none.
    assert ids[0] == "1"
    assert ids.index("3") < ids.index("2")


def test_text_query_respects_k(vectorizer_docs: SearchClient) -> None:
    found = list(
        vectorizer_docs.search(
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=2
                )
            ]
        )
    )
    assert len(found) == 2


def test_text_query_hybrid_with_search_text(vectorizer_docs: SearchClient) -> None:
    found = list(
        vectorizer_docs.search(
            search_text="quantum",
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=3
                )
            ],
        )
    )
    ids = [doc["id"] for doc in found]
    # Doc 1 matches both the full-text and vectorizer sides, so it ranks first.
    assert ids[0] == "1"
    assert "3" in ids


def test_text_query_prefilter(vectorizer_docs: SearchClient) -> None:
    found = list(
        vectorizer_docs.search(
            filter="category eq 'misc'",
            vector_filter_mode="preFilter",
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=3
                )
            ],
        )
    )
    # Only the misc doc (3) is a candidate.
    assert [doc["id"] for doc in found] == ["3"]


def test_multiple_text_queries_union(vectorizer_docs: SearchClient) -> None:
    found = list(
        vectorizer_docs.search(
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=1
                ),
                VectorizableTextQuery(
                    text="classical physics", fields="content_vector", k_nearest_neighbors=1
                ),
            ]
        )
    )
    ids = {doc["id"] for doc in found}
    # The union contains the top match of each query.
    assert "1" in ids
    assert "2" in ids


def test_mixed_text_and_vector_queries(
    search_client: SearchClient, index_client: SearchIndexClient
) -> None:
    """A ``kind: "text"`` query and a raw ``kind: "vector"`` query union."""
    index = vectorizer_index()
    # Add a vectorizer-free profile and a raw-vector field for the vector query.
    assert index.vector_search is not None
    assert index.vector_search.profiles is not None
    index.vector_search.profiles.append(
        VectorSearchProfile(name="no_vz", algorithm_configuration_name="hnsw-1")
    )
    index.fields.append(
        SearchField(
            name="raw_vector",
            type="Collection(Edm.Single)",
            searchable=True,
            vector_search_dimensions=DIMENSIONS,
            vector_search_profile_name="no_vz",
        )
    )
    index_client.create_index(index)
    raw = [0.0] * DIMENSIONS
    raw[1] = 1.0
    results = search_client.upload_documents(
        documents=[
            {"id": "1", "title": "quantum computing", "content": "applications"},
            {"id": "2", "title": "unrelated", "content": "nothing", "raw_vector": raw},
        ]
    )
    assert all(r.succeeded for r in results)
    found = list(
        search_client.search(
            vector_queries=[
                VectorizableTextQuery(
                    text="quantum computing", fields="content_vector", k_nearest_neighbors=1
                ),
                VectorizedQuery(vector=raw, fields="raw_vector", k_nearest_neighbors=1),
            ]
        )
    )
    ids = {doc["id"] for doc in found}
    assert "1" in ids
    assert "2" in ids


def test_text_query_rejects_field_without_vectorizer(
    search_client: SearchClient, index_client: SearchIndexClient
) -> None:
    index = vectorizer_index()
    assert index.vector_search is not None
    assert index.vector_search.profiles is not None
    index.vector_search.profiles.append(
        VectorSearchProfile(name="no_vz", algorithm_configuration_name="hnsw-1")
    )
    index.fields.append(
        SearchField(
            name="raw_vector",
            type="Collection(Edm.Single)",
            searchable=True,
            vector_search_dimensions=DIMENSIONS,
            vector_search_profile_name="no_vz",
        )
    )
    index_client.create_index(index)
    search_client.upload_documents(
        documents=[{"id": "1", "title": "hello", "content": "world"}]
    )
    with pytest.raises(Exception) as exc_info:
        list(
            search_client.search(
                vector_queries=[
                    VectorizableTextQuery(
                        text="hello", fields="raw_vector", k_nearest_neighbors=1
                    )
                ]
            )
        )
    assert "vectorizer" in str(exc_info.value).lower()
