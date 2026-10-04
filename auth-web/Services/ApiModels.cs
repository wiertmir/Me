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

public record SignupResponse(User User, bool VerificationRequired);

public record ExchangeResponse(string SessionToken, User User, string? Challenge);

public record Identity(string Provider, string? Email, DateTimeOffset CreatedAt);

public record SessionInfo(Guid Id, DateTimeOffset CreatedAt, DateTimeOffset LastSeen, string UserAgent, string Ip, bool Current);

public record AppPasswordInfo(Guid Id, string Label, DateTimeOffset CreatedAt, DateTimeOffset? LastUsed);

public record CreatedAppPassword(Guid Id, string Label, string Password, DateTimeOffset CreatedAt);

public record CreateUserResponse(User User, string TemporaryPassword);

public record ResetPasswordResponse(string TemporaryPassword);

public record LinkIntentResponse(string StartUrl);
