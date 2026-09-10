using System.Net;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using Azure;
using Azure.Search.Documents;
using Azure.Search.Documents.Indexes;
using Azure.Search.Documents.Models;

namespace Emulator.Tests.Infrastructure;

/// <summary>
/// Base class for every emulator-backed test. Provides the shared client
/// options (pinned API version), per-test state reset, SDK client factories,
/// and raw-HTTP helpers for the few cases the SDK cannot express (health,
/// auth/version errors, the bare-number <c>$count</c> facet).
/// </summary>
[Collection(EmulatorCollection.Name)]
public abstract class EmulatorTestBase : IAsyncLifetime
{
    public const string ApiKey = "test-key";
    public const string ApiVersion = "2024-07-01";

    protected EmulatorEndpoint Endpoint { get; }

    /// <summary>The emulator's real plain-HTTP base URL (used by the raw-HTTP
    /// helpers and the fixture replay).</summary>
    protected string BaseUrl => Endpoint.Url;

    /// <summary>https:// form of the base URL, handed to the SDK clients so the
    /// constructor's <c>AssertHttpsScheme</c> check passes; the
    /// <see cref="HttpSchemeRewritingTransport"/> downgrades it back to http on
    /// send.</summary>
    protected string SdkBaseUrl => BaseUrl.Replace("http://", "https://");

    /// <summary>Client options pinning the emulator's default supported API
    /// version (the SDK defaults to a newer one, mirroring the Python suite's
    /// <c>api_version="2024-07-01"</c>) and routing through the scheme-rewriting
    /// transport so the plain-HTTP emulator is reachable.</summary>
    protected SearchClientOptions Options { get; }

    private readonly HttpClient _http = new() { Timeout = TimeSpan.FromSeconds(10) };

    protected EmulatorTestBase(EmulatorEndpoint endpoint)
    {
        Endpoint = endpoint;
        Options = new SearchClientOptions(SearchClientOptions.ServiceVersion.V2024_07_01)
        {
            Transport = new HttpSchemeRewritingTransport(),
        };
    }

    public async Task InitializeAsync()
    {
        await ResetAsync();
    }

    public Task DisposeAsync()
    {
        _http.Dispose();
        return Task.CompletedTask;
    }

    /// <summary>Reset all emulator state so each test starts clean (the .NET
    /// equivalent of the Python <c>clean_emulator</c> fixture).</summary>
    protected async Task ResetAsync()
    {
        using var request = new HttpRequestMessage(HttpMethod.Post, $"{BaseUrl}/admin/reset");
        request.Content = new StringContent("", Encoding.UTF8, "application/json");
        using var response = await _http.SendAsync(request);
        response.EnsureSuccessStatusCode();
    }

    /// <summary>Drain a <see cref="SearchResults{T}"/> into a list, following
    /// continuation tokens across pages (the .NET equivalent of iterating the
    /// Python SDK's lazy paged result).</summary>
    protected static async Task<List<T>> CollectAsync<T>(SearchResults<T> results)
    {
        var list = new List<T>();
        await foreach (var item in results.GetResultsAsync())
        {
            list.Add(item.Document);
        }
        return list;
    }

    /// <summary>Run a search and drain all pages into a list. Pass
    /// <c>query: null</c> for vector-only searches (no full-text term).</summary>
    protected static async Task<List<T>> RunSearch<T>(SearchClient client, SearchOptions options, string? query = null)
    {
        var response = query is null
            ? await client.SearchAsync<T>(options)
            : await client.SearchAsync<T>(query, options);
        return await CollectAsync(response.Value);
    }

    /// <summary>Run a search and return (document, score) pairs. The .NET SDK
    /// exposes <c>@search.score</c> on <see cref="SearchResult{T}"/> rather than
    /// inside the document dictionary (unlike the Python SDK).</summary>
    protected static async Task<List<(T Document, double? Score)>> RunSearchWithScores<T>(
        SearchClient client, SearchOptions options, string? query = null)
    {
        var response = query is null
            ? await client.SearchAsync<T>(options)
            : await client.SearchAsync<T>(query, options);
        var list = new List<(T, double?)>();
        await foreach (var item in response.Value.GetResultsAsync())
        {
            list.Add((item.Document, item.Score));
        }
        return list;
    }

    protected SearchIndexClient IndexClient() =>
        new(new Uri(SdkBaseUrl), new AzureKeyCredential(ApiKey), Options);

    protected SearchClient SearchClient(string indexName) =>
        new(new Uri(SdkBaseUrl), indexName, new AzureKeyCredential(ApiKey), Options);

    // --- Raw HTTP helpers ---------------------------------------------------

    /// <summary>Issue a GET and return (status, body) without throwing on 4xx/5xx.</summary>
    protected async Task<(int Status, string Body)> RawGetAsync(string url, string? apiKey = null)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, url);
        request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue("application/json"));
        if (apiKey is not null)
        {
            request.Headers.Add("api-key", apiKey);
        }
        using var response = await _http.SendAsync(request);
        return ((int)response.StatusCode, await response.Content.ReadAsStringAsync());
    }

    /// <summary>Issue a JSON POST and return (status, body) without throwing.</summary>
    protected async Task<(int Status, string Body)> RawPostAsync(string url, string jsonBody, string? apiKey = ApiKey)
    {
        using var request = new HttpRequestMessage(HttpMethod.Post, url);
        if (apiKey is not null)
        {
            request.Headers.Add("api-key", apiKey);
        }
        request.Content = new StringContent(jsonBody, Encoding.UTF8, "application/json");
        using var response = await _http.SendAsync(request);
        return ((int)response.StatusCode, await response.Content.ReadAsStringAsync());
    }

    /// <summary>Issue an arbitrary request (any method, optional JSON body and
    /// extra headers) and return (status, body) without throwing on 4xx/5xx.
    /// Used by the fixture-replay test.</summary>
    protected async Task<(int Status, string Body)> RawRequestAsync(
        string method, string url, string? jsonBody = null, IEnumerable<KeyValuePair<string, string>>? headers = null)
    {
        using var request = new HttpRequestMessage(new HttpMethod(method), url);
        if (headers is not null)
        {
            foreach (var (name, value) in headers)
            {
                request.Headers.TryAddWithoutValidation(name, value);
            }
        }
        if (jsonBody is not null)
        {
            request.Content = new StringContent(jsonBody, Encoding.UTF8, "application/json");
        }
        using var response = await _http.SendAsync(request);
        return ((int)response.StatusCode, await response.Content.ReadAsStringAsync());
    }

    /// <summary>Assert an Azure-structured error body: exactly
    /// <c>{"error": {"code", "message"}}</c>.</summary>
    protected static void AssertAzureError(string body, string expectedCode)
    {
        using var doc = System.Text.Json.JsonDocument.Parse(body);
        var root = doc.RootElement;
        Assert.True(root.TryGetProperty("error", out var error),
            $"expected an 'error' member, got: {body}");
        Assert.True(error.TryGetProperty("code", out var code),
            $"expected error.code, got: {body}");
        Assert.True(error.TryGetProperty("message", out _),
            $"expected error.message, got: {body}");
        Assert.Equal(expectedCode, code.GetString());
    }
}
