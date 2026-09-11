"""SDK compatibility tests: every supported operation through the official SDK."""

import json

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient

# KnowledgeBase/SearchAlias/SearchIndexKnowledgeSource are not in the pinned
# azure-search-documents==12.0.0; they are exercised ahead of the next SDK bump.
from azure.search.documents.indexes.models import (  # type: ignore[attr-defined]
    AnalyzeTextOptions,
    KnowledgeBase,
    SearchableField,
    SearchAlias,
    SearchField,
    SearchFieldDataType,
    SearchIndex,
    SearchIndexKnowledgeSource,
    SearchSuggester,
    SimpleField,
    SynonymMap,
)
from azure.search.documents.knowledgebases import KnowledgeBaseRetrievalClient
from azure.search.documents.knowledgebases.models import (
    KnowledgeBaseRetrievalRequest,
    KnowledgeRetrievalSemanticIntent,
)

API_KEY = "test-key"
INDEX_NAME = "sdk-index"
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
def full_index() -> SearchIndex:
    return SearchIndex(
        name=INDEX_NAME,
        fields=[
            SearchField(name="id", type=SearchFieldDataType.String, key=True),
            SearchableField(name="title", type=SearchFieldDataType.String, filterable=True),
            SimpleField(name="price", type=SearchFieldDataType.Double, filterable=True, sortable=True),
            SimpleField(
                name="tags",
                type=SearchFieldDataType.Collection(SearchFieldDataType.String),
                filterable=True,
                facetable=True,
            ),
        ],
    )


@pytest.fixture()
def created_index(index_client: SearchIndexClient, full_index: SearchIndex) -> SearchIndex:
    return index_client.create_index(full_index)


def test_index_crud(index_client: SearchIndexClient, full_index: SearchIndex) -> None:
    created = index_client.create_index(full_index)
    assert created.name == INDEX_NAME
    fetched = index_client.get_index(INDEX_NAME)
    assert fetched.name == INDEX_NAME
    names = [index.name for index in index_client.list_indexes()]
    assert names == [INDEX_NAME]
    index_client.delete_index(INDEX_NAME)
    with pytest.raises(HttpResponseError):
        index_client.get_index(INDEX_NAME)


def test_upload_merge_delete(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    results = search_client.upload_documents(
        documents=[
            {"id": "1", "title": "one", "price": 1.0, "tags": ["a"]},
            {"id": "2", "title": "two", "price": 2.0, "tags": ["b"]},
        ]
    )
    assert all(r.succeeded for r in results)

    results = search_client.merge_documents(documents=[{"id": "1", "price": 9.0}])
    assert all(r.succeeded for r in results)

    found = list(search_client.search(search_text="one"))
    assert len(found) == 1
    assert found[0]["price"] == 9.0
    assert found[0]["title"] == "one"

    results = search_client.delete_documents(documents=[{"id": "1"}])
    assert all(r.succeeded for r in results)
    assert len(list(search_client.search(search_text="*"))) == 1


def test_merge_or_upload(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(documents=[{"id": "1", "title": "one", "price": 1.0}])

    results = search_client.merge_or_upload_documents(
        documents=[
            {"id": "1", "price": 5.0},
            {"id": "2", "title": "two", "price": 3.0},
        ]
    )
    assert all(r.succeeded for r in results)
    found = {doc["id"]: doc for doc in search_client.search(search_text="*")}
    assert found["1"]["price"] == 5.0
    assert found["1"]["title"] == "one"
    assert found["2"]["title"] == "two"


@pytest.fixture()
def created_full_index(index_client: SearchIndexClient, full_index: SearchIndex) -> SearchIndex:
    return index_client.create_index(full_index)


@pytest.fixture()
def priced_docs(search_client: SearchClient, created_full_index: SearchIndex) -> SearchClient:
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "cheap red", "price": 5.0, "tags": ["red"]},
            {"id": "2", "title": "mid blue", "price": 50.0, "tags": ["blue", "red"]},
            {"id": "3", "title": "expensive green", "price": 500.0, "tags": ["green"]},
        ]
    )
    return search_client


