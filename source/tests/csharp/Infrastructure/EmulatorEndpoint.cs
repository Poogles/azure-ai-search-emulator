using System.Net;
using System.Net.Http;
using DotNet.Testcontainers.Builders;
using DotNet.Testcontainers.Containers;
using DotNet.Testcontainers.Images;

namespace Emulator.Tests.Infrastructure;

/// <summary>
/// Session-scoped (per-collection) fixture that owns the emulator container for
/// the whole C# suite. It builds the image if it is missing (Docker CLI), then
/// runs it through Testcontainers — the .NET equivalent of the
/// <c>emulator_image</c> + <c>emulator_endpoint</c> fixtures in
/// source/tests/python/conftest.py. The container is stopped on disposal.
/// </summary>
public sealed class EmulatorEndpoint : IAsyncDisposable
{
    private const int ContainerPort = 8080;

    private readonly IContainer _container;
    private readonly HttpClient _http = new() { Timeout = TimeSpan.FromSeconds(10) };

    public string Url { get; }

    public EmulatorEndpoint()
    {
        if (!Docker.ImageExists(Docker.ImageName))
        {
            var context = Path.Combine(Docker.FindRepoRoot().FullName, "source", "rust");
            Docker.BuildImage(Docker.ImageName, context);
        }

        _container = new ContainerBuilder(new DockerImage(Docker.ImageName))
            .WithPortBinding(ContainerPort, assignRandomHostPort: true)
            .Build();
        _container.StartAsync().GetAwaiter().GetResult();
        var port = _container.GetMappedPublicPort(ContainerPort);
        Url = $"http://127.0.0.1:{port}";
        WaitForHealth(Url, timeoutSeconds: 30);
    }

    private static void WaitForHealth(string url, double timeoutSeconds)
    {
        var deadline = DateTime.UtcNow.AddSeconds(timeoutSeconds);
        using var http = new HttpClient { Timeout = TimeSpan.FromSeconds(2) };
        while (DateTime.UtcNow < deadline)
        {
            try
            {
                using var response = http.GetAsync($"{url}/health").GetAwaiter().GetResult();
                if (response.StatusCode == HttpStatusCode.OK)
                {
                    return;
                }
            }
            catch
            {
                // not up yet
            }
            Thread.Sleep(500);
        }
        throw new InvalidOperationException(
            $"emulator did not become healthy within {timeoutSeconds}s at {url}");
    }

    public async ValueTask DisposeAsync()
    {
        await _container.StopAsync();
        await _container.DisposeAsync();
        _http.Dispose();
    }
}
