using Azure.Search.Documents.Indexes.Models;
using Azure.Search.Documents.Models;

namespace Emulator.Tests.Infrastructure;

/// <summary>
/// Shared index and document shapes used across the C# suite. Mirrors the
/// <c>full_index</c> / <c>priced_docs</c> fixtures in
/// source/tests/python/tests/sdk/test_sdk.py so the two suites exercise the
/// same data.
/// </summary>
internal static class TestData
{
    public const string IndexName = "csharp-index";

    /// <summary>The standard four-field index: string key, searchable+filterable
    /// title, filterable+sortable double price, filterable+facetable string
    /// collection tags.</summary>
    public static SearchIndex FullIndex(string name = IndexName) => new(name)
    {
        Fields =
        {
            new SearchField("id", SearchFieldDataType.String) { IsKey = true },
            new SearchableField("title") { IsFilterable = true },
            new SimpleField("price", SearchFieldDataType.Double) { IsFilterable = true, IsSortable = true },
            new SimpleField("tags", SearchFieldDataType.Collection(SearchFieldDataType.String))
            {
                IsFilterable = true,
                IsFacetable = true,
            },
        },
    };

    /// <summary>Three documents with distinct prices and overlapping tags, used
    /// by the filter/orderby/select/facet tests.</summary>
    public static SearchDocument[] PricedDocs() =>
    [
        new() { ["id"] = "1", ["title"] = "cheap red", ["price"] = 5.0, ["tags"] = new[] { "red" } },
        new() { ["id"] = "2", ["title"] = "mid blue", ["price"] = 50.0, ["tags"] = new[] { "blue", "red" } },
        new() { ["id"] = "3", ["title"] = "expensive green", ["price"] = 500.0, ["tags"] = new[] { "green" } },
    ];
}