def test_search_filter(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", filter="price ge 50"))
    assert {doc["id"] for doc in found} == {"2", "3"}
    found = list(priced_docs.search(search_text="*", filter="price lt 10 or price gt 100"))
    assert {doc["id"] for doc in found} == {"1", "3"}
    found = list(priced_docs.search(search_text="mid", filter="price gt 10"))
    assert [doc["id"] for doc in found] == ["2"]


def test_search_order_by(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", order_by="price desc"))
    assert [doc["id"] for doc in found] == ["3", "2", "1"]
    found = list(priced_docs.search(search_text="*", order_by="price asc"))
    assert [doc["id"] for doc in found] == ["1", "2", "3"]


def test_search_select(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", select=["id", "price"]))
    assert len(found) == 3
    for doc in found:
        assert "id" in doc
        assert "price" in doc
        assert "title" not in doc
        assert "tags" not in doc


def test_search_facets(priced_docs: SearchClient) -> None:
    results = priced_docs.search(search_text="*", facets=["tags"])
    facets = results.get_facets()
    assert facets is not None
    values = {entry["value"]: entry["count"] for entry in facets["tags"]}
    assert values == {"red": 2, "blue": 1, "green": 1}


def test_search_fields(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="red", search_fields=["title"]))
    assert {doc["id"] for doc in found} == {"1"}


def test_search_paging(priced_docs: SearchClient) -> None:
    pages = list(priced_docs.search(search_text="*", top=2).by_page())
    assert len(pages) == 2
    assert [doc["id"] for doc in pages[0]] == ["1", "2"]
    assert [doc["id"] for doc in pages[1]] == ["3"]


def test_search_count(priced_docs: SearchClient) -> None:
    results = priced_docs.search(search_text="*", include_total_count=True)
    assert results.get_count() == 3


def test_get_document(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(documents=[{"id": "1", "title": "one", "price": 1.0}])
    doc = search_client.get_document(key="1")
    assert doc["id"] == "1"
    assert doc["title"] == "one"
    assert doc["price"] == 1.0
    with pytest.raises(HttpResponseError):
        search_client.get_document(key="missing")


def test_search_facet_options(priced_docs: SearchClient) -> None:
    results = priced_docs.search(search_text="*", facets=["tags,count:1"])
    facets = results.get_facets()
    assert facets is not None
    assert len(facets["tags"]) == 1
    assert facets["tags"][0]["value"] == "red"
    assert facets["tags"][0]["count"] == 2


def test_geography_point_upload(index_client: SearchIndexClient, search_client: SearchClient) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="location", type=SearchFieldDataType.GeographyPoint),
            ],
        )
    )
    results = search_client.upload_documents(
        documents=[
            {"id": "1", "location": {"type": "Point", "coordinates": [-122.13, 47.67]}},
        ]
    )
    assert all(r.succeeded for r in results)
    doc = search_client.get_document(key="1")
    assert doc["location"] == {"type": "Point", "coordinates": [-122.13, 47.67]}


def test_complex_type_filter(index_client: SearchIndexClient, search_client: SearchClient) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchableField(name="name", type=SearchFieldDataType.String),
                SearchField(
                    name="address",
                    type=SearchFieldDataType.ComplexType,
                    fields=[
                        SearchField(name="city", type=SearchFieldDataType.String, filterable=True),
                        SearchField(
                            name="state",
                            type=SearchFieldDataType.String,
                            filterable=True,
                        ),
                    ],
                ),
            ],
        )
    )
    results = search_client.upload_documents(
        documents=[
            {"id": "1", "name": "one", "address": {"city": "Miami", "state": "FL"}},
            {"id": "2", "name": "two", "address": {"city": "Seattle", "state": "WA"}},
        ]
    )
    assert all(r.succeeded for r in results)
    found = list(search_client.search(search_text="*", filter="address/state eq 'FL'"))
    assert [doc["id"] for doc in found] == ["1"]
    assert found[0]["address"] == {"city": "Miami", "state": "FL"}


