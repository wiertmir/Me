using System.Security.Claims;
using Microsoft.AspNetCore.Authentication;
using Microsoft.AspNetCore.Authentication.Cookies;

namespace AuthWeb.Services;

public static class SessionPrincipal
{
    public const string SessionClaim = "session";
    public const string MustChangeClaim = "must_change";

    /// <summary>Issues (or re-issues) the encrypted auth cookie; the session token lives only inside it.</summary>
    public static Task SignIn(HttpContext ctx, string sessionToken, User user)
    {
        var claims = new List<Claim>
        {
            new(SessionClaim, sessionToken),
            new("sub", user.Id.ToString()),
            new("name", user.Username),
            new(MustChangeClaim, user.MustChangePassword ? "true" : "false"),
        };
        if (user.IsAdmin) claims.Add(new Claim(ClaimTypes.Role, "admin"));
        var identity = new ClaimsIdentity(claims, CookieAuthenticationDefaults.AuthenticationScheme, "name", ClaimTypes.Role);
        return ctx.SignInAsync(
            CookieAuthenticationDefaults.AuthenticationScheme,
            new ClaimsPrincipal(identity),
            new AuthenticationProperties { IsPersistent = true, ExpiresUtc = DateTimeOffset.UtcNow.AddDays(30) });
    }

    public static string? Token(ClaimsPrincipal user) => user.FindFirstValue(SessionClaim);

    public static bool MustChange(ClaimsPrincipal user) => user.FindFirstValue(MustChangeClaim) == "true";
}

public static class LogText
{
    /// <summary>
    /// Text a visitor typed, made safe to log: control characters (CR, LF, escapes…) removed so it cannot forge
    /// or break log lines, and cut to 64 characters. The service applies the same rule to its own log.
    /// </summary>
    public static string Clean(string? s) => new((s ?? "").Where(c => !char.IsControl(c)).Take(64).ToArray());
}

public static class LocalUrl
{
    /// <summary>A path on this site: one leading '/', not '//' or '/\', no control characters.</summary>
    public static bool IsLocal(string? url)
    {
        if (string.IsNullOrEmpty(url) || url[0] != '/') return false;
        if (url.Length > 1 && (url[1] == '/' || url[1] == '\\')) return false;
        return !url.Any(char.IsControl);
    }
}

public static class InteractiveRedirects
{
    /// <summary>
    /// For interactive components: sends the browser to the right page for failures every user-scoped call can
    /// return (a full page load, so the cookie can be cleared). True when it navigated.
    /// </summary>
    public static bool Handle(this Microsoft.AspNetCore.Components.NavigationManager nav, ApiResult r)
    {
        var target = r.Code switch
        {
            "unauthorized" => "/session-expired",
            "password_change_required" => "/change-password",
            "unavailable" => "/unavailable",
            _ => null,
        };
        if (target is null) return false;
        nav.NavigateTo(target, forceLoad: true);
        return true;
    }
}
