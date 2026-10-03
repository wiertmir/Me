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

    private async Task<ApiResult<T>> Send<T>(HttpMethod method, string path, string? session, object? body)
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
                var value = response.Content.Headers.ContentLength == 0 || typeof(T) == typeof(object)
                    ? default
                    : await response.Content.ReadFromJsonAsync<T>(Json);
                return new ApiResult<T>(true, null, null, value);
            }
            ErrorBody? error = null;
            try { error = await response.Content.ReadFromJsonAsync<ErrorBody>(Json); }
            catch (JsonException) { }
            return new ApiResult<T>(false, error?.Code ?? "error", error?.Message ?? "Unexpected response from the service.", default);
        }
        catch (Exception ex) when (ex is HttpRequestException or TaskCanceledException or JsonException)
        {
            // Type and message only: no stack trace in the log, nothing from the exception on the page.
            logger.LogError("Auth service unavailable: {ExceptionType}: {ExceptionMessage}", ex.GetType().Name, ex.Message);
            return new ApiResult<T>(false, "unavailable", "The service is not available.", default);
        }
    }
}
