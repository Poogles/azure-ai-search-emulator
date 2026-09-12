using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.KnowledgeBases;
using Azure.Search.Documents.KnowledgeBases.Models;
using Azure.Search.Documents.Models;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// SDK compatibility tests: every supported operation through the official
/// Azure AI Search .NET SDK. Mirrors source/tests/python/tests/sdk/test_sdk.py.
/// </summary>
public class SdkTests : EmulatorTestBase
{
    private const string IndexName = TestData.IndexName;

    public SdkTests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    /// <summary>Create the standard index and upload the priced documents,
    /// returning a ready search client (the .NET <c>priced_docs</c> fixture).</summary>
    private async Task<SearchClient> PricedDocsAsync()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var results = await searchClient.UploadDocumentsAsync(TestData.PricedDocs());
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        return searchClient;
    }

    [Fact]
    public async Task IndexCrud()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateIndexAsync(TestData.FullIndex())).Value;
        Assert.Equal(IndexName, created.Name);
        var fetched = (await indexClient.GetIndexAsync(IndexName)).Value;
        Assert.Equal(IndexName, fetched.Name);
        var names = new List<string>();
        await foreach (var index in indexClient.GetIndexesAsync())
        {
            names.Add(index.Name);
        }
        Assert.Equal(new[] { IndexName }, names);
        await indexClient.DeleteIndexAsync(IndexName, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetIndexAsync(IndexName));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task UploadMergeDelete()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one", ["price"] = 1.0, ["tags"] = new[] { "a" } },
            new SearchDocument { ["id"] = "2", ["title"] = "two", ["price"] = 2.0, ["tags"] = new[] { "b" } },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));

        var merged = await searchClient.MergeDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["price"] = 9.0 },
        });
        Assert.All(merged.Value.Results, r => Assert.True(r.Succeeded));

        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "one");
        Assert.Single(found);
        Assert.Equal(9.0, Convert.ToDouble(found[0]["price"]));
        Assert.Equal("one", found[0]["title"]);

        var deleted = await searchClient.DeleteDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1" },
        });
        Assert.All(deleted.Value.Results, r => Assert.True(r.Succeeded));
        Assert.Single(await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "*"));
    }

    [Fact]
    public async Task MergeOrUpload()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one", ["price"] = 1.0 },
        });
        var results = await searchClient.MergeOrUploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["price"] = 5.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "two", ["price"] = 3.0 },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        var found = (await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "*"))
            .ToDictionary(d => (string)d["id"]);
        Assert.Equal(5.0, Convert.ToDouble(found["1"]["price"]));
        Assert.Equal("one", found["1"]["title"]);
        Assert.Equal("two", found["2"]["title"]);
    }

    [Fact]
    public async Task SearchFilter()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "price ge 50" }, "*");
        Assert.Equal(new[] { "2", "3" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "price lt 10 or price gt 100" }, "*");
        Assert.Equal(new[] { "1", "3" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "price gt 10" }, "mid");
        Assert.Equal(new[] { "2" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchOrderBy()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { OrderBy = { "price desc" } }, "*");
        Assert.Equal(new[] { "3", "2", "1" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { OrderBy = { "price asc" } }, "*");
        Assert.Equal(new[] { "1", "2", "3" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchSelect()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Select = { "id", "price" } }, "*");
        Assert.Equal(3, found.Count);
        foreach (var doc in found)
        {
            Assert.True(doc.ContainsKey("id"));
            Assert.True(doc.ContainsKey("price"));
            Assert.False(doc.ContainsKey("title"));
            Assert.False(doc.ContainsKey("tags"));
        }
    }

    [Fact]
    public async Task SearchFacets()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { Facets = { "tags" } });
        var facets = response.Value.Facets;
        Assert.NotNull(facets);
        var values = facets["tags"].ToDictionary(f => (string)f.Value, f => f.Count!.Value);
        Assert.Equal(new Dictionary<string, long> { ["red"] = 2, ["blue"] = 1, ["green"] = 1 }, values);
    }

    [Fact]
    public async Task SearchFields()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { SearchFields = { "title" } }, "red");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchPaging()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { Size = 2 });
        var pages = new List<Page<SearchResult<SearchDocument>>>();
        await foreach (var page in response.Value.GetResultsAsync().AsPages())
        {
            pages.Add(page);
        }
        Assert.Equal(2, pages.Count);
        Assert.Equal(new[] { "1", "2" }, pages[0].Values.Select(v => (string)v.Document["id"]));
        Assert.Equal(new[] { "3" }, pages[1].Values.Select(v => (string)v.Document["id"]));
    }

    [Fact]
    public async Task ContinuationTokenSurvivesMutation()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "doc 1", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "doc 2", ["price"] = 2.0 },
            new SearchDocument { ["id"] = "3", ["title"] = "doc 3", ["price"] = 3.0 },
            new SearchDocument { ["id"] = "4", ["title"] = "doc 4", ["price"] = 4.0 },
            new SearchDocument { ["id"] = "5", ["title"] = "doc 5", ["price"] = 5.0 },
        });

        // Raw HTTP so the opaque `continuation` token is preserved (the SDK
        // pages via `skip` and drops it).
        var url = $"{BaseUrl}/indexes('{IndexName}')/docs/search.post.search?api-version={ApiVersion}";
        var (status, body) = await RawPostAsync(url, """{"search": "*", "top": 2}""");
        Assert.Equal(200, status);
        using (var doc = System.Text.Json.JsonDocument.Parse(body))
        {
            var ids = doc.RootElement.GetProperty("value")
                .EnumerateArray().Select(e => e.GetProperty("id").GetString()).ToArray();
            Assert.Equal(new[] { "1", "2" }, ids);
            var nextParams = doc.RootElement.GetProperty("@search.nextPageParameters").GetRawText();
            Assert.Contains("continuation", nextParams);

            // A document mutation does NOT invalidate the outstanding token;
            // the new doc sorts last, so skip=2 still resumes at "3".
            await searchClient.UploadDocumentsAsync(new[]
            {
                new SearchDocument { ["id"] = "6", ["title"] = "doc 6", ["price"] = 6.0 },
            });

            var (status2, body2) = await RawPostAsync(url, nextParams);
            Assert.Equal(200, status2);
            using var doc2 = System.Text.Json.JsonDocument.Parse(body2);
            var ids2 = doc2.RootElement.GetProperty("value")
                .EnumerateArray().Select(e => e.GetProperty("id").GetString()).ToArray();
            Assert.Equal(new[] { "3", "4" }, ids2);
        }
    }

    [Fact]
    public async Task SearchCount()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { IncludeTotalCount = true });
        Assert.Equal(3, response.Value.TotalCount);
    }

    [Fact]
    public async Task GetDocument()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one", ["price"] = 1.0 },
        });
        var doc = (await searchClient.GetDocumentAsync<SearchDocument>("1")).Value;
        Assert.Equal("1", doc["id"]);
        Assert.Equal("one", doc["title"]);
        Assert.Equal(1.0, Convert.ToDouble(doc["price"]));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(
            () => searchClient.GetDocumentAsync<SearchDocument>("missing"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task SearchFacetOptions()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { Facets = { "tags,count:1" } });
        var facets = response.Value.Facets;
        Assert.NotNull(facets);
        Assert.Single(facets["tags"]);
        Assert.Equal("red", facets["tags"][0].Value);
        Assert.Equal(2, facets["tags"][0].Count);
    }

    [Fact]
    public async Task GeographyPointUpload()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchField("location", SearchFieldDataType.GeographyPoint),
            },
        });
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument
            {
                ["id"] = "1",
                ["location"] = new Dictionary<string, object> { ["type"] = "Point", ["coordinates"] = new[] { -122.13, 47.67 } },
            },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        var doc = (await searchClient.GetDocumentAsync<SearchDocument>("1")).Value;
        // Geography points deserialize as GeoPoint (not Dictionary) in the .NET SDK.
        var location = (Azure.Core.GeoJson.GeoPoint)doc["location"];
        Assert.Equal(-122.13, location.Coordinates.Longitude);
        Assert.Equal(47.67, location.Coordinates.Latitude);
    }

    [Fact]
    public async Task ComplexTypeFilter()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("name"),
                new SearchField("address", SearchFieldDataType.Complex)
                {
                    Fields =
                    {
                        new SearchField("city", SearchFieldDataType.String) { IsFilterable = true },
                        new SearchField("state", SearchFieldDataType.String) { IsFilterable = true },
                    },
                },
            },
        });
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument
            {
                ["id"] = "1",
                ["name"] = "one",
                ["address"] = new Dictionary<string, object> { ["city"] = "Miami", ["state"] = "FL" },
            },
            new SearchDocument
            {
                ["id"] = "2",
                ["name"] = "two",
                ["address"] = new Dictionary<string, object> { ["city"] = "Seattle", ["state"] = "WA" },
            },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "address/state eq 'FL'" }, "*");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task CollectionOfComplexType()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("name"),
                new SearchField("address", SearchFieldDataType.Collection(SearchFieldDataType.Complex))
                {
                    Fields =
                    {
                        new SearchableField("city"),
                        new SearchField("state", SearchFieldDataType.String),
                    },
                },
            },
        });
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument
            {
                ["id"] = "1",
                ["name"] = "contoso",
                ["address"] = new[]
                {
                    new Dictionary<string, object> { ["city"] = "Miami", ["state"] = "FL" },
                    new Dictionary<string, object> { ["city"] = "Seattle", ["state"] = "WA" },
                },
            },
            new SearchDocument
            {
                ["id"] = "2",
                ["name"] = "fabrikam",
                ["address"] = new[]
                {
                    new Dictionary<string, object> { ["city"] = "Montreal", ["state"] = "QC" },
                },
            },
        });
        Assert.All(results.Value.Results, r => Assert.True(r.Succeeded));

        // A searchable subfield is indexed across all collection elements.
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "Seattle");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task CountDocuments()
    {
        var searchClient = await PricedDocsAsync();
        Assert.Equal(3, (await searchClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task CountDocumentsEmpty()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        Assert.Equal(0, (await searchClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task CountDocumentsMissingIndex()
    {
        var searchClient = SearchClient("missing");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => searchClient.GetDocumentCountAsync());
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task ServiceStatistics()
    {
        var indexClient = IndexClient();
        var stats = (await indexClient.GetServiceStatisticsAsync()).Value;
        Assert.NotNull(stats.Counters);
        Assert.NotNull(stats.Limits);
    }

    [Fact]
    public async Task AnalyzeText()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var tokens = (await indexClient.AnalyzeTextAsync(IndexName, new AnalyzeTextOptions("Hello, World!"))).Value
            .Select(t => t.Token).ToList();
        Assert.Contains("hello", tokens);
        Assert.Contains("world", tokens);
    }

    [Fact]
    public async Task SearchBooleanOperators()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("title"),
            },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure search" },
            new SearchDocument { ["id"] = "2", ["title"] = "azure emulators" },
            new SearchDocument { ["id"] = "3", ["title"] = "other" },
            new SearchDocument { ["id"] = "4", ["title"] = "quick brown fox" },
            new SearchDocument { ["id"] = "5", ["title"] = "brown quick" },
        });

        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "azure -emulators");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "-azure");
        Assert.Equal(new[] { "3", "4", "5" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "\"quick brown\"");
        Assert.Equal(new[] { "4" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchEmptyTextMatchesAll()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "");
        Assert.Equal(3, found.Count);
    }

    [Fact]
    public async Task SearchCollectionAnyAll()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "tags any t eq 'red'" }, "*");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "tags/any(t: t eq 'red')" }, "*");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Filter = "tags/all(t: t ne 'green')" }, "*");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
    }

    [Fact]
    public async Task SearchFacetTopOption()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { Facets = { "tags,top:1" } });
        var facets = response.Value.Facets;
        Assert.NotNull(facets);
        Assert.Single(facets["tags"]);
        Assert.Equal("red", facets["tags"][0].Value);
        Assert.Equal(2, facets["tags"][0].Count);
    }

    [Fact]
    public async Task SearchFacetStarExpandsAllFacetable()
    {
        var searchClient = await PricedDocsAsync();
        var response = await searchClient.SearchAsync<SearchDocument>("*", new SearchOptions { Facets = { "*" } });
        var facets = response.Value.Facets;
        Assert.NotNull(facets);
        Assert.True(facets.ContainsKey("tags"));
    }

    [Fact]
    public async Task SearchOrderByMultipleFields()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SimpleField("price", SearchFieldDataType.Double) { IsSortable = true },
                new SimpleField("rating", SearchFieldDataType.Int32) { IsSortable = true },
            },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["price"] = 5.0, ["rating"] = 2 },
            new SearchDocument { ["id"] = "2", ["price"] = 5.0, ["rating"] = 1 },
            new SearchDocument { ["id"] = "3", ["price"] = 1.0, ["rating"] = 9 },
        });
        var found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { OrderBy = { "price asc", "rating desc" } }, "*");
        Assert.Equal(new[] { "3", "1", "2" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task CreateOrUpdateIndexPreservesDocumentsWhenCompatible()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one" },
        });
        Assert.Equal(1, (await searchClient.GetDocumentCountAsync()).Value);
        // Re-PUTting the same (compatible) schema updates in place and keeps
        // the documents.
        await indexClient.CreateOrUpdateIndexAsync(TestData.FullIndex());
        Assert.Equal(1, (await searchClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task CreateOrUpdateIndexDiscardsDocumentsWhenIncompatible()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one" },
        });
        Assert.Equal(1, (await searchClient.GetDocumentCountAsync()).Value);
        // Dropping the searchable `title` field is incompatible: the index is
        // replaced and its documents discarded.
        var reduced = new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SimpleField("price", SearchFieldDataType.Double) { IsFilterable = true, IsSortable = true },
            },
        };
        await indexClient.CreateOrUpdateIndexAsync(reduced);
        Assert.Equal(0, (await searchClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task MergeMissingDocumentReports404()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var results = await searchClient.MergeDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "missing", ["price"] = 1.0 },
        });
        Assert.False(results.Value.Results[0].Succeeded);
        Assert.Equal(404, results.Value.Results[0].Status);
    }

    [Fact]
    public async Task DeleteMissingDocumentReports404()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var results = await searchClient.DeleteDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "missing" },
        });
        Assert.False(results.Value.Results[0].Succeeded);
        Assert.Equal(404, results.Value.Results[0].Status);
    }

    [Fact]
    public async Task UploadInvalidDocumentReportsPerDocumentError()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var results = await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "two", ["price"] = "not-a-number" },
        });
        Assert.True(results.Value.Results[0].Succeeded);
        Assert.False(results.Value.Results[1].Succeeded);
        Assert.Equal(400, results.Value.Results[1].Status);
        // The valid document in the same batch is still indexed.
        Assert.Equal(1, (await searchClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task UploadToMissingIndexReturns404()
    {
        var searchClient = SearchClient("missing");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(
            () => searchClient.UploadDocumentsAsync(new[] { new SearchDocument { ["id"] = "1" } }));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task UnsupportedQueryOptionsRejected()
    {
        var searchClient = await PricedDocsAsync();
        var cases = new[]
        {
            new SearchOptions { ScoringProfile = "profile" },
            new SearchOptions { SemanticSearch = new SemanticSearchOptions { SemanticConfigurationName = "config" } },
            new SearchOptions { QueryType = SearchQueryType.Full },
        };
        foreach (var options in cases)
        {
            var ex = await Assert.ThrowsAsync<RequestFailedException>(
                async () => await RunSearch<SearchDocument>(searchClient, options, "*"));
            Assert.Equal(400, ex.Status);
            Assert.Equal("UnsupportedQuery", ex.ErrorCode);
        }

        // An invalid searchMode is a malformed query, not an unsupported option.
        // (The SDK's SearchMode type cannot express invalid values, so this goes
        // over raw HTTP.)
        var (badStatus, badBody) = await RawPostAsync(
            $"{BaseUrl}/indexes('{IndexName}')/docs/search.post.search?api-version={ApiVersion}",
            """{"search": "*", "searchMode": "exact"}""");
        Assert.Equal(400, badStatus);
        AssertAzureError(badBody, "InvalidQuery");
    }

    [Fact]
    public async Task ErrorBodyIsAzureStructured()
    {
        var indexClient = IndexClient();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetIndexAsync("missing"));
        Assert.Equal(404, ex.Status);
        Assert.Equal("ResourceNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task SuggestAndAutocomplete()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("title"),
            },
            Suggesters = { new SearchSuggester("sg", new[] { "title" }) },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "Boston Harbor Hotel" },
            new SearchDocument { ["id"] = "2", ["title"] = "Portland Airport Inn" },
        });

        var suggestions = (await searchClient.SuggestAsync<SearchDocument>("bos", "sg", new SuggestOptions())).Value;
        Assert.Equal(new[] { "1" }, suggestions.Results.Select(d => (string)d.Document["id"]));
        Assert.Equal("Boston", suggestions.Results[0].Text);

        var completions = (await searchClient.AutocompleteAsync("bos", "sg", new AutocompleteOptions())).Value;
        Assert.Equal(new[] { "Boston" }, completions.Results.Select(c => c.Text));
        Assert.Equal("bos Boston", completions.Results[0].QueryPlusText);
    }

    [Fact]
    public async Task SuggestAndAutocompleteFilter()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("title"),
                new SearchField("category", SearchFieldDataType.String) { IsFilterable = true },
            },
            Suggesters = { new SearchSuggester("sg", new[] { "title" }) },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "Boston Harbor Hotel", ["category"] = "hotel" },
            new SearchDocument { ["id"] = "2", ["title"] = "Boston Airport Inn", ["category"] = "inn" },
            new SearchDocument { ["id"] = "3", ["title"] = "Portland Harbor Hotel", ["category"] = "hotel" },
        });

        var allSuggestions = (await searchClient.SuggestAsync<SearchDocument>("bos", "sg", new SuggestOptions())).Value;
        Assert.Equal(new[] { "1", "2" }, allSuggestions.Results.Select(d => (string)d.Document["id"]));

        var filteredSuggestions = (await searchClient.SuggestAsync<SearchDocument>("bos", "sg", new SuggestOptions { Filter = "category eq 'hotel'" })).Value;
        Assert.Equal(new[] { "1" }, filteredSuggestions.Results.Select(d => (string)d.Document["id"]));
        Assert.Equal("Boston", filteredSuggestions.Results[0].Text);

        var filteredCompletions = (await searchClient.AutocompleteAsync("bos", "sg", new AutocompleteOptions { Filter = "category eq 'hotel'" })).Value;
        Assert.Equal(new[] { "Boston" }, filteredCompletions.Results.Select(c => c.Text));
        Assert.Equal("bos Boston", filteredCompletions.Results[0].QueryPlusText);
    }

    [Fact]
    public async Task SynonymMapCrud()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateSynonymMapAsync(new SynonymMap("sm", new[] { "a", "b" }))).Value;
        Assert.Equal("sm", created.Name);
        var fetched = (await indexClient.GetSynonymMapAsync("sm")).Value;
        Assert.Equal(new[] { "a", "b" }, fetched.SynonymsList);
        var names = (await indexClient.GetSynonymMapsAsync()).Value.Select(map => map.Name).ToList();
        Assert.Equal(new[] { "sm" }, names);
        var updated = (await indexClient.CreateOrUpdateSynonymMapAsync(new SynonymMap("sm", new[] { "a", "c" }))).Value;
        Assert.Equal(new[] { "a", "c" }, updated.SynonymsList);
        await indexClient.DeleteSynonymMapAsync("sm", CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetSynonymMapAsync("sm"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task AliasCrud()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateAliasAsync(new SearchAlias("al", new[] { "i1" }))).Value;
        Assert.Equal("al", created.Name);
        var fetched = (await indexClient.GetAliasAsync("al")).Value;
        Assert.Equal(new[] { "i1" }, fetched.Indexes);
        var names = new List<string>();
        await foreach (var alias in indexClient.GetAliasesAsync())
        {
            names.Add(alias.Name);
        }
        Assert.Equal(new[] { "al" }, names);
        var updated = (await indexClient.CreateOrUpdateAliasAsync(new SearchAlias("al", new[] { "i2" }))).Value;
        Assert.Equal(new[] { "i2" }, updated.Indexes);
        await indexClient.DeleteAliasAsync("al", null, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetAliasAsync("al"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task KnowledgeSourceCrud()
    {
        var indexClient = IndexClient();
        var created = (await indexClient.CreateKnowledgeSourceAsync(new SearchIndexKnowledgeSource(
            "src1", new SearchIndexKnowledgeSourceParameters("i1")))).Value;
        Assert.Equal("src1", created.Name);
        var fetched = (await indexClient.GetKnowledgeSourceAsync("src1")).Value;
        Assert.Equal("src1", fetched.Name);
        var names = new List<string>();
        await foreach (var source in indexClient.GetKnowledgeSourcesAsync())
        {
            names.Add(source.Name);
        }
        Assert.Equal(new[] { "src1" }, names);
        var updated = (await indexClient.CreateOrUpdateKnowledgeSourceAsync(new SearchIndexKnowledgeSource(
            "src1", new SearchIndexKnowledgeSourceParameters("i1"))
        {
            Description = "updated",
        })).Value;
        Assert.Equal("updated", updated.Description);
        await indexClient.DeleteKnowledgeSourceAsync("src1", null, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetKnowledgeSourceAsync("src1"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task KnowledgeBaseCrud()
    {
        var indexClient = IndexClient();
        await indexClient.CreateKnowledgeSourceAsync(new SearchIndexKnowledgeSource(
            "src1", new SearchIndexKnowledgeSourceParameters("i1")));
        var created = (await indexClient.CreateKnowledgeBaseAsync(new KnowledgeBase(
            "kb1", new[] { new KnowledgeSourceReference("src1") }))).Value;
        Assert.Equal("kb1", created.Name);
        var fetched = (await indexClient.GetKnowledgeBaseAsync("kb1")).Value;
        Assert.Equal("src1", fetched.KnowledgeSources[0].Name);
        var names = new List<string>();
        await foreach (var base_ in indexClient.GetKnowledgeBasesAsync())
        {
            names.Add(base_.Name);
        }
        Assert.Equal(new[] { "kb1" }, names);
        await indexClient.DeleteKnowledgeBaseAsync("kb1", null, CancellationToken.None);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(() => indexClient.GetKnowledgeBaseAsync("kb1"));
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task KnowledgeBaseRetrieveReturnsEmpty()
    {
        var indexClient = IndexClient();
        await indexClient.CreateKnowledgeSourceAsync(new SearchIndexKnowledgeSource(
            "src1", new SearchIndexKnowledgeSourceParameters("i1")));
        await indexClient.CreateKnowledgeBaseAsync(new KnowledgeBase(
            "kb1", new[] { new KnowledgeSourceReference("src1") }));

        var client = new KnowledgeBaseRetrievalClient(
            new Uri(SdkBaseUrl), "kb1", new AzureKeyCredential(ApiKey), Options);
        var response = (await client.RetrieveAsync(new KnowledgeBaseRetrievalRequest
        {
            Intents = { new KnowledgeRetrievalSemanticIntent("hotels") },
        })).Value;
        Assert.Empty(response.Response);
        Assert.Empty(response.Activity);
        Assert.Empty(response.References);
    }
    [Fact]
    public async Task SearchModeAnyMatchesUnion()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure search", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "local emulators", ["price"] = 2.0 },
            new SearchDocument { ["id"] = "3", ["title"] = "unrelated", ["price"] = 3.0 },
        });

        // Default (any/OR, matching Azure): either term matches.
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "azure emulators");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        // Explicit all: AND semantics — no document contains both terms.
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { SearchMode = SearchMode.All }, "azure emulators");
        Assert.Empty(found);
        // Explicit any (OR): either term matches.
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { SearchMode = SearchMode.Any }, "azure emulators");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
    }

    [Fact]
    public async Task SearchStemmingAndStopwords()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "running shoes", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "other things", ["price"] = 2.0 },
        });

        foreach (var term in new[] { "run", "runs", "running" })
        {
            var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), term);
            Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        }
        // "the" is an English stopword: it matches nothing.
        Assert.Empty(await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "the"));
    }

    [Fact]
    public async Task SearchFuzzyMatchesTypos()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure emulator", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "other things", ["price"] = 2.0 },
        });

        foreach (var term in new[] { "emu~", "omul~", "emul~2" })
        {
            var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), term);
            Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        }
        Assert.Empty(await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "zzz~"));
    }

    [Fact]
    public async Task SearchFuzzyLowercasesButDoesNotStem()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure emulator", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "other things", ["price"] = 2.0 },
        });

        // The indexed term is the stem "emul". A fuzzy term is lowercased only
        // (no stemming), matching Azure: "emul" matches, but the full word
        // "emulator" is 4 edits from "emul" and matches nothing.
        Assert.Equal(new[] { "1" }, (await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "emul~")).Select(d => (string)d["id"]));
        Assert.Empty(await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "emulator~"));
        // Fuzzy terms are case-insensitive.
        Assert.Equal(new[] { "1" }, (await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "EMUL~")).Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchScoresRankByRelevance()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure azure azure", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "azure", ["price"] = 2.0 },
        });

        var scored = await RunSearchWithScores<SearchDocument>(searchClient, new SearchOptions(), "azure");
        Assert.Equal(new[] { "1", "2" }, scored.Select(p => (string)p.Document["id"]));
        Assert.All(scored, p => Assert.True(p.Score > 0, $"expected a positive score, got {p.Score}"));
        Assert.True(scored[0].Score >= scored[1].Score, "expected score ordering");
    }

    [Fact]
    public async Task SearchHighlights()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "Azure Search Rocks", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "Other things", ["price"] = 2.0 },
        });

        var response = await searchClient.SearchAsync<SearchDocument>(
            "azure", new SearchOptions { HighlightFields = { "title" } });
        var results = new List<SearchResult<SearchDocument>>();
        await foreach (var item in response.Value.GetResultsAsync())
        {
            results.Add(item);
        }
        Assert.Single(results);
        Assert.Equal(new[] { "<em>Azure</em> Search Rocks" }, results[0].Highlights["title"]);

        response = await searchClient.SearchAsync<SearchDocument>(
            "azure",
            new SearchOptions
            {
                HighlightFields = { "title" },
                HighlightPreTag = "<b>",
                HighlightPostTag = "</b>",
            });
        results.Clear();
        await foreach (var item in response.Value.GetResultsAsync())
        {
            results.Add(item);
        }
        Assert.Equal(new[] { "<b>Azure</b> Search Rocks" }, results[0].Highlights["title"]);

        // Highlighting an unknown field is rejected explicitly.
        var ex = await Assert.ThrowsAsync<RequestFailedException>(
            async () => await RunSearch<SearchDocument>(
                searchClient, new SearchOptions { HighlightFields = { "missing" } }, "azure"));
        Assert.Equal(400, ex.Status);
        Assert.Equal("InvalidQuery", ex.ErrorCode);
    }

    [Fact]
    public async Task SearchFieldsWeightsBoostScores()
    {
        var searchClient = await PricedDocsAsync();
        var plain = await RunSearchWithScores<SearchDocument>(searchClient, new SearchOptions(), "red");
        Assert.Single(plain);
        var boosted = await RunSearchWithScores<SearchDocument>(
            searchClient, new SearchOptions { SearchFields = { "title^10" } }, "red");
        Assert.Single(boosted);
        Assert.True(boosted[0].Score > plain[0].Score,
            $"boosted score {boosted[0].Score} should exceed plain score {plain[0].Score}");
    }

    [Fact]
    public async Task SearchFilterInOperator()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "price in (5.0, 500.0)" }, "*");
        Assert.Equal(new[] { "1", "3" }, found.Select(d => (string)d["id"]).OrderBy(x => x));
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "price in (1.0, 2.0)" }, "*");
        Assert.Empty(found);
    }

    [Fact]
    public async Task SearchFilterStringFunctions()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "startswith(title, 'cheap')" }, "*");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "endswith(title, 'blue')" }, "*");
        Assert.Equal(new[] { "2" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "contains(title, 'ens')" }, "*");
        Assert.Equal(new[] { "3" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { Filter = "title in ('cheap red', 'other')" }, "*");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchOrderByNulls()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "priced", ["price"] = 10.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "unpriced" },
            new SearchDocument { ["id"] = "3", ["title"] = "cheap", ["price"] = 1.0 },
        });

        var found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { OrderBy = { "price asc" } }, "*");
        Assert.Equal(new[] { "2", "3", "1" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(
            searchClient, new SearchOptions { OrderBy = { "price desc" } }, "*");
        Assert.Equal(new[] { "1", "3", "2" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task AliasResolvesForSearchAndDocuments()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "hello" },
        });
        await indexClient.CreateAliasAsync(new SearchAlias("al", new[] { IndexName }));

        var aliasClient = SearchClient("al");
        var found = await RunSearch<SearchDocument>(aliasClient, new SearchOptions(), "hello");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        var doc = (await aliasClient.GetDocumentAsync<SearchDocument>("1")).Value;
        Assert.Equal("1", (string)doc["id"]);
        Assert.Equal(1, (await aliasClient.GetDocumentCountAsync()).Value);
    }

    [Fact]
    public async Task SuggestAndAutocompleteInfix()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(new SearchIndex(IndexName)
        {
            Fields =
            {
                new SearchField("id", SearchFieldDataType.String) { IsKey = true },
                new SearchableField("title"),
            },
            Suggesters = { new SearchSuggester("sg", new[] { "title" }) },
        });
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "Boston Harbor Hotel" },
            new SearchDocument { ["id"] = "2", ["title"] = "Portland Airport Inn" },
        });

        // "rbor" is an infix (not a prefix) of "Harbor".
        var suggestions = (await searchClient.SuggestAsync<SearchDocument>("rbor", "sg", new SuggestOptions())).Value;
        Assert.Equal(new[] { "1" }, suggestions.Results.Select(d => (string)d.Document["id"]));
        // "osto" is an infix of "Boston".
        var completions = (await searchClient.AutocompleteAsync("osto", "sg", new AutocompleteOptions())).Value;
        Assert.Equal(new[] { "Boston" }, completions.Results.Select(c => c.Text));
    }

    [Fact]
    public async Task AnalyzeTextWithAnalyzer()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var tokens = (await indexClient.AnalyzeTextAsync(
            IndexName,
            new AnalyzeTextOptions("Running tests") { AnalyzerName = "standard.lucene" })).Value
            .Select(t => t.Token).ToList();
        Assert.Equal(new[] { "run", "test" }, tokens);
    }

    [Fact]
    public async Task AnalyzeTextKeywordAndWhitespaceAnalyzers()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        var keyword = (await indexClient.AnalyzeTextAsync(
            IndexName,
            new AnalyzeTextOptions("Running Tests") { AnalyzerName = "keyword" })).Value
            .Select(t => t.Token).ToList();
        Assert.Equal(new[] { "Running Tests" }, keyword);
        var whitespace = (await indexClient.AnalyzeTextAsync(
            IndexName,
            new AnalyzeTextOptions("Running tests") { AnalyzerName = "whitespace" })).Value
            .Select(t => t.Token).ToList();
        Assert.Equal(new[] { "Running", "tests" }, whitespace);
    }

    [Fact]
    public async Task SearchOrderByScore()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure azure azure", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "azure", ["price"] = 2.0 },
        });

        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { OrderBy = { "@search.score desc" } }, "azure");
        Assert.Equal(new[] { "1", "2" }, found.Select(d => (string)d["id"]));
        found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { OrderBy = { "@search.score asc" } }, "azure");
        Assert.Equal(new[] { "2", "1" }, found.Select(d => (string)d["id"]));
    }

    [Fact]
    public async Task SearchSelectStar()
    {
        var searchClient = await PricedDocsAsync();
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions { Select = { "*" } }, "*");
        Assert.Equal(3, found.Count);
        foreach (var doc in found)
        {
            Assert.True(doc.ContainsKey("id"));
            Assert.True(doc.ContainsKey("title"));
            Assert.True(doc.ContainsKey("price"));
            Assert.True(doc.ContainsKey("tags"));
        }
    }

    [Fact]
    public async Task SearchFuzzyDefaultDistanceTwo()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "azure emulator", ["price"] = 1.0 },
            new SearchDocument { ["id"] = "2", ["title"] = "other things", ["price"] = 2.0 },
        });

        // A bare `~` uses the default edit distance 2 (matching Azure).
        var found = await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "eamu~");
        Assert.Equal(new[] { "1" }, found.Select(d => (string)d["id"]));
        Assert.Empty(await RunSearch<SearchDocument>(searchClient, new SearchOptions(), "eamu~1"));
    }

    [Fact]
    public async Task AliasConflictsWithIndexName()
    {
        var indexClient = IndexClient();
        await indexClient.CreateIndexAsync(TestData.FullIndex(IndexName));

        // An alias cannot take the name of an existing index.
        var ex = await Assert.ThrowsAsync<RequestFailedException>(
            () => indexClient.CreateAliasAsync(new SearchAlias(IndexName, new[] { IndexName })));
        Assert.Equal(409, ex.Status);

        // An index cannot take the name of an existing alias.
        await indexClient.CreateAliasAsync(new SearchAlias("al", new[] { IndexName }));
        ex = await Assert.ThrowsAsync<RequestFailedException>(
            () => indexClient.CreateIndexAsync(TestData.FullIndex("al")));
        Assert.Equal(409, ex.Status);
    }

    [Fact]
    public async Task BatchLastActionWins()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());

        // Upload-then-delete in one batch (over raw HTTP, which is the only
        // wire shape that mixes actions): the document is gone.
        var (status, body) = await RawPostAsync(
            $"{BaseUrl}/indexes('{IndexName}')/docs/search.index?api-version={ApiVersion}",
            """
            {"value": [
                {"@search.action": "upload", "id": "9", "title": "nine", "price": 9.0},
                {"@search.action": "delete", "id": "9"}
            ]}
            """);
        Assert.Equal(200, status);
        Assert.Equal(0, (await searchClient.GetDocumentCountAsync()).Value);

        // Delete-then-upload: the upload wins.
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one", ["price"] = 1.0 },
        });
        (status, _) = await RawPostAsync(
            $"{BaseUrl}/indexes('{IndexName}')/docs/search.index?api-version={ApiVersion}",
            """
            {"value": [
                {"@search.action": "delete", "id": "1"},
                {"@search.action": "upload", "id": "1", "title": "replaced", "price": 2.0}
            ]}
            """);
        Assert.Equal(200, status);
        var doc = (await searchClient.GetDocumentAsync<SearchDocument>("1")).Value;
        Assert.Equal("replaced", (string)doc["title"]);
    }
}