def test_collection_of_complex_type(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="name", type=SearchFieldDataType.String, searchable=True),
                SearchField(
                    name="address",
                    type=SearchFieldDataType.Collection(SearchFieldDataType.ComplexType),
                    fields=[
                        SearchField(name="city", type=SearchFieldDataType.String, searchable=True),
                        SearchField(name="state", type=SearchFieldDataType.String),
                    ],
                ),
            ],
        )
    )
    results = search_client.upload_documents(
        documents=[
            {
                "id": "1",
                "name": "contoso",
                "address": [
                    {"city": "Miami", "state": "FL"},
                    {"city": "Seattle", "state": "WA"},
                ],
            },
            {"id": "2", "name": "fabrikam", "address": [{"city": "Montreal", "state": "QC"}]},
        ]
    )
    assert all(r.succeeded for r in results)

    # A searchable subfield is indexed across all collection elements.
    found = list(search_client.search(search_text="Seattle"))
    assert [doc["id"] for doc in found] == ["1"]
    doc = search_client.get_document(key="1")
    assert doc["address"] == [
        {"city": "Miami", "state": "FL"},
        {"city": "Seattle", "state": "WA"},
    ]


def test_count_documents(priced_docs: SearchClient) -> None:
    assert priced_docs.get_document_count() == 3


def test_count_documents_empty(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    assert search_client.get_document_count() == 0


def test_count_documents_missing_index(clean_emulator: str) -> None:
    client = SearchClient(
        endpoint=clean_emulator,
        index_name="missing",
        credential=CREDENTIAL,
        api_version=API_VERSION,
    )
    with pytest.raises(ResourceNotFoundError):
        client.get_document_count()


def test_service_statistics(index_client: SearchIndexClient) -> None:
    stats = index_client.get_service_statistics()
    assert stats.counters is not None
    assert stats.limits is not None


def test_analyze_text(index_client: SearchIndexClient, created_index: SearchIndex) -> None:
    result = index_client.analyze_text(INDEX_NAME, AnalyzeTextOptions(text="Hello, World!"))
    tokens = [token.token for token in result.tokens]
    assert "hello" in tokens
    assert "world" in tokens


def test_search_boolean_operators(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
            ],
        )
    )
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure search"},
            {"id": "2", "title": "azure emulators"},
            {"id": "3", "title": "other"},
            {"id": "4", "title": "quick brown fox"},
            {"id": "5", "title": "brown quick"},
        ]
    )

    found = list(search_client.search(search_text="azure -emulators"))
    assert [doc["id"] for doc in found] == ["1"]

    found = list(search_client.search(search_text="-azure"))
    assert {doc["id"] for doc in found} == {"3", "4", "5"}

    found = list(search_client.search(search_text='"quick brown"'))
    assert [doc["id"] for doc in found] == ["4"]


def test_search_empty_text_matches_all(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text=""))
    assert len(found) == 3


def test_search_collection_any_all(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", filter="tags any t eq 'red'"))
    assert {doc["id"] for doc in found} == {"1", "2"}

    found = list(priced_docs.search(search_text="*", filter="tags/any(t: t eq 'red')"))
    assert {doc["id"] for doc in found} == {"1", "2"}

    found = list(priced_docs.search(search_text="*", filter="tags/all(t: t ne 'green')"))
    assert {doc["id"] for doc in found} == {"1", "2"}


def test_search_facet_top_option(priced_docs: SearchClient) -> None:
    results = priced_docs.search(search_text="*", facets=["tags,top:1"])
    facets = results.get_facets()
    assert len(facets["tags"]) == 1
    assert facets["tags"][0]["value"] == "red"
    assert facets["tags"][0]["count"] == 2


def test_search_facet_star_expands_all_facetable(priced_docs: SearchClient) -> None:
    results = priced_docs.search(search_text="*", facets=["*"])
    facets = results.get_facets()
    assert "tags" in facets


def test_search_order_by_multiple_fields(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SimpleField(name="price", type=SearchFieldDataType.Double, sortable=True),
                SimpleField(name="rating", type=SearchFieldDataType.Int32, sortable=True),
            ],
        )
    )
    search_client.upload_documents(
        documents=[
            {"id": "1", "price": 5.0, "rating": 2},
            {"id": "2", "price": 5.0, "rating": 1},
            {"id": "3", "price": 1.0, "rating": 9},
        ]
    )
    found = list(search_client.search(search_text="*", order_by="price asc, rating desc"))
    assert [doc["id"] for doc in found] == ["3", "1", "2"]


