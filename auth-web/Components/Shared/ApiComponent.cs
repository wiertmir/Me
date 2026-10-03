using AuthWeb.Services;
using Microsoft.AspNetCore.Components;
using Microsoft.AspNetCore.Components.Authorization;

namespace AuthWeb.Components.Shared;

/// <summary>What a <see cref="ConfirmDialog"/> asks, and what to run if the user agrees.</summary>
public record ConfirmRequest(string Message, string ConfirmText, Func<Task> Action);

/// <summary>Base for interactive components that call the API as the signed-in user.</summary>
public abstract class ApiComponent : ComponentBase
{
    [Inject] protected AuthApi Api { get; set; } = default!;
    [Inject] protected NavigationManager Nav { get; set; } = default!;
    [Inject] protected AuthenticationStateProvider AuthState { get; set; } = default!;

    /// <summary>
    /// Runs one API call with the session token from the cookie's claims. Returns null after navigating away for
    /// the failures every user call shares (expired session, forced password change, outage).
    /// </summary>
    protected async Task<T?> Call<T>(Func<string, Task<T>> call) where T : ApiResult
    {
        var principal = (await AuthState.GetAuthenticationStateAsync()).User;
        if (SessionPrincipal.Token(principal) is not { } token)
        {
            Nav.NavigateTo("/session-expired", forceLoad: true);
            return null;
        }
        var r = await call(token);
        return Nav.Handle(r) ? null : r;
    }

    protected static string Describe(ApiResult r) => r.Code switch
    {
        "rate_limited" => "Too many attempts. Try again in a moment.",
        "forbidden" => "You are not allowed to do that.",
        _ => r.Message ?? "Something went wrong.",
    };
}
