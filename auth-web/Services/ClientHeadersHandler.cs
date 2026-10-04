using Microsoft.Extensions.Options;

namespace AuthWeb.Services;

public sealed class AuthServiceOptions
{
    public string BaseUrl { get; set; } = "";
    public string PublicUrl { get; set; } = "";
    public string Secret { get; set; } = "";
}

/// <summary>
/// Adds the service secret and the client headers to every call. Handlers live in the HttpClientFactory's own DI
/// scope, so the per-caller <see cref="RequestContext"/> arrives on the request's Options (set by AuthApi).
/// </summary>
public sealed class ClientHeadersHandler(IOptions<AuthServiceOptions> options) : DelegatingHandler
{
    public static readonly HttpRequestOptionsKey<RequestContext> ContextKey = new("me.request-context");

    // Non-ASCII or control characters make HttpClient throw, which would look like an outage.
    private static string Ascii(string s) => string.Concat(s.Select(c => c is >= ' ' and <= '~' ? c : '?'));

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        request.Headers.Add("X-Service-Secret", options.Value.Secret);
        if (request.Options.TryGetValue(ContextKey, out var ctx))
        {
            request.Headers.Add("X-Request-Id", ctx.RequestId);
            if (ctx.ClientIp is { } ip) request.Headers.Add("X-Forwarded-For", ip);
            if (!string.IsNullOrEmpty(ctx.UserAgent)) request.Headers.TryAddWithoutValidation("X-Client-User-Agent", Ascii(ctx.UserAgent));
        }
        return base.SendAsync(request, ct);
    }
}