def test_create_or_update_index_discards_documents(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(documents=[{"id": "1", "title": "one"}])
    assert search_client.get_document_count() == 1

    index_client.create_or_update_index(full_index)
    assert search_client.get_document_count() == 0


def test_merge_missing_document_reports_404(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    results = search_client.merge_documents(documents=[{"id": "missing", "price": 1.0}])
    assert results[0].succeeded is False
    assert results[0].status_code == 404


def test_delete_missing_document_reports_404(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    results = search_client.delete_documents(documents=[{"id": "missing"}])
    assert results[0].succeeded is False
    assert results[0].status_code == 404


def test_upload_invalid_document_reports_per_document_error(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    results = search_client.upload_documents(
        documents=[
            {"id": "1", "title": "one", "price": 1.0},
            {"id": "2", "title": "two", "price": "not-a-number"},
        ]
    )
    assert results[0].succeeded is True
    assert results[1].succeeded is False
    assert results[1].status_code == 400
    # The valid document in the same batch is still indexed.
    assert search_client.get_document_count() == 1


def test_upload_to_missing_index_returns_404(clean_emulator: str) -> None:
    client = SearchClient(
        endpoint=clean_emulator,
        index_name="missing",
        credential=CREDENTIAL,
        api_version=API_VERSION,
    )
    with pytest.raises(ResourceNotFoundError):
        client.upload_documents(documents=[{"id": "1"}])


def test_unsupported_query_options_rejected(priced_docs: SearchClient) -> None:
    cases = [
        {"scoring_profile": "profile"},
        {"semantic_configuration_name": "config"},
        {"query_type": "full"},
    ]
    for options in cases:
        with pytest.raises(HttpResponseError) as exc_info:
            list(priced_docs.search(search_text="*", **options))
        assert exc_info.value.status_code == 400
        response = exc_info.value.response
        assert response is not None
        assert json.loads(response.text())["error"]["code"] == "UnsupportedQuery"

    # An invalid searchMode is a malformed query, not an unsupported option.
    with pytest.raises(HttpResponseError) as exc_info:
        list(priced_docs.search(search_text="*", search_mode="exact"))
    assert exc_info.value.status_code == 400
    response = exc_info.value.response
    assert response is not None
    assert json.loads(response.text())["error"]["code"] == "InvalidQuery"


def test_error_body_is_azure_structured(index_client: SearchIndexClient) -> None:
    with pytest.raises(HttpResponseError) as exc_info:
        index_client.get_index("missing")
    response = exc_info.value.response
    assert response is not None
    body = json.loads(response.text())
    assert set(body) == {"error"}
    assert set(body["error"]) == {"code", "message"}
    assert body["error"]["code"] == "ResourceNotFound"


def test_suggest_and_autocomplete(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
            ],
            suggesters=[SearchSuggester(name="sg", source_fields=["title"])],
        )
    )
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "Boston Harbor Hotel"},
            {"id": "2", "title": "Portland Airport Inn"},
        ]
    )

    suggestions = search_client.suggest(search_text="bos", suggester_name="sg")
    assert [doc["id"] for doc in suggestions] == ["1"]
    assert suggestions[0].text == "Boston"

    completions = search_client.autocomplete(search_text="bos", suggester_name="sg")
    assert [item.text for item in completions] == ["Boston"]
    assert completions[0].query_plus_text == "bos Boston"


def test_synonym_map_crud(index_client: SearchIndexClient) -> None:
    created = index_client.create_synonym_map(SynonymMap(name="sm", synonyms=["a", "b"]))
    assert created.name == "sm"

    fetched = index_client.get_synonym_map("sm")
    assert fetched.synonyms == ["a", "b"]

    names = [sm.name for sm in index_client.get_synonym_maps()]
    assert names == ["sm"]

    updated = index_client.create_or_update_synonym_map(
        SynonymMap(name="sm", synonyms=["a", "c"])
    )
    assert updated.synonyms == ["a", "c"]

    index_client.delete_synonym_map("sm")
    with pytest.raises(ResourceNotFoundError):
        index_client.get_synonym_map("sm")


