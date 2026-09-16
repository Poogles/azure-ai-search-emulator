using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.Models;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// Semantic search compatibility tests through the official Azure AI Search
/// .NET SDK. Mirrors the semantic additions in
/// source/tests/python/tests/sdk/test_sdk.py.
/// </summary>
public class SemanticSearchTests : EmulatorTestBase
{
    private const string IndexName = "csharp-semantic-index";

    public SemanticSearchTests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    /// <summary>A semantic index in the SDK wire format (prioritizedFields).</summary>
    private static SearchIndex SemanticIndex() => new(IndexName)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchableField("title"),
            new SearchableField("content"),
            new SearchableField("summary"),
            new SimpleField("price", SearchFieldDataType.Double) { IsFilterable = true, IsSortable = true },
        },
        SemanticSearch = new SemanticSearch
        {
            Configurations =
            {
                new SemanticConfiguration(
                    "default",
                    new SemanticPrioritizedFields
                    {
                        TitleField = new SemanticField("title"),
                        ContentFields = { new SemanticField("content") },
                    }),
            },
        },
    };

    private static SearchDocument[] SemanticDocs() =>
    [
        new()
        {
            ["id"] = "1",
            ["title"] = "Azure AI Search Overview",
            ["content"] = "Azure AI Search is a cloud service. It provides rich text search capabilities. It also supports vector search. The service is highly available.",
            ["summary"] = "An overview of Azure AI Search capabilities and features.",
            ["price"] = 10.0,
        },
        new()
        {
            ["id"] = "2",
            ["title"] = "Vector Search Guide",
            ["content"] = "Vector search uses embeddings. Neural networks create the embeddings. The search finds similar documents. This is useful for semantic matching.",
            ["summary"] = "A guide to vector search and embeddings.",
            ["price"] = 20.0,
        },
    ];

    private async Task<SearchClient> SetupAsync()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(SemanticIndex());
        var results = await searchClient.UploadDocumentsAsync(SemanticDocs());
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        return searchClient;
    }

    /// <summary>Run a semantic search and return the full result items (so
    /// <c>SemanticSearch.RerankerScore</c> / <c>Captions</c> are reachable).</summary>
    private static async Task<List<SearchResult<SearchDocument>>> RunSemanticAsync(
        SearchClient client, SearchOptions options, string query)
    {
        var response = await client.SearchAsync<SearchDocument>(query, options);
        var list = new List<SearchResult<SearchDocument>>();
        await foreach (var item in response.Value.GetResultsAsync())
        {
            list.Add(item);
        }
        return list;
    }

    [Fact]
    public async Task SemanticIndexCreation()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateIndexAsync(SemanticIndex())).Value;
        Assert.Equal(IndexName, created.Name);
        var fetched = (await indexClient.GetIndexAsync(IndexName)).Value;
        Assert.NotNull(fetched.SemanticSearch);
        Assert.Single(fetched.SemanticSearch!.Configurations);
        Assert.Equal("default", fetched.SemanticSearch.Configurations.First().Name);
    }

    [Fact]
    public async Task SemanticSearchFlatFormat()
    {
        var searchClient = await SetupAsync();
        var options = new SearchOptions
        {
            QueryType = SearchQueryType.Semantic,
            SemanticSearch = new SemanticSearchOptions
            {
                SemanticConfigurationName = "default",
                QueryAnswer = new QueryAnswer(QueryAnswerType.Extractive) { Count = 3 },
                QueryCaption = new QueryCaption(QueryCaptionType.Extractive),
                ErrorMode = SemanticErrorMode.Fail,
            },
        };
        var found = await RunSemanticAsync(searchClient, options, "vector search embeddings");
        Assert.NotEmpty(found);
        foreach (var item in found)
        {
            Assert.NotNull(item.SemanticSearch);
            Assert.NotNull(item.SemanticSearch!.RerankerScore);
            Assert.InRange(item.SemanticSearch.RerankerScore.Value, 0.0, 1.0);
        }
        Assert.Equal("2", found[0].Document["id"]);
        Assert.NotNull(found[0].SemanticSearch!.Captions);
        Assert.NotEmpty(found[0].SemanticSearch.Captions);
        Assert.False(string.IsNullOrEmpty(found[0].SemanticSearch.Captions.First().Text));
    }

    [Fact]
    public async Task SemanticSearchTopLevelAnswersRawHttp()
    {
        await SetupAsync();
        var url = $"{BaseUrl}/indexes('{IndexName}')/docs/search.post.search?api-version={ApiVersion}";
        var body = """
        {
          "search": "vector search",
          "semantic": {
            "semanticConfiguration": "default",
            "queryContext": { "questions": ["What is vector search used for?"] },
            "answers": { "count": 3, "type": "extractive" },
            "captions": { "count": 1, "type": "extractive", "answers": { "count": 1, "type": "extractive" } },
            "semanticErrorHandling": "returnPartialResults"
          }
        }
        """;
        var (status, raw) = await RawPostAsync(url, body);
        Assert.Equal(200, status);
        using var doc = System.Text.Json.JsonDocument.Parse(raw);
        var root = doc.RootElement;
        Assert.True(root.TryGetProperty("@search.answers", out var answers));
        Assert.Equal(System.Text.Json.JsonValueKind.Array, answers.ValueKind);
        Assert.True(answers.GetArrayLength() >= 1);
        var first = answers[0];
        Assert.False(string.IsNullOrEmpty(first.GetProperty("key").GetString()));
        Assert.False(string.IsNullOrEmpty(first.GetProperty("text").GetString()));
        Assert.InRange(first.GetProperty("score").GetDouble(), 0.0, 1.0);
        Assert.False(string.IsNullOrEmpty(first.GetProperty("highlights").GetString()));
        // Per-document captions with nested answers.
        var value = root.GetProperty("value");
        var doc2 = value.EnumerateArray().First(d => d.GetProperty("id").GetString() == "2");
        Assert.True(doc2.TryGetProperty("@search.captions", out var captions));
        Assert.Equal(System.Text.Json.JsonValueKind.Array, captions.ValueKind);
        Assert.True(captions.GetArrayLength() >= 1);
        Assert.False(string.IsNullOrEmpty(captions[0].GetProperty("text").GetString()));
        Assert.True(captions[0].TryGetProperty("answers", out var nested));
        Assert.Equal(System.Text.Json.JsonValueKind.Array, nested.ValueKind);
        Assert.True(nested.GetArrayLength() >= 1);
    }

    [Fact]
    public async Task SemanticSearchWithFilterOrderbySelect()
    {
        var searchClient = await SetupAsync();
        var options = new SearchOptions
        {
            QueryType = SearchQueryType.Semantic,
            SemanticSearch = new SemanticSearchOptions
            {
                SemanticConfigurationName = "default",
                QueryAnswer = new QueryAnswer(QueryAnswerType.Extractive),
            },
            Filter = "price gt 5",
            OrderBy = { "price asc" },
            Select = { "id", "title", "price" },
        };
        var found = await RunSemanticAsync(searchClient, options, "search");
        Assert.NotEmpty(found);
        Assert.Equal("1", found[0].Document["id"]);
        // select projection: content/summary omitted.
        Assert.False(found[0].Document.ContainsKey("content"));
        Assert.False(found[0].Document.ContainsKey("summary"));
        Assert.NotNull(found[0].SemanticSearch);
        Assert.NotNull(found[0].SemanticSearch!.RerankerScore);
    }

    [Fact]
    public async Task NonSemanticQueryOnSemanticIndexHasNoSemanticProps()
    {
        var searchClient = await SetupAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("vector search", new SearchOptions());
        var list = new List<SearchResult<SearchDocument>>();
        await foreach (var item in response.Value.GetResultsAsync())
        {
            list.Add(item);
        }
        Assert.NotEmpty(list);
        foreach (var item in list)
        {
            // The .NET SDK always instantiates SemanticSearch; a non-semantic
            // search leaves its fields null.
            Assert.Null(item.SemanticSearch?.RerankerScore);
            Assert.Null(item.SemanticSearch?.Captions);
        }
    }

    [Fact]
    public async Task SemanticErrorCases()
    {
        await SetupAsync();
        var url = $"{BaseUrl}/indexes('{IndexName}')/docs/search.post.search?api-version={ApiVersion}";

        // Unknown configuration name.
        var (status, body) = await RawPostAsync(
            url,
            """{"search": "search", "queryType": "semantic", "semanticConfiguration": "nonexistent"}""");
        Assert.Equal(400, status);
        AssertAzureError(body, "InvalidQuery");

        // queryType=semantic without a configuration.
        (status, body) = await RawPostAsync(url, """{"search": "search", "queryType": "semantic"}""");
        Assert.Equal(400, status);
        AssertAzureError(body, "InvalidQuery");

        // semantic + queryType=full.
        (status, body) = await RawPostAsync(
            url,
            """{"search": "search", "queryType": "full", "semantic": {"semanticConfiguration": "default"}}""");
        Assert.Equal(400, status);
        AssertAzureError(body, "InvalidQuery");

        // Bad answers.count (above the default max of 5).
        (status, body) = await RawPostAsync(
            url,
            """{"search": "search", "semantic": {"semanticConfiguration": "default", "answers": {"count": 6}}}""");
        Assert.Equal(400, status);
        AssertAzureError(body, "InvalidQuery");
    }
}
