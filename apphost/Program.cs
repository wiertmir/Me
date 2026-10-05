// .NET Aspire app host: runs the home-network setup (the N-run-*.sh scripts) as resources of one
// dashboard, with their state, console logs, traces and start/stop:   dotnet run --project apphost
// Same .env, same auth-service/, calendar-service/ and tasks-service/config.local.toml, same Caddyfile and data directories as the scripts,
// so stop those (and the Kubernetes cluster) first.
var builder = DistributedApplication.CreateBuilder(args);
var root = Path.GetFullPath(Path.Combine(builder.AppHostDirectory, ".."));

var env = File.ReadLines(Path.Combine(root, ".env"))
    .Where(l => !l.StartsWith('#') && l.Contains('='))
    .Select(l => l.Split('=', 2))
    .ToDictionary(p => p[0].Trim(), p => p[1].Trim());
// Also log to ./logs, a file per day, as the run scripts do, unless .env names another directory.
foreach (var name in new[] { "ME_AUTH__LOG__DIR", "ME_CALENDAR__LOG__DIR", "ME_TASKS__LOG__DIR", "LOG_DIR" })
    env.TryAdd(name, Path.Combine(root, "logs"));

// Fixed ports and no Aspire proxy in front: the Caddyfile and the configurations name these addresses.
var authService = WithDotEnv(builder
    .AddExecutable("auth-service", "cargo", root,
        "run", "--release", "-p", "auth-service", "--", "auth-service/config.local.toml")
    .WithHttpEndpoint(port: 8081, targetPort: 8081, isProxied: false)
    .WithHttpHealthCheck("/health")
    .WithOtlpExporter());

var calendarService = WithDotEnv(builder
    .AddExecutable("calendar-service", "cargo", root,
        "run", "--release", "-p", "calendar-service", "--", "calendar-service/config.local.toml")
    .WithHttpEndpoint(port: 8083, targetPort: 8083, isProxied: false)
    .WithHttpHealthCheck("/health")
    .WithOtlpExporter()
    .WaitFor(authService));

var tasksService = WithDotEnv(builder
    .AddExecutable("tasks-service", "cargo", root,
        "run", "--release", "-p", "tasks-service", "--", "tasks-service/config.local.toml")
    .WithHttpEndpoint(port: 8084, targetPort: 8084, isProxied: false)
    .WithHttpHealthCheck("/health")
    .WithOtlpExporter()
    .WaitFor(authService));

// No launch profile: that one is the development setup; .env makes this Production on 127.0.0.1:5080.
var authWeb = WithDotEnv(builder
    .AddProject<Projects.AuthWeb>("auth-web", o => o.ExcludeLaunchProfile = true)
    .WithHttpEndpoint(port: 5080, targetPort: 5080, isProxied: false)
    .WaitFor(authService));

WithDotEnv(builder
    .AddExecutable("caddy", "caddy", root, "run")
    .WithHttpsEndpoint(port: 443, targetPort: 443, isProxied: false)
    .WithUrlForEndpoint("https", u => u.Url = $"https://{env["ME_HOST"]}")
    .WaitFor(authWeb)
    .WaitFor(authService)
    .WaitFor(calendarService)
    .WaitFor(tasksService));

builder.Build().Run();

IResourceBuilder<T> WithDotEnv<T>(IResourceBuilder<T> resource) where T : IResourceWithEnvironment
{
    foreach (var (name, value) in env)
        resource.WithEnvironment(name, value);
    return resource;
}
