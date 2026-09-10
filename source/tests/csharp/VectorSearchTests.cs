using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.Models;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// Vector search compatibility tests (Phase 2.1). Mirrors
/// source/tests/python/tests/sdk/test_vectors.py.
/// </summary>
public class VectorSearchTests : EmulatorTestBase
{
    private const string IndexName = "vector-index";

    public VectorSearchTests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    private static SearchIndex VectorIndex() => new(IndexName)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchableField("title") { IsFilterable = true },
            new SimpleField("category", SearchFieldDataType.String) { IsFilterable = true },
            new SearchField("content_vector", new SearchFieldDataType("Collection(Edm.Single)"))
            {
                IsSearchable = true,
                VectorSearchDimensions = 3,
                VectorSearchProfileName = "cos",
            },
        },
        VectorSearch = new VectorSearch
        {
            Algorithms =
            {
                new HnswAlgorithmConfiguration("hnsw-1")
                {
                    Parameters = new HnswParameters { M = 4, EfConstruction = 40, EfSearch = 20, Metric = "cosine" },
                },
                new ExhaustiveKnnAlgorithmConfiguration("eknn-1")
                {
                    Parameters = new ExhaustiveKnnParameters { Metric = "cosine" },
                },
            },
            Profiles = { new VectorSearchProfile("cos", "hnsw-1") },
        },
    };

    private static SearchIndex MetricIndex(string metric) => new(IndexName)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchField("v", new SearchFieldDataType("Collection(Edm.Single)"))
            {
                IsSearchable = true,
                VectorSearchDimensions = 2,
                VectorSearchProfileName = "p",
            },
        },
        VectorSearch = new VectorSearch
        {
            Algorithms =
            {
                new HnswAlgorithmConfiguration("hnsw-1")
                {
                    Parameters = new HnswParameters { M = 4, EfConstruction = 40, EfSearch = 20, Metric = metric },
                },
            },
            Profiles = { new VectorSearchProfile("p", "hnsw-1") },
        },
    };

    private static VectorizedQuery Query(float[] vector, int k, bool exhaustive = false) =>
        new(vector)
        {
            KNearestNeighborsCount = k,
            Fields = { "content_vector" },
            Exhaustive = exhaustive ? true : null,
        };

    private async Task<SearchClient> VectorDocsAsync()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(VectorIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure search", ["category"] = "tech", ["content_vector"] = new[] { 1.0f, 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "2", ["title"] = "azure emulators", ["category"] = "tech", ["content_vector"] = new[] { 0.0f, 1.0f, 0.0f } },
            new SearchDocument { ["id"] = "3", ["title"] = "other", ["category"] = "misc", ["content_vector"] = new[] { 0.0f, 0.0f, 1.0f } },
            new SearchDocument { ["id"] = "4", ["title"] = "azure mixed", ["category"] = "misc", ["content_vector"] = new[] { 0.7f, 0.7f, 0.0f } },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        return searchClient;
    }

    [Fact]
    public async Task VectorIndexCrud()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateIndexAsync(VectorIndex())).Value;
        Assert.Equal(IndexName, created.Name);
        var fetched = (await indexClient.GetIndexAsync(IndexName)).Value;
        Assert.Equal(IndexName, fetched.Name);
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
    }

    [Fact]
    public async Task VectorSearchReturnsNearestFirst()
    {
        var searchClient = await VectorDocsAsync();
        var found = await RunSearchWithScores<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { Query([1.0f, 0.0f, 0.0f], 2) } },
        });
        Assert.Equal(new[] { "1", "4" }, found.Select(d => (string)d.Document["id"]));
        Assert.True(found[0].Score >= found[1].Score);
    }

    [Fact]
    public async Task VectorSearchExhaustive()
    {
        var searchClient = await VectorDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { Query([0.6f, 0.6f, 0.0f], 4, exhaustive: true) } },
        });
        Assert.Equal(4, found.Count);
    }

    [Fact]
    public async Task HybridSearchReturnsUnion()
    {
        var searchClient = await VectorDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { Query([0.0f, 0.0f, 1.0f], 2) } },
        }, "azure");
        // Full-text matches 1, 2, 4; vector matches 3 — the union has all four.
        Assert.Equal(new[] { "1", "2", "3", "4" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
    }

    [Fact]
    public async Task VectorFilterModes()
    {
        var searchClient = await VectorDocsAsync();
        var post = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            Filter = "category eq 'misc'",
            VectorSearch = new VectorSearchOptions
            {
                Queries = { Query([1.0f, 0.0f, 0.0f], 1) },
                FilterMode = VectorFilterMode.PostFilter,
            },
        });
        // Top-1 by vector is doc 1 (tech), filtered out afterwards.
        Assert.Empty(post);

        var pre = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            Filter = "category eq 'misc'",
            VectorSearch = new VectorSearchOptions
            {
                Queries = { Query([1.0f, 0.0f, 0.0f], 1) },
                FilterMode = VectorFilterMode.PreFilter,
            },
        });
        // Candidates are the misc docs first; top-1 is doc 4.
        Assert.Equal(new[] { "4" }, pre.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task MultipleVectorQueriesUnion()
    {
        var searchClient = await VectorDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries =
                {
                    Query([1.0f, 0.0f, 0.0f], 1),
                    Query([0.0f, 1.0f, 0.0f], 1),
                },
            },
        });
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
    }

    [Fact]
    public async Task DotProductMetric()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(MetricIndex("dotProduct"));
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "neg", ["v"] = new[] { -3.0f, 0.0f } },
            new SearchDocument { ["id"] = "zero", ["v"] = new[] { 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "big", ["v"] = new[] { 100.0f, 100.0f } },
            new SearchDocument { ["id"] = "unit", ["v"] = new[] { 1.0f, 0.0f } },
        });
        var found = await RunSearchWithScores<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries = { new VectorizedQuery(new float[] { 1.0f, 1.0f }) { KNearestNeighborsCount = 4, Fields = { "v" } } },
            },
        });
        // Raw inner products: 200, 1, 0, -3.
        Assert.Equal(new[] { "big", "unit", "zero", "neg" }, found.Select(d => (string)d.Document["id"]));
        Assert.Equal(200.0, found[0].Score);
    }

    [Fact]
    public async Task EuclideanMetric()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(MetricIndex("euclidean"));
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "a", ["v"] = new[] { 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "b", ["v"] = new[] { 1.0f, 0.0f } },
            new SearchDocument { ["id"] = "c", ["v"] = new[] { 3.0f, 0.0f } },
        });
        var found = await RunSearchWithScores<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries = { new VectorizedQuery(new float[] { 0.0f, 0.0f }) { KNearestNeighborsCount = 3, Fields = { "v" } } },
            },
        });
        // Scores are 1/(1+l2): 1.0, 0.5, 0.25.
        Assert.Equal(new[] { "a", "b", "c" }, found.Select(d => (string)d.Document["id"]));
        Assert.Equal(new double?[] { 1.0, 0.5, 0.25 }, found.Select(d => d.Score).ToArray());
    }

    [Fact]
    public async Task NonRetrievableVectorOmittedUnlessSelected()
    {
        // The .NET SDK does not expose the `retrievable` field attribute, so the
        // index is created over raw HTTP; upload and search still go through the SDK.
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        var url = $"{BaseUrl}/indexes?api-version={ApiVersion}";
        var (status, body) = await RawPostAsync(url, """
        {
          "name": "vector-index",
          "fields": [
            { "name": "id", "type": "Edm.String", "key": true },
            { "name": "content_vector", "type": "Collection(Edm.Single)", "searchable": true, "retrievable": false, "dimensions": 3, "vectorSearchProfile": "cos" },
            { "name": "flat_vector", "type": "Collection(Edm.Single)", "searchable": true, "dimensions": 3, "vectorSearchProfile": "eknn" }
          ],
          "vectorSearch": {
            "algorithms": [
              { "name": "hnsw-1", "kind": "hnsw", "parameters": { "m": 4, "efConstruction": 40, "efSearch": 20 } },
              { "name": "eknn-1", "kind": "exhaustiveKnn", "parameters": {} }
            ],
            "profiles": [
              { "name": "cos", "algorithmConfigurationName": "hnsw-1" },
              { "name": "eknn", "algorithmConfigurationName": "eknn-1" }
            ]
          }
        }
        """);
        Assert.Equal(201, status);

        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["content_vector"] = new[] { 1.0f, 0.0f, 0.0f }, ["flat_vector"] = new[] { 1.0f, 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "2", ["content_vector"] = new[] { 0.0f, 1.0f, 0.0f }, ["flat_vector"] = new[] { 0.0f, 1.0f, 0.0f } },
        });
        var query = new VectorSearchOptions
        {
            Queries = { new VectorizedQuery(new float[] { 1.0f, 0.0f, 0.0f }) { KNearestNeighborsCount = 1, Fields = { "content_vector" } } },
        };
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { VectorSearch = query });
        Assert.Equal("1", found[0]["id"]);
        Assert.False(found[0].ContainsKey("content_vector"));
        Assert.True(found[0].ContainsKey("flat_vector"));

        var selected = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { VectorSearch = query, Select = { "id", "content_vector" } });
        Assert.Equal("1", selected[0]["id"]);
        Assert.True(selected[0].ContainsKey("content_vector"));
    }

    [Fact]
    public async Task VectorSearchWithOrderByOrdersByField()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SimpleField("price", SearchFieldDataType.Double) { IsSortable = true },
                new SearchField("content_vector", new SearchFieldDataType("Collection(Edm.Single)"))
                {
                    IsSearchable = true,
                    VectorSearchDimensions = 3,
                    VectorSearchProfileName = "cos",
                },
            },
            VectorSearch = new VectorSearch
            {
                Algorithms =
                {
                    new HnswAlgorithmConfiguration("hnsw-1")
                    {
                        Parameters = new HnswParameters { M = 4, EfConstruction = 40, EfSearch = 20 },
                    },
                },
                Profiles = { new VectorSearchProfile("cos", "hnsw-1") },
            },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["price"] = 5.0, ["content_vector"] = new[] { 1.0f, 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "2", ["price"] = 1.0, ["content_vector"] = new[] { 0.0f, 1.0f, 0.0f } },
        });
        // The vector query ranks doc 2 first, but orderby takes precedence.
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            OrderBy = { "price asc" },
            VectorSearch = new VectorSearchOptions
            {
                Queries = { new VectorizedQuery(new float[] { 0.0f, 1.0f, 0.0f }) { KNearestNeighborsCount = 2, Fields = { "content_vector" } } },
            },
        });
        Assert.Equal(new[] { "2", "1" }, found.Select(d => (string)d["id"]));
    }
}
