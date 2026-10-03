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
