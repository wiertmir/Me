using AuthWeb.Infrastructure;
using Serilog;
using Serilog.Events;
using Serilog.Formatting.Json;
using Serilog.Sinks.SystemConsole.Themes;

// Serilog's "Code" theme with one colour per level (the stock one shows debug and information both in white).
var consoleTheme = new AnsiConsoleTheme(new Dictionary<ConsoleThemeStyle, string>
{
    [ConsoleThemeStyle.Text] = "\x1b[38;5;0253m",
    [ConsoleThemeStyle.SecondaryText] = "\x1b[38;5;0246m",
    [ConsoleThemeStyle.TertiaryText] = "\x1b[38;5;0242m",
    [ConsoleThemeStyle.Invalid] = "\x1b[33;1m",
    [ConsoleThemeStyle.Null] = "\x1b[38;5;0038m",
    [ConsoleThemeStyle.Name] = "\x1b[38;5;0081m",
    [ConsoleThemeStyle.String] = "\x1b[38;5;0216m",
    [ConsoleThemeStyle.Number] = "\x1b[38;5;151m",
    [ConsoleThemeStyle.Boolean] = "\x1b[38;5;0038m",
    [ConsoleThemeStyle.Scalar] = "\x1b[38;5;0079m",
    [ConsoleThemeStyle.LevelVerbose] = "\x1b[38;5;0242m",                  // grey
    [ConsoleThemeStyle.LevelDebug] = "\x1b[38;5;0075m",                    // blue
    [ConsoleThemeStyle.LevelInformation] = "\x1b[38;5;0078m",              // green
    [ConsoleThemeStyle.LevelWarning] = "\x1b[38;5;0229m",                  // yellow
    [ConsoleThemeStyle.LevelError] = "\x1b[38;5;0197m\x1b[48;5;0238m",     // red on grey
    [ConsoleThemeStyle.LevelFatal] = "\x1b[38;5;0197m\x1b[48;5;0238m",
});

var builder = WebApplication.CreateBuilder(args);
// Lets a build that is run outside Development (the Aspire app host does that) find the framework's
// scripts; does nothing in a published copy, which has them in wwwroot.
builder.WebHost.UseStaticWebAssets();
// Serilog is the only logging provider. LOG_FORMAT=json selects JSON lines; anything else the coloured console.
builder.Services.AddSerilog(log =>
{
    log.MinimumLevel.Debug()
        // Below Warning the framework and HttpClient loggers print request URLs, which hold one-time tokens.
        .MinimumLevel.Override("Microsoft.AspNetCore", LogEventLevel.Warning)
        .MinimumLevel.Override("System.Net.Http", LogEventLevel.Warning)
        // The HttpClient factory reports its handler clean-up cycle at debug every ten seconds.
        .MinimumLevel.Override("Microsoft.Extensions.Http", LogEventLevel.Information)
        .Enrich.FromLogContext();
    // Local time: JSON lines carry the UTC offset, the console format leaves it out.
    if (Environment.GetEnvironmentVariable("LOG_FORMAT") == "json")
        log.WriteTo.Console(new JsonFormatter(renderMessage: true));
    else
        log.WriteTo.Console(
            outputTemplate: "{Timestamp:yyyy-MM-dd HH:mm:ss.fff} [{Level:u3}] [{RequestId}] {SourceContext}: {Message:lj}{NewLine}{Exception}",
            theme: consoleTheme);
});
builder.AddAuthWeb();

var app = builder.Build();
app.UseAuthWeb();
app.Run();
