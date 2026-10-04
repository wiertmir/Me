using System.Net;
using System.Security.Claims;
using System.Text;
using AuthWeb.Services;
using Bunit;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;

namespace AuthWeb.Tests;

public record Recorded(string Method, string Path, string Body);

/// <summary>Canned JSON per "METHOD /path"; records every request. Unlisted routes answer 200 with an empty list.</summary>
public sealed class StubHandler : HttpMessageHandler
{
    public List<Recorded> Requests { get; } = [];
    public Dictionary<string, Func<(HttpStatusCode, string)>> Routes { get; } = [];

    public StubHandler Reply(string route, HttpStatusCode code, string json) { Routes[route] = () => (code, json); return this; }

    /// <summary>When set, every answer waits for it (to keep a call pending while the test acts).</summary>
    public Task Gate { get; set; } = Task.CompletedTask;

    public int Count(string route) => Requests.Count(r => $"{r.Method} {r.Path}" == route);

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(ct);
        var key = $"{request.Method} {request.RequestUri!.AbsolutePath}";
        Requests.Add(new(request.Method.Method, request.RequestUri.AbsolutePath, body));
        var (code, json) = Routes.TryGetValue(key, out var f) ? f() : (HttpStatusCode.OK, "[]");
        await Gate;
        return new HttpResponseMessage(code) { Content = new StringContent(json, Encoding.UTF8, "application/json") };
    }
}

public abstract class PageTest : BunitContext
{
    protected readonly StubHandler Stub = new();

    protected PageTest()
    {
        JSInterop.Mode = JSRuntimeMode.Loose;
        var api = new AuthApi(new HttpClient(Stub) { BaseAddress = new Uri("http://stub") },
            new RequestContext(new HttpContextAccessor()), NullLogger<AuthApi>.Instance);
        Services.AddSingleton(api);
        Stub.Reply("GET /api/providers", HttpStatusCode.OK, """["github"]""");
        Stub.Reply("GET /api/me", HttpStatusCode.OK, UserJson("alice"));
        var auth = AddAuthorization();
        auth.SetAuthorized("alice");
        auth.SetClaims(new Claim(SessionPrincipal.SessionClaim, "tok"));
    }

    protected static string Error(string code, string message = "m") => $$"""{"code":"{{code}}","message":"{{message}}"}""";

    protected static string UserJson(string name, bool admin = false, bool hasPassword = true) => $$"""
        {"id":"{{Guid.NewGuid()}}","username":"{{name}}","email":"{{name}}@x.test","email_verified":true,"display_name":"{{name}}",
         "is_admin":{{(admin ? "true" : "false")}},"must_change_password":false,"has_password":{{(hasPassword ? "true" : "false")}},"disabled":false,"created_at":"2026-01-02T03:04:05Z"}
        """;

    protected static AngleSharp.Dom.IElement Button(IRenderedComponent<Microsoft.AspNetCore.Components.IComponent> cut, string text) =>
        cut.FindAll("button").First(b => b.TextContent.Contains(text));
}
