using System.Text.Json;
using Emulator.Tests.Infrastructure;

namespace Emulator.Tests;

/// <summary>
/// Replays the Python-captured HTTP fixtures (source/tests/python/fixtures)
/// against the emulator and asserts each exchange's status code. This is the
/// wire-format compatibility probe: the exact request shapes the official
/// Python SDK emits must be accepted by the emulator. The fixtures form a
/// coherent sequence (create index, upload, search, delete, vector index,
/// vector + hybrid search) and are replayed in order against a fresh emulator.
/// </summary>
public class FixtureReplayTests : EmulatorTestBase
{
    public FixtureReplayTests(EmulatorEndpoint endpoint) : base(endpoint)
    {
    }

    private static DirectoryInfo FixturesDir()
    {
        var root = Infrastructure.Docker.FindRepoRoot();
        return new DirectoryInfo(Path.Combine(root.FullName, "source", "tests", "python", "fixtures"));
    }

    [Fact]
    public async Task ReplayCapturedFixtures()
    {
        var dir = FixturesDir();
        Assert.True(dir.Exists, $"fixtures directory not found at {dir.FullName}");
        var fixtures = dir.GetFiles("*.json").OrderBy(f => f.Name).ToArray();
        Assert.NotEmpty(fixtures);

        foreach (var file in fixtures)
        {
            using var doc = JsonDocument.Parse(File.ReadAllText(file.FullName));
            var root = doc.RootElement;
            var method = root.GetProperty("method").GetString() ?? throw new InvalidOperationException("missing method");
            var url = root.GetProperty("url").GetString()!.Replace("http://<endpoint>", BaseUrl);
            var expectedStatus = root.GetProperty("status_code").GetInt32();
            string? body = root.TryGetProperty("request_body", out var rb) && rb.ValueKind != JsonValueKind.Null
                ? rb.GetString()
                : null;
            // Forward the captured api-key (any non-empty key is accepted).
            var headers = new List<KeyValuePair<string, string>>();
            if (root.TryGetProperty("headers", out var hdrs)
                && hdrs.TryGetProperty("api-key", out var apiKey))
            {
                headers.Add(new KeyValuePair<string, string>("api-key", apiKey.GetString()!));
            }

            var (status, responseBody) = await RawRequestAsync(method, url, body, headers);
            Assert.True(
                status == expectedStatus,
                $"{file.Name}: expected {expectedStatus} for {method} {url}, got {status}: {responseBody}");
        }
    }
}