def test_alias_crud(index_client: SearchIndexClient) -> None:
    created = index_client.create_alias(SearchAlias(name="al", indexes=["i1"]))
    assert created.name == "al"

    fetched = index_client.get_alias("al")
    assert fetched.indexes == ["i1"]

    names = [alias.name for alias in index_client.list_aliases()]
    assert names == ["al"]

    updated = index_client.create_or_update_alias(SearchAlias(name="al", indexes=["i2"]))
    assert updated.indexes == ["i2"]

    index_client.delete_alias("al")
    with pytest.raises(ResourceNotFoundError):
        index_client.get_alias("al")


def test_knowledge_source_crud(index_client: SearchIndexClient) -> None:
    created = index_client.create_knowledge_source(
        SearchIndexKnowledgeSource(
            name="src1",
            kind="searchIndex",
            search_index_parameters={"searchIndexName": "i1"},
        )
    )
    assert created.name == "src1"

    fetched = index_client.get_knowledge_source("src1")
    assert fetched.kind == "searchIndex"

    names = [source.name for source in index_client.list_knowledge_sources()]
    assert names == ["src1"]

    updated = index_client.create_or_update_knowledge_source(
        SearchIndexKnowledgeSource(
            name="src1",
            kind="searchIndex",
            description="updated",
            search_index_parameters={"searchIndexName": "i1"},
        )
    )
    assert updated.description == "updated"

    index_client.delete_knowledge_source("src1")
    with pytest.raises(ResourceNotFoundError):
        index_client.get_knowledge_source("src1")


def test_knowledge_base_crud(clean_emulator: str, index_client: SearchIndexClient) -> None:
    index_client.create_knowledge_source(
        SearchIndexKnowledgeSource(
            name="src1",
            kind="searchIndex",
            search_index_parameters={"searchIndexName": "i1"},
        )
    )
    created = index_client.create_knowledge_base(
        KnowledgeBase(name="kb1", knowledge_sources=[{"name": "src1"}])
    )
    assert created.name == "kb1"

    fetched = index_client.get_knowledge_base("kb1")
    assert fetched.knowledge_sources[0]["name"] == "src1"

    names = [base.name for base in index_client.list_knowledge_bases()]
    assert names == ["kb1"]

    index_client.delete_knowledge_base("kb1")
    with pytest.raises(ResourceNotFoundError):
        index_client.get_knowledge_base("kb1")


def test_knowledge_base_retrieve_returns_empty(
    clean_emulator: str, index_client: SearchIndexClient
) -> None:
    index_client.create_knowledge_source(
        SearchIndexKnowledgeSource(
            name="src1",
            kind="searchIndex",
            search_index_parameters={"searchIndexName": "i1"},
        )
    )
    index_client.create_knowledge_base(
        KnowledgeBase(name="kb1", knowledge_sources=[{"name": "src1"}])
    )

    client = KnowledgeBaseRetrievalClient(
        endpoint=clean_emulator,
        knowledge_base_name="kb1",
        credential=CREDENTIAL,
        api_version=API_VERSION,
    )
    response = client.retrieve(
        KnowledgeBaseRetrievalRequest(
            intents=[KnowledgeRetrievalSemanticIntent(type="semantic", search="hotels")]
        )
    )
    assert response.response == []
    assert response.activity == []
    assert response.references == []


def test_search_mode_any_matches_union(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure search", "price": 1.0},
            {"id": "2", "title": "local emulators", "price": 2.0},
            {"id": "3", "title": "unrelated", "price": 3.0},
        ]
    )
    # Default (all/AND): no document contains both terms.
    assert list(search_client.search(search_text="azure emulators")) == []
    # Explicit all: same AND semantics.
    found = list(search_client.search(search_text="azure emulators", search_mode="all"))
    assert found == []
    # Any (OR): either term matches.
    found = list(search_client.search(search_text="azure emulators", search_mode="any"))
    assert {doc["id"] for doc in found} == {"1", "2"}


