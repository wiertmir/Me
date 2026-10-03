using AuthWeb.Infrastructure;
using NLog.Web;

var builder = WebApplication.CreateBuilder(args);
builder.Logging.ClearProviders();
builder.Host.UseNLog();
builder.AddAuthWeb();

var app = builder.Build();
app.UseAuthWeb();
app.Run();
