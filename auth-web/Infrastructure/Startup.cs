using AuthWeb.Services;
using Microsoft.AspNetCore.Authentication.Cookies;
using Microsoft.AspNetCore.Components.Server.Circuits;
using Microsoft.AspNetCore.DataProtection;
using Microsoft.AspNetCore.HttpOverrides;

namespace AuthWeb.Infrastructure;

public static class Startup
{
    public static void AddAuthWeb(this WebApplicationBuilder builder)
    {
        var dev = builder.Environment.IsDevelopment();
        var section = builder.Configuration.GetSection("AuthService");
        var options = section.Get<AuthServiceOptions>() ?? new AuthServiceOptions();
        if (!dev && options.Secret.Length < 16)
            throw new InvalidOperationException("AuthService:Secret must be set to at least 16 characters.");
        if (options.BaseUrl.Length == 0)
            throw new InvalidOperationException("AuthService:BaseUrl must be set.");

        var services = builder.Services;
        services.Configure<AuthServiceOptions>(section);
        services.AddRazorComponents().AddInteractiveServerComponents();
        services.AddCascadingAuthenticationState();
        services.AddHttpContextAccessor();

        services.AddDataProtection()
            .SetApplicationName("me-auth-web")
            .PersistKeysToFileSystem(new DirectoryInfo(builder.Configuration["DataProtection:Path"] ?? "./data/dp-keys"));

        var secure = dev ? CookieSecurePolicy.SameAsRequest : CookieSecurePolicy.Always;
        services.AddAuthentication(CookieAuthenticationDefaults.AuthenticationScheme)
            .AddCookie(o =>
            {
                o.Cookie.Name = "me_auth";
                o.Cookie.HttpOnly = true;
                o.Cookie.SecurePolicy = secure;
                o.Cookie.SameSite = SameSiteMode.Lax;
                o.ExpireTimeSpan = TimeSpan.FromDays(30);
                o.SlidingExpiration = false; // the service's session is what expires
                o.LoginPath = "/signin";
                o.ReturnUrlParameter = "returnUrl";
                // Signed in but not allowed (e.g. /admin/users for a non-admin): a real 403, which the status-code
                // page turns into "Not authorised", not a redirect to a login-style page.
                o.Events.OnRedirectToAccessDenied = ctx =>
                {
                    ctx.Response.StatusCode = StatusCodes.Status403Forbidden;
                    return Task.CompletedTask;
                };
            });
        services.AddAuthorization();
        services.AddAntiforgery(o => o.Cookie.SecurePolicy = secure);

        services.Configure<ForwardedHeadersOptions>(o =>
        {
            o.ForwardedHeaders = ForwardedHeaders.XForwardedFor | ForwardedHeaders.XForwardedProto;
            // Known proxies/networks stay at their defaults: loopback only.
        });

        services.AddScoped<RequestContext>();
        services.AddScoped<CircuitHandler, ClientCircuitHandler>();
        services.AddTransient<ClientHeadersHandler>();
        services.AddHttpClient<AuthApi>(c =>
            {
                c.BaseAddress = new Uri(options.BaseUrl);
                c.Timeout = TimeSpan.FromSeconds(10);
            })
            .AddHttpMessageHandler<ClientHeadersHandler>();
    }

    public static void UseAuthWeb(this WebApplication app)
    {
        app.UseForwardedHeaders();
        app.UseMiddleware<RequestIdMiddleware>();
        app.UseMiddleware<SecurityHeadersMiddleware>();
        if (!app.Environment.IsDevelopment())
        {
            app.UseExceptionHandler("/Error", createScopeForErrors: true);
            app.UseHsts();
            app.UseHttpsRedirection();
        }
        app.UseStatusCodePagesWithReExecute("/not-found", createScopeForStatusCodePages: true);
        app.UseAuthentication();
        app.UseMiddleware<MustChangeMiddleware>();
        app.UseAuthorization();
        app.UseAntiforgery();
        app.MapStaticAssets();
        app.MapGet("/", () => Results.Redirect("/account"));
        app.MapRazorComponents<Components.App>().AddInteractiveServerRenderMode();
    }
}
