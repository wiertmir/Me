using AuthWeb.Services;
using Microsoft.AspNetCore.Authentication;
using Microsoft.AspNetCore.Components;

namespace AuthWeb.Components;

/// <summary>Base for the static-SSR pages that set or clear the cookie and decide where to go next.</summary>
public abstract class SsrPage : ComponentBase
{
    [Inject] protected AuthApi Api { get; set; } = default!;
    [Inject] protected NavigationManager Nav { get; set; } = default!;
    [CascadingParameter] protected HttpContext HttpContext { get; set; } = default!;

    protected string? SessionToken => SessionPrincipal.Token(HttpContext.User);

    /// <summary>Redirects for the failures every page treats the same. True when it redirected.</summary>
    protected async Task<bool> Redirected(ApiResult r, bool userScoped = true)
    {
        switch (r.Code)
        {
            case "unavailable":
                Nav.NavigateTo("/unavailable");
                return true;
            case "password_change_required":
                Nav.NavigateTo("/change-password");
                return true;
            case "unauthorized" when userScoped:
                await HttpContext.SignOutAsync(); // the service no longer knows this session
                Nav.NavigateTo("/signin");
                return true;
            default:
                return false;
        }
    }

    /// <summary>True when this request is a POST of the named form (so a page can tell which form was submitted).</summary>
    protected bool Posted(string formName) =>
        HttpMethods.IsPost(HttpContext.Request.Method) && HttpContext.Request.HasFormContentType
        && HttpContext.Request.Form["_handler"] == formName;

    /// <summary>"?challenge=…&amp;returnUrl=…" for links that must carry the pending sign-in along; "" when neither is set.</summary>
    protected static string CarryQuery(string? challenge, string? returnUrl)
    {
        var q = new List<string>();
        if (!string.IsNullOrEmpty(challenge)) q.Add("challenge=" + Uri.EscapeDataString(challenge));
        if (LocalUrl.IsLocal(returnUrl)) q.Add("returnUrl=" + Uri.EscapeDataString(returnUrl!));
        return q.Count > 0 ? "?" + string.Join('&', q) : "";
    }

    protected static string Describe(ApiResult r) => r.Code switch
    {
        "rate_limited" => "Too many attempts. Try again in a moment.",
        "invalid_challenge" => "This sign-in request has expired. Return to the app and try again.",
        _ => r.Message ?? "Something went wrong.",
    };

    /// <summary>
    /// After a successful sign-in or password change: change-password if still required, else accept the
    /// challenge, else a local returnUrl, else /account. Returns an error message to show, or null when redirected.
    /// </summary>
    protected async Task<string?> Continue(string token, bool mustChange, string? challenge, string? returnUrl)
    {
        if (mustChange)
        {
            Nav.NavigateTo("/change-password" + CarryQuery(challenge, returnUrl));
            return null;
        }
        if (!string.IsNullOrEmpty(challenge))
        {
            var r = await Api.AcceptAuthRequest(token, challenge);
            if (r.Ok) { Nav.NavigateTo(r.Value!.RedirectTo); return null; }
            if (await Redirected(r)) return null;
            return Describe(r);
        }
        Nav.NavigateTo(LocalUrl.IsLocal(returnUrl) ? returnUrl! : "/account");
        return null;
    }
}
