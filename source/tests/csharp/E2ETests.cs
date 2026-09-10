using System.Text.Json;
using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.Models;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// End-to-end tests: the official Azure AI Search .NET SDK against the
/// containerised emulator. Mirrors source/tests/python/tests/e2e/test_emulator.py.
/// </summary>
public class E2ETests : EmulatorTestBase
{
    private const string IndexName = "e2e-index";

    public E2ETests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    private SearchIndex TestIndex() => new(IndexName)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchableField("title"),
        },
    };

    [Fact]
    public async Task HealthEndpoint()
    {
        var (status, body) = await RawGetAsync($"{BaseUrl}/health");
        Assert.Equal(200, status);
        Assert.Contains("ok", body);
    }

    [Fact]
    public async Task MissingApiKeyReturns401()
    {
        var (status, body) = await RawGetAsync($"{BaseUrl}/indexes?api-version={ApiVersion}", apiKey: null);
        Assert.Equal(401, status);
        AssertAzureError(body, "AuthenticationFailed");
    }

    [Fact]
    public async Task EmptyApiKeyReturns401()
    {
        var (status, body) = await RawGetAsync($"{BaseUrl}/indexes?api-version={ApiVersion}", apiKey: "");
        Assert.Equal(401, status);
        AssertAzureError(body, "AuthenticationFailed");
    }

    [Fact]
    public async Task MissingApiVersionReturns400()
    {
        var (status, body) = await RawGetAsync($"{BaseUrl}/indexes", apiKey: ApiKey);
        Assert.Equal(400, status);
        AssertAzureError(body, "ApiVersionMissing");
    }

    [Fact]
    public async Task UnsupportedApiVersionReturns400()
    {
        var (status, body) = await RawGetAsync($"{BaseUrl}/indexes?api-version=1900-01-01", apiKey: ApiKey);
        Assert.Equal(400, status);
        AssertAzureError(body, "ApiVersionUnsupported");
    }

    [Fact]
    public async Task ErrorBodyIsAzureStructured()
    {
        var (_, body) = await RawGetAsync($"{BaseUrl}/indexes?api-version={ApiVersion}", apiKey: null);
        using var doc = JsonDocument.Parse(body);
        var root = doc.RootElement;
        // Exactly one top-level member: "error".
        var top = root.EnumerateObject().ToArray();
        Assert.Single(top);
        Assert.Equal("error", top[0].Name);
        var errorNames = root.GetProperty("error").EnumerateObject().Select(p => p.Name).OrderBy(n => n).ToArray();
        Assert.Equal(new[] { "code", "message" }, errorNames);
    }

    [Fact]
    public async Task SearchFacetCount()
    {
        // The bare-number "$count" facet is exercised over raw HTTP because the
        // SDK types facets as IDictionary<string, IList<FacetResult>> and cannot
        // deserialize a bare number (the whole response falls back to raw JSON).
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "alpha" },
            new SearchDocument { ["id"] = "2", ["title"] = "beta" },
            new SearchDocument { ["id"] = "3", ["title"] = "gamma" },
        });
        var url = $"{BaseUrl}/indexes('{IndexName}')/docs/search.post.search?api-version={ApiVersion}";
        var (status, body) = await RawPostAsync(url, """{"search": "*", "facets": ["$count"]}""");
        Assert.Equal(200, status);
        using var doc = JsonDocument.Parse(body);
        Assert.Equal(3, doc.RootElement.GetProperty("@search.facets").GetProperty("$count").GetInt32());
    }

    [Fact]
    public async Task CreateIndex()
    {
        var created = (await IndexClient().CreateIndexAsync(TestIndex())).Value;
        Assert.Equal(IndexName, created.Name);
        Assert.Equal(2, created.Fields.Count);
    }

    [Fact]
    public async Task CreateDuplicateIndexFails()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestIndex());
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.CreateIndexAsync(TestIndex()));
        Assert.Equal(409, ex.Status);
    }

    [Fact]
    public async Task ListIndexes()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestIndex());
        var names = new List<string>();
        await foreach (var index in indexClient.GetIndexesAsync())
        {
            names.Add(index.Name);
        }
        Assert.Equal(new[] { IndexName }, names);
    }

    [Fact]
    public async Task UpdateIndex()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestIndex());
        var updated = TestIndex();
        updated.Fields[1].IsSearchable = false;
        await indexClient.CreateOrUpdateIndexAsync(updated);
        var fetched = (await indexClient.GetIndexAsync(IndexName)).Value;
        Assert.False(fetched.Fields[1].IsSearchable);
    }

    [Fact]
    public async Task DeleteIndex()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestIndex());
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetIndexAsync(IndexName));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task UploadAndSearch()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "hello world" },
            new SearchDocument { ["id"] = "2", ["title"] = "foo bar" },
            new SearchDocument { ["id"] = "3", ["title"] = "hello there" },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));

        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Size = 10 }, "hello");
        Assert.Equal(2, found.Count);
        Assert.Equal(new[] { "1", "3" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
    }

    [Fact]
    public async Task SearchMatchAll()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "alpha" },
            new SearchDocument { ["id"] = "2", ["title"] = "beta" },
        });
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Size = 10 }, "*");
        Assert.Equal(2, found.Count);
    }

    [Fact]
    public async Task OperationsOnDeletedIndexFail()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestIndex());
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(
            async () => await RunSearch<SearchDocument>(searchClient, new SearchOptions { Size = 1 }, "test"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task RagStyleVectorAndHybridFlow()
    {
        // RAG-style flow: text + vector fields, embedding upload, vector and
        // hybrid retrieval with ordering (never exact scores).
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("title"),
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
                        Parameters = new HnswParameters { Metric = "cosine" },
                    },
                },
                Profiles = { new VectorSearchProfile("cos", "hnsw-1") },
            },
        });
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure search", ["content_vector"] = new[] { 1.0f, 0.0f, 0.0f } },
            new SearchDocument { ["id"] = "2", ["title"] = "azure emulators", ["content_vector"] = new[] { 0.0f, 1.0f, 0.0f } },
            new SearchDocument { ["id"] = "3", ["title"] = "unrelated", ["content_vector"] = new[] { 0.0f, 0.0f, 1.0f } },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));

        // Vector-only retrieval: nearest first.
        var vector = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries =
                {
                    new VectorizedQuery(new float[] { 1.0f, 0.0f, 0.0f })
                    {
                        KNearestNeighborsCount = 2,
                        Fields = { "content_vector" },
                    },
                },
            },
        });
        Assert.Equal(new[] { "1", "2" }, vector.Select(d => (string)d["id"]));

        // Hybrid retrieval: union of the full-text ("azure" -> 1, 2) and vector
        // ([0,0,1] -> 3) sides.
        var hybrid = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries =
                {
                    new VectorizedQuery(new float[] { 0.0f, 0.0f, 1.0f })
                    {
                        KNearestNeighborsCount = 1,
                        Fields = { "content_vector" },
                    },
                },
            },
        }, "azure");
        Assert.Equal(new[] { "1", "2", "3" }, hybrid.Select(d => (string)d["id"]).OrderBy(x => x));
    }
}
