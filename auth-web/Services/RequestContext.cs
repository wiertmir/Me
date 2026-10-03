namespace AuthWeb.Services;

/// <summary>
/// The caller's address, User-Agent and request id, captured from the real HTTP request.
/// In an HTTP request scope it fills itself lazily from <see cref="IHttpContextAccessor"/>. A SignalR circuit has
/// no HttpContext per call, so <see cref="ClientCircuitHandler"/> calls <see cref="CaptureForCircuit"/> once when
/// the circuit opens; later calls reuse the stored values and get a fresh request id each.
/// </summary>
public sealed class RequestContext(IHttpContextAccessor accessor)
{
    public const string ItemKey = "RequestId";

    private bool _captured;
    private bool _perCall;
    private string? _ip, _userAgent, _requestId;

    public string? ClientIp { get { Ensure(); return _ip; } }
    public string? UserAgent { get { Ensure(); return _userAgent; } }

    public string RequestId
    {
        get
        {
            Ensure();
            return _perCall || _requestId is null ? Guid.NewGuid().ToString() : _requestId;
        }
    }

    public void CaptureForCircuit()
    {
        Capture();
        _perCall = true;
    }

    private void Ensure()
    {
        if (!_captured) Capture();
    }

    private void Capture()
    {
        var http = accessor.HttpContext;
        if (http is null) return;
        _captured = true;
        // After ForwardedHeaders processing; never a header copied from the browser's request.
        var ip = http.Connection.RemoteIpAddress;
        _ip = ip is null ? null : (ip.IsIPv4MappedToIPv6 ? ip.MapToIPv4() : ip).ToString();
        var ua = http.Request.Headers.UserAgent.ToString();
        _userAgent = ua.Length > 256 ? ua[..256] : ua;
        _requestId = http.Items[ItemKey] as string;
    }
}

/// <summary>Captures the client address when a circuit opens (the connect request is the last time it is visible).</summary>
public sealed class ClientCircuitHandler(RequestContext context, ILogger<ClientCircuitHandler> logger)
    : Microsoft.AspNetCore.Components.Server.Circuits.CircuitHandler
{
    public override Task OnCircuitOpenedAsync(
        Microsoft.AspNetCore.Components.Server.Circuits.Circuit circuit, CancellationToken cancellationToken)
    {
        context.CaptureForCircuit();
        if (context.ClientIp is null) logger.LogWarning("Circuit opened without a client address");
        return Task.CompletedTask;
    }
}