def test_search_stemming_and_stopwords(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "running shoes", "price": 1.0},
            {"id": "2", "title": "other things", "price": 2.0},
        ]
    )
    # Inflected forms reduce to the same stem.
    for term in ["run", "runs", "running"]:
        found = list(search_client.search(search_text=term))
        assert [doc["id"] for doc in found] == ["1"], f"term {term!r}"
    # "the" is an English stopword: it matches nothing.
    assert list(search_client.search(search_text="the")) == []


def test_search_fuzzy_matches_typos(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure emulator", "price": 1.0},
            {"id": "2", "title": "other things", "price": 2.0},
        ]
    )
    # Typos within edit distance 1 of the indexed stem "emul".
    for term in ["emu~", "omul~", "emul~2"]:
        found = list(search_client.search(search_text=term))
        assert [doc["id"] for doc in found] == ["1"], f"term {term!r}"
    # Unrelated terms match nothing, even fuzzy.
    assert list(search_client.search(search_text="zzz~")) == []


def test_search_scores_rank_by_relevance(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure azure azure", "price": 1.0},
            {"id": "2", "title": "azure", "price": 2.0},
        ]
    )
    found = list(search_client.search(search_text="azure"))
    assert [doc["id"] for doc in found] == ["1", "2"]
    scores = [doc["@search.score"] for doc in found]
    assert all(score > 0 for score in scores)
    assert scores[0] >= scores[1]


def test_search_highlights(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "Azure Search Rocks", "price": 1.0},
            {"id": "2", "title": "Other things", "price": 2.0},
        ]
    )
    found = list(search_client.search(search_text="azure", highlight_fields="title"))
    assert len(found) == 1
    assert found[0]["@search.highlights"] == {"title": ["<em>Azure</em> Search Rocks"]}

    found = list(
        search_client.search(
            search_text="azure",
            highlight_fields="title",
            highlight_pre_tag="<b>",
            highlight_post_tag="</b>",
        )
    )
    assert found[0]["@search.highlights"] == {"title": ["<b>Azure</b> Search Rocks"]}

    # Highlighting an unknown field is rejected explicitly.
    with pytest.raises(HttpResponseError) as exc_info:
        list(search_client.search(search_text="azure", highlight_fields="missing"))
    assert exc_info.value.status_code == 400


def test_search_fields_weights_boost_scores(priced_docs: SearchClient) -> None:
    plain = list(priced_docs.search(search_text="red"))
    assert len(plain) == 1
    plain_score = plain[0]["@search.score"]
    boosted = list(priced_docs.search(search_text="red", search_fields=["title^10"]))
    assert len(boosted) == 1
    assert boosted[0]["@search.score"] > plain_score


def test_search_filter_in_operator(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", filter="price in (5.0, 500.0)"))
    assert {doc["id"] for doc in found} == {"1", "3"}
    found = list(priced_docs.search(search_text="*", filter="price in (1.0, 2.0)"))
    assert found == []


def test_search_filter_string_functions(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", filter="startswith(title, 'cheap')"))
    assert [doc["id"] for doc in found] == ["1"]
    found = list(priced_docs.search(search_text="*", filter="endswith(title, 'blue')"))
    assert [doc["id"] for doc in found] == ["2"]
    found = list(priced_docs.search(search_text="*", filter="contains(title, 'ens')"))
    assert [doc["id"] for doc in found] == ["3"]
    found = list(priced_docs.search(search_text="*", filter="title in ('cheap red', 'other')"))
    assert [doc["id"] for doc in found] == ["1"]


def test_search_orderby_nulls_first_ascending_last_descending(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "priced", "price": 10.0},
            {"id": "2", "title": "unpriced"},
            {"id": "3", "title": "cheap", "price": 1.0},
        ]
    )
    found = list(search_client.search(search_text="*", order_by="price asc"))
    assert [doc["id"] for doc in found] == ["2", "3", "1"]
    found = list(search_client.search(search_text="*", order_by="price desc"))
    assert [doc["id"] for doc in found] == ["1", "3", "2"]


