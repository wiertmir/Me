using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;

namespace AuthWeb.Services;

public sealed class AuthApi(HttpClient http, RequestContext context, ILogger<AuthApi> logger)
{
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    };

    public Task<ApiResult<SignInResponse>> SignIn(string login, string password) =>
        Send<SignInResponse>(HttpMethod.Post, "/api/signin", null, new { login, password });

    public async Task<ApiResult> SignOut(string session) =>
        await Send<object>(HttpMethod.Post, "/api/signout", session, null);

    public async Task<ApiResult> ChangePassword(string session, string current, string @new) =>
        await Send<object>(HttpMethod.Post, "/api/password/change", session,
            new { current_password = current, new_password = @new });

    public Task<ApiResult<User>> Me(string session) =>
        Send<User>(HttpMethod.Get, "/api/me", session, null);

    public Task<ApiResult<AcceptResponse>> AcceptAuthRequest(string session, string challenge) =>
        Send<AcceptResponse>(HttpMethod.Post, $"/api/auth-requests/{Uri.EscapeDataString(challenge)}/accept", session, null);

    public Task<ApiResult<SignupResponse>> SignUp(string username, string email, string? displayName, string password) =>
        Send<SignupResponse>(HttpMethod.Post, "/api/signup", null,
            new { username, email, display_name = displayName, password });

    public async Task<ApiResult> VerifyEmail(string token) =>
        await Send<object>(HttpMethod.Post, "/api/email/verify", null, new { token });

    public async Task<ApiResult> ResendVerification(string email) =>
        await Send<object>(HttpMethod.Post, "/api/email/resend", null, new { email });

    public async Task<ApiResult> ForgotPassword(string email) =>
        await Send<object>(HttpMethod.Post, "/api/password/forgot", null, new { email });

    public async Task<ApiResult> ResetPassword(string token, string newPassword) =>
        await Send<object>(HttpMethod.Post, "/api/password/reset", null, new { token, new_password = newPassword });

    public Task<ApiResult<List<string>>> Providers() =>
        Send<List<string>>(HttpMethod.Get, "/api/providers", null, null, LogLevel.Warning); // optional: not an outage

    public Task<ApiResult<ExchangeResponse>> ExchangeSocialTicket(string ticket) =>
        Send<ExchangeResponse>(HttpMethod.Post, "/api/social/exchange", null, new { ticket });

    public Task<ApiResult<User>> UpdateDisplayName(string session, string displayName) =>
        Send<User>(HttpMethod.Patch, "/api/me", session, new { display_name = displayName });

    public Task<ApiResult<List<Identity>>> Identities(string session) =>
        Send<List<Identity>>(HttpMethod.Get, "/api/me/identities", session, null);

    public async Task<ApiResult> Unlink(string session, string provider) =>
        await Send<object>(HttpMethod.Delete, $"/api/me/identities/{Uri.EscapeDataString(provider)}", session, null);

    public Task<ApiResult<LinkIntentResponse>> LinkIntent(string session, string provider) =>
        Send<LinkIntentResponse>(HttpMethod.Post, "/api/social/link-intent", session, new { provider });

    public async Task<ApiResult> ConfirmLink(string session, string ticket) =>
        await Send<object>(HttpMethod.Post, "/api/me/identities/confirm", session, new { ticket });

    public Task<ApiResult<List<SessionInfo>>> Sessions(string session) =>
        Send<List<SessionInfo>>(HttpMethod.Get, "/api/me/sessions", session, null);

    public async Task<ApiResult> RevokeSession(string session, Guid id) =>
        await Send<object>(HttpMethod.Delete, $"/api/me/sessions/{id}", session, null);

    public Task<ApiResult<List<AppPasswordInfo>>> AppPasswords(string session) =>
        Send<List<AppPasswordInfo>>(HttpMethod.Get, "/api/me/app-passwords", session, null);

    public Task<ApiResult<CreatedAppPassword>> CreateAppPassword(string session, string label) =>
        Send<CreatedAppPassword>(HttpMethod.Post, "/api/me/app-passwords", session, new { label });

    public async Task<ApiResult> DeleteAppPassword(string session, Guid id) =>
        await Send<object>(HttpMethod.Delete, $"/api/me/app-passwords/{id}", session, null);

    public Task<ApiResult<List<User>>> AdminUsers(string session) =>
        Send<List<User>>(HttpMethod.Get, "/api/admin/users", session, null);

    public Task<ApiResult<CreateUserResponse>> AdminCreateUser(string session, string username, string email, string? displayName) =>
        Send<CreateUserResponse>(HttpMethod.Post, "/api/admin/users", session,
            new { username, email, display_name = displayName });

    public Task<ApiResult<User>> AdminPatchUser(string session, Guid id, bool? disabled, bool? isAdmin) =>
        Send<User>(HttpMethod.Patch, $"/api/admin/users/{id}", session, new { disabled, is_admin = isAdmin });

    public Task<ApiResult<ResetPasswordResponse>> AdminResetPassword(string session, Guid id) =>
        Send<ResetPasswordResponse>(HttpMethod.Post, $"/api/admin/users/{id}/reset-password", session, null);

    private async Task<ApiResult<T>> Send<T>(HttpMethod method, string path, string? session, object? body,
        LogLevel outage = LogLevel.Error)
    {
        using var request = new HttpRequestMessage(method, path);
        request.Options.Set(ClientHeadersHandler.ContextKey, context);
        if (session is not null) request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", session);
        if (body is not null) request.Content = JsonContent.Create(body, options: Json);
        try
        {
            using var response = await http.SendAsync(request);
            if (response.IsSuccessStatusCode)
            {
                if (typeof(T) == typeof(object)) return new ApiResult<T>(true, null, null, default);
                var value = await response.Content.ReadFromJsonAsync<T>(Json);
                if (value is not null) return new ApiResult<T>(true, null, null, value);
                return Unavailable<T>("empty or unreadable response", (int)response.StatusCode, outage);
            }
            ErrorBody? error = null;
            try { error = await response.Content.ReadFromJsonAsync<ErrorBody>(Json); }
            catch (Exception e) when (e is JsonException or NotSupportedException) { }
            // Anything that is not the service's JSON error shape (a proxy's HTML 502, say) counts as an outage.
            if (error is null || string.IsNullOrEmpty(error.Code))
                return Unavailable<T>("unexpected response", (int)response.StatusCode, outage);
            return new ApiResult<T>(false, error.Code, error.Message, default);
        }
        catch (Exception ex) when (ex is HttpRequestException or TaskCanceledException or JsonException or NotSupportedException)
        {
            // Type and message only: no stack trace in the log, nothing from the exception on the page.
            logger.Log(outage, "Auth service unavailable: {ExceptionType}: {ExceptionMessage}", ex.GetType().Name, ex.Message);
            return new ApiResult<T>(false, "unavailable", "The service is not available.", default);
        }
    }

    private ApiResult<T> Unavailable<T>(string reason, int status, LogLevel outage)
    {
        logger.Log(outage, "Auth service unavailable: {Reason} (HTTP {StatusCode})", reason, status);
        return new ApiResult<T>(false, "unavailable", "The service is not available.", default);
    }
}
