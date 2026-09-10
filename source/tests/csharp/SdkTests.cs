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
    public async Task CreateOrUpdateIndexDiscardsDocuments()
    {
        var indexClient = IndexClient();
        var searchClient = SearchClient(IndexName);
        await indexClient.CreateIndexAsync(TestData.FullIndex());
        await searchClient.UploadDocumentsAsync(new[]
        {
            new SearchDocument { ["id"] = "1", ["title"] = "one" },
        });
        Assert.Equal(1, (await searchClient.GetDocumentCountAsync()).Value);
        await indexClient.CreateOrUpdateIndexAsync(TestData.FullIndex());
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
            new SearchOptions { SearchMode = SearchMode.Any },
            new SearchOptions { HighlightFields = { "title" } },
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
}