def test_alias_resolves_for_search_and_documents(
    clean_emulator: str,
    index_client: SearchIndexClient,
    search_client: SearchClient,
    full_index: SearchIndex,
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(documents=[{"id": "1", "title": "hello"}])
    index_client.create_alias(SearchAlias(name="al", indexes=[INDEX_NAME]))

    alias_client = SearchClient(
        endpoint=clean_emulator,
        index_name="al",
        credential=CREDENTIAL,
        api_version=API_VERSION,
    )
    found = list(alias_client.search(search_text="hello"))
    assert [doc["id"] for doc in found] == ["1"]
    doc = alias_client.get_document(key="1")
    assert doc["id"] == "1"
    assert alias_client.get_document_count() == 1


def test_suggest_and_autocomplete_infix(
    index_client: SearchIndexClient, search_client: SearchClient
) -> None:
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.String, key=True),
                SearchField(name="title", type=SearchFieldDataType.String, searchable=True),
            ],
            suggesters=[SearchSuggester(name="sg", source_fields=["title"])],
        )
    )
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "Boston Harbor Hotel"},
            {"id": "2", "title": "Portland Airport Inn"},
        ]
    )
    # "rbor" is an infix (not a prefix) of "Harbor".
    suggestions = search_client.suggest(search_text="rbor", suggester_name="sg")
    assert [doc["id"] for doc in suggestions] == ["1"]
    # "osto" is an infix of "Boston".
    completions = search_client.autocomplete(search_text="osto", suggester_name="sg")
    assert [item.text for item in completions] == ["Boston"]


def test_analyze_text_with_analyzer(index_client: SearchIndexClient, created_index: SearchIndex) -> None:
    result = index_client.analyze_text(
        INDEX_NAME, AnalyzeTextOptions(text="Running tests", analyzer_name="standard.lucene")
    )
    assert [token.token for token in result.tokens] == ["run", "test"]


def test_analyze_text_keyword_and_whitespace_analyzers(
    index_client: SearchIndexClient, created_index: SearchIndex
) -> None:
    result = index_client.analyze_text(
        INDEX_NAME, AnalyzeTextOptions(text="Running Tests", analyzer_name="keyword")
    )
    assert [token.token for token in result.tokens] == ["Running Tests"]
    result = index_client.analyze_text(
        INDEX_NAME, AnalyzeTextOptions(text="Running tests", analyzer_name="whitespace")
    )
    assert [token.token for token in result.tokens] == ["Running", "tests"]


def test_search_order_by_score(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure azure azure", "price": 1.0},
            {"id": "2", "title": "azure", "price": 2.0},
        ]
    )
    found = list(search_client.search(search_text="azure", order_by="@search.score desc"))
    assert [doc["id"] for doc in found] == ["1", "2"]
    found = list(search_client.search(search_text="azure", order_by="@search.score asc"))
    assert [doc["id"] for doc in found] == ["2", "1"]


def test_search_select_star(priced_docs: SearchClient) -> None:
    found = list(priced_docs.search(search_text="*", select=["*"]))
    assert len(found) == 3
    for doc in found:
        assert {"id", "title", "price", "tags"} <= set(doc)


def test_search_fuzzy_default_distance_two(
    index_client: SearchIndexClient, search_client: SearchClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "azure emulator", "price": 1.0},
            {"id": "2", "title": "other things", "price": 2.0},
        ]
    )
    # A bare `~` uses the default edit distance 2 (matching Azure): "eamu"
    # is two edits from the indexed stem "emul".
    found = list(search_client.search(search_text="eamu~"))
    assert [doc["id"] for doc in found] == ["1"]
    assert list(search_client.search(search_text="eamu~1")) == []


def test_alias_conflicts_with_index_name(
    index_client: SearchIndexClient, full_index: SearchIndex
) -> None:
    index_client.create_index(full_index)
    # An alias cannot take the name of an existing index.
    with pytest.raises(HttpResponseError) as exc_info:
        index_client.create_alias(SearchAlias(name=INDEX_NAME, indexes=[INDEX_NAME]))
    assert exc_info.value.status_code == 409
    # An index cannot take the name of an existing alias.
    index_client.create_alias(SearchAlias(name="al", indexes=[INDEX_NAME]))
    with pytest.raises(HttpResponseError) as exc_info:
        index_client.create_index(
            SearchIndex(
                name="al",
                fields=[SearchField(name="id", type=SearchFieldDataType.String, key=True)],
            )
        )
    assert exc_info.value.status_code == 409
