using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.Models;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// Vectorizer query compatibility tests (Phase 2.4). Mirrors
/// source/tests/python/tests/sdk/test_vectorizer.py. The pinned .NET SDK
/// associates a vectorizer with a field through the profile's
/// <c>VectorizerName</c> and nests <c>Vectorizers</c> inside
/// <c>VectorSearch</c>; <c>kind: "text"</c> queries are
/// <see cref="VectorizableTextQuery"/>.
/// </summary>
public class VectorizerSearchTests : EmulatorTestBase
{
    private const string IndexName = "vectorizer-index";
    private const int Dimensions = 64;

    public VectorizerSearchTests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    private static SearchIndex VectorizerIndex(VectorSearchVectorizer? vectorizer = null) => new(IndexName)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchableField("title") { IsFilterable = true },
            new SearchableField("content"),
            new SimpleField("category", SearchFieldDataType.String) { IsFilterable = true },
            new SearchField("content_vector", new SearchFieldDataType("Collection(Edm.Single)"))
            {
                IsSearchable = true,
                VectorSearchDimensions = Dimensions,
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
            },
            // The vectorizer is associated with the field through the profile.
            Profiles =
            {
                new VectorSearchProfile("cos", "hnsw-1") { VectorizerName = "embedder" },
            },
            Vectorizers =
            {
                vectorizer ?? new AzureOpenAIVectorizer("embedder")
                {
                    Parameters = new AzureOpenAIVectorizerParameters
                    {
                        ResourceUri = new Uri("https://example-resource.openai.azure.com"),
                        DeploymentName = "text-embedding-ada-002",
                    },
                },
            },
        },
    };

    private static VectorizableTextQuery TextQuery(string text, int k) => new(text)
    {
        KNearestNeighborsCount = k,
        Fields = { "content_vector" },
    };

    private async Task<SearchClient> VectorizerDocsAsync()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(VectorizerIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "quantum computing", ["content"] = "quantum computing applications", ["category"] = "tech" },
            new SearchDocument { ["id"] = "2", ["title"] = "classical physics", ["content"] = "classical mechanics", ["category"] = "tech" },
            new SearchDocument { ["id"] = "3", ["title"] = "quantum mechanics", ["content"] = "quantum physics", ["category"] = "misc" },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        return searchClient;
    }

    [Fact]
    public async Task CreateIndexWithVectorizer()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateIndexAsync(VectorizerIndex())).Value;
        Assert.Equal(IndexName, created.Name);
        Assert.NotNull(created.VectorSearch?.Vectorizers);
        Assert.Equal("embedder", created.VectorSearch.Vectorizers[0].VectorizerName);
        var fetched = (await indexClient.GetIndexAsync(IndexName)).Value;
        Assert.NotNull(fetched.VectorSearch?.Vectorizers);
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
    }

    [Fact]
    public async Task WebApiVectorizerAccepted()
    {
        var indexClient = IndexClient();
        var index = VectorizerIndex(
            new WebApiVectorizer("embedder")
            {
                Parameters = new WebApiVectorizerParameters { Uri = new Uri("https://example.com/embed") },
            });
        var created = (await indexClient.CreateIndexAsync(index)).Value;
        Assert.NotNull(created.VectorSearch?.Vectorizers);
        Assert.Equal("embedder", created.VectorSearch.Vectorizers[0].VectorizerName);
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
    }

    [Fact]
    public async Task UploadWithoutVectorGeneratesVector()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(VectorizerIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "quantum computing", ["content"] = "applications" },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        var doc = await searchClient.GetDocumentAsync<SearchDocument>("1");
        var vector = ((System.Collections.IEnumerable)doc.Value["content_vector"])
            .Cast<object>()
            .Select(v => Convert.ToSingle(v))
            .ToArray();
        Assert.Equal(Dimensions, vector.Length);
        Assert.Contains(vector, v => v != 0.0f);
    }

    [Fact]
    public async Task TextQueryOrdersByTokenOverlap()
    {
        var searchClient = await VectorizerDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { TextQuery("quantum computing", 3) } },
        });
        var ids = found.Select(d => (string)d["id"]).ToList();
        // Doc 1 shares both query tokens; doc 3 shares "quantum"; doc 2 shares none.
        Assert.Equal("1", ids[0]);
        Assert.True(ids.IndexOf("3") < ids.IndexOf("2"));
    }

    [Fact]
    public async Task TextQueryRespectsK()
    {
        var searchClient = await VectorizerDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { TextQuery("quantum computing", 2) } },
        });
        Assert.Equal(2, found.Count);
    }

    [Fact]
    public async Task TextQueryHybridWithSearchText()
    {
        var searchClient = await VectorizerDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions { Queries = { TextQuery("quantum computing", 3) } },
        }, "quantum");
        var ids = found.Select(d => (string)d["id"]).ToList();
        // Doc 1 matches both the full-text and vectorizer sides, so it ranks first.
        Assert.Equal("1", ids[0]);
        Assert.Contains("3", ids);
    }

    [Fact]
    public async Task TextQueryPreFilter()
    {
        var searchClient = await VectorizerDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            Filter = "category eq 'misc'",
            VectorSearch = new VectorSearchOptions
            {
                Queries = { TextQuery("quantum computing", 3) },
                FilterMode = VectorFilterMode.PreFilter,
            },
        });
        // Only the misc doc (3) is a candidate.
        Assert.Equal(new[] { "3" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task MultipleTextQueriesUnion()
    {
        var searchClient = await VectorizerDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries = { TextQuery("quantum computing", 1), TextQuery("classical physics", 1) },
            },
        });
        var ids = found.Select(d => (string)d["id"]).ToList();
        // The union contains the top match of each query.
        Assert.Contains("1", ids);
        Assert.Contains("2", ids);
    }

    [Fact]
    public async Task MixedTextAndVectorQueries()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        var index = VectorizerIndex();
        // Add a vectorizer-free profile and a raw-vector field for the vector query.
        index.VectorSearch.Profiles.Add(new VectorSearchProfile("no_vz", "hnsw-1"));
        index.Fields.Add(new SearchField("raw_vector", new SearchFieldDataType("Collection(Edm.Single)"))
        {
            IsSearchable = true,
            VectorSearchDimensions = Dimensions,
            VectorSearchProfileName = "no_vz",
        });
        await indexClient.CreateIndexAsync(index);
        var raw = new float[Dimensions];
        raw[1] = 1.0f;
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "quantum computing", ["content"] = "applications" },
            new SearchDocument { ["id"] = "2", ["title"] = "unrelated", ["content"] = "nothing", ["raw_vector"] = raw },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions
        {
            VectorSearch = new VectorSearchOptions
            {
                Queries =
                {
                    TextQuery("quantum computing", 1),
                    new VectorizedQuery(raw) { KNearestNeighborsCount = 1, Fields = { "raw_vector" } },
                },
            },
        });
        var ids = found.Select(d => (string)d["id"]).ToList();
        Assert.Contains("1", ids);
        Assert.Contains("2", ids);
    }

    [Fact]
    public async Task TextQueryRejectsFieldWithoutVectorizer()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        var index = VectorizerIndex();
        index.VectorSearch.Profiles.Add(new VectorSearchProfile("no_vz", "hnsw-1"));
        index.Fields.Add(new SearchField("raw_vector", new SearchFieldDataType("Collection(Edm.Single)"))
        {
            IsSearchable = true,
            VectorSearchDimensions = Dimensions,
            VectorSearchProfileName = "no_vz",
        });
        await indexClient.CreateIndexAsync(index);
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "hello", ["content"] = "world" },
        });
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () =>
        {
            await RunSearch<SearchDocument>(searchClient, new SearchOptions
            {
                VectorSearch = new VectorSearchOptions
                {
                    Queries = { new VectorizableTextQuery("hello") { KNearestNeighborsCount = 1, Fields = { "raw_vector" } } },
                },
            });
        });
        Assert.Contains("vectorizer", ex.Message, StringComparison.OrdinalIgnoreCase);
    }
}
