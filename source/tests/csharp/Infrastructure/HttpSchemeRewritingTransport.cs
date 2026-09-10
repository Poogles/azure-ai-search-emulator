using Azure.Core;
using Azure.Core.Pipeline;

namespace Emulator.Tests.Infrastructure;

/// <summary>
/// Test-harness-only transport that rewrites an https request URI to http before
/// sending. The pinned .NET SDK (Azure.Search.Documents 12.0.0) rejects any
/// non-TLS endpoint in the client constructor (<c>AssertHttpsScheme</c>), which
/// is stricter than the Python SDK the emulator is primarily targeted at. The
/// harness therefore hands the SDK an https:// endpoint (satisfying the check)
/// and this transport transparently downgrades it to the emulator's plain-HTTP
/// URL. The full HTTP contract is still exercised end to end.
/// </summary>
internal sealed class HttpSchemeRewritingTransport : HttpPipelineTransport
{
    private readonly HttpPipelineTransport _inner =
        new HttpClientTransport(new System.Net.Http.HttpClient());

    public override void Process(Azure.Core.HttpMessage message)
    {
        Rewrite(message);
        _inner.Process(message);
    }

    public override ValueTask ProcessAsync(Azure.Core.HttpMessage message)
    {
        Rewrite(message);
        return _inner.ProcessAsync(message);
    }

    public override Azure.Core.Request CreateRequest() => _inner.CreateRequest();

    public override void Update(HttpPipelineTransportOptions options) => _inner.Update(options);

    private static void Rewrite(Azure.Core.HttpMessage message)
    {
        if (message.Request.Uri.Scheme == Uri.UriSchemeHttps)
        {
            message.Request.Uri.Scheme = Uri.UriSchemeHttp;
        }
    }
}
