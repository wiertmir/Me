using AuthWeb.Services;
using NLog;

namespace AuthWeb.Infrastructure;

/// <summary>One request id per HTTP request: logged (NLog scope), returned, and forwarded by the API handler.</summary>
public sealed class RequestIdMiddleware(RequestDelegate next)
{
    public async Task Invoke(HttpContext ctx)
    {
        var incoming = ctx.Request.Headers["X-Request-Id"].ToString();
        var id = incoming.Length is > 0 and <= 128 && incoming.All(c => c is > ' ' and < '\u007f')
            ? incoming
            : Guid.NewGuid().ToString();
        ctx.Items[RequestContext.ItemKey] = id;
        // OnStarting: the exception handler clears the response before re-executing, eager headers would vanish.
        ctx.Response.OnStarting(() => { ctx.Response.Headers["X-Request-Id"] = id; return Task.CompletedTask; });
        using (ScopeContext.PushProperty("RequestId", id))
        {
            await next(ctx);
        }
    }
}

public sealed class SecurityHeadersMiddleware(RequestDelegate next)
{
    private static readonly string[] NoStorePaths = ["/signin", "/change-password", "/account", "/signup", "/verify", "/forgot", "/reset", "/social/complete", "/session-expired"];

    // form-action is deliberately absent: Chrome applies it to redirects, and sign-in redirects to the client's callback.
    private const string Csp = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; " +
                               "connect-src 'self'; base-uri 'self'; frame-ancestors 'none'";

    public Task Invoke(HttpContext ctx)
    {
        ctx.Response.OnStarting(() =>
        {
            var h = ctx.Response.Headers;
            h["X-Content-Type-Options"] = "nosniff";
            h["Referrer-Policy"] = "no-referrer";
            h["X-Frame-Options"] = "DENY";
            h["Content-Security-Policy"] = Csp;
            var path = ctx.Request.Path;
            if (NoStorePaths.Any(p => path.Equals(p, StringComparison.OrdinalIgnoreCase)))
                h.CacheControl = "no-store";
            return Task.CompletedTask;
        });
        return next(ctx);
    }
}

/// <summary>A signed-in user who must change a temporary password sees only the change page (and sign-out).</summary>
public sealed class MustChangeMiddleware(RequestDelegate next)
{
    private static readonly string[] Allowed =
        ["/change-password", "/signout", "/unavailable", "/session-expired", "/verify", "/reset", "/social/complete",
         "/_framework", "/_blazor", "/_content"];

    public Task Invoke(HttpContext ctx)
    {
        var path = ctx.Request.Path;
        if (SessionPrincipal.MustChange(ctx.User)
            && !Path.HasExtension(path.Value)
            && !Allowed.Any(p => path.StartsWithSegments(p, StringComparison.OrdinalIgnoreCase)))
        {
            ctx.Response.Redirect("/change-password" + ctx.Request.QueryString);
            return Task.CompletedTask;
        }
        return next(ctx);
    }
}
