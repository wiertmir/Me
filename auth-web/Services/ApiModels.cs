namespace AuthWeb.Services;

public record ApiResult(bool Ok, string? Code, string? Message)
{
    public static ApiResult Success { get; } = new(true, null, null);
}

public record ApiResult<T>(bool Ok, string? Code, string? Message, T? Value) : ApiResult(Ok, Code, Message);

public record User(
    Guid Id,
    string Username,
    string Email,
    bool EmailVerified,
    string DisplayName,
    bool IsAdmin,
    bool MustChangePassword,
    bool HasPassword,
    bool Disabled,
    DateTimeOffset CreatedAt);

public record SignInResponse(string SessionToken, User User);

public record AcceptResponse(string RedirectTo);

public record ErrorBody(string Code, string Message);
