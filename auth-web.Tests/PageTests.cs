using System.Net;
using AuthWeb.Components.Pages.Account;
using AuthWeb.Components.Pages.Admin;
using Bunit;
using Bunit.TestDoubles;
using Microsoft.AspNetCore.Components;
using Microsoft.Extensions.DependencyInjection;
using Xunit;

namespace AuthWeb.Tests;

public class PageTests : PageTest
{
    private const string Confirm = "POST /api/me/identities/confirm";

    private IRenderedComponent<Security> SecurityWithTicket()
    {
        Services.GetRequiredService<NavigationManager>().NavigateTo("/account/security?link_ticket=abc123");
        Stub.Reply(Confirm, HttpStatusCode.NoContent, "");
        return Render<Security>();
    }

    [Fact]
    public void LinkTicket_IsNotConfirmedOnRender_OnlyWhenUserPressesLinkAccount()
    {
        var cut = SecurityWithTicket();
        Assert.Equal(0, Stub.Count(Confirm));
        Assert.Contains("Link the account you just signed in to?", cut.Markup);

        Button(cut, "Link account").Click();

        cut.WaitForAssertion(() => Assert.Equal(1, Stub.Count(Confirm)));
        Assert.Contains("\"ticket\":\"abc123\"", Stub.Requests.Single(r => r.Path.EndsWith("/confirm")).Body);
        cut.WaitForAssertion(() => Assert.Contains("Account linked.", cut.Markup));
        AssertLeftTicketUrl();
    }

    [Fact]
    public void LinkTicket_DoubleClick_ConfirmsOnce_AndKeepsTheSuccessMessage()
    {
        var cut = SecurityWithTicket();
        var gate = new TaskCompletionSource();
        Stub.Gate = gate.Task; // hold the first answer so the second click lands while it is pending
        var button = Button(cut, "Link account");

        button.Click();
        button.Click();
        gate.SetResult();

        cut.WaitForAssertion(() => Assert.Contains("Account linked.", cut.Markup));
        Assert.Equal(1, Stub.Count(Confirm));
    }

    [Fact]
    public void LinkTicket_Cancel_CallsNothingAndLeavesTheUrl()
    {
        var cut = SecurityWithTicket();
        Button(cut, "Cancel").Click();
        Assert.Equal(0, Stub.Count(Confirm));
        AssertLeftTicketUrl();
    }

    private void AssertLeftTicketUrl()
    {
        var nav = (BunitNavigationManager)Services.GetRequiredService<NavigationManager>();
        var last = nav.History.First();
        Assert.EndsWith("/account/security", last.Uri);
        Assert.True(last.Options.ReplaceHistoryEntry);
    }

    [Fact]
    public void Unlink_LastSignInMethod_ShowsTheFixedMessage()
    {
        Stub.Reply("GET /api/me/identities", HttpStatusCode.OK, """[{"provider":"github","email":"a@x.test","created_at":"2026-01-01T00:00:00Z"}]""")
            .Reply("DELETE /api/me/identities/github", HttpStatusCode.Conflict, Error("last_sign_in_method"));
        var cut = Render<Security>();

        Button(cut, "Unlink").Click();
        cut.Find("dialog button:last-of-type").Click();

        cut.WaitForAssertion(() => Assert.Contains("You can't remove your only way to sign in. Set a password first.", cut.Find("[role=alert]").TextContent));
    }

    [Fact]
    public void AppPassword_IsShownOnce_NotListed_AndGoneAfterDismiss()
    {
        Stub.Reply("POST /api/me/app-passwords", HttpStatusCode.Created,
            """{"id":"6f1c1c10-0000-4000-8000-000000000001","label":"Phone","password":"s3cret-value-xyz","created_at":"2026-01-01T00:00:00Z"}""");
        var cut = Render<AppPasswords>();
        cut.Find("#label").Change("Phone");
        cut.Find("form").Submit();

        cut.WaitForAssertion(() => Assert.Contains("s3cret-value-xyz", cut.Markup));
        Assert.Contains("You won't be able to see this again.", cut.Markup);
        Assert.DoesNotContain("s3cret-value-xyz", string.Concat(cut.FindAll("table").Select(t => t.OuterHtml)));

        Button(cut, "I've saved it").Click();
        Assert.DoesNotContain("s3cret-value-xyz", cut.Markup);
    }

    [Fact]
    public void ConfirmedAction_ThatCompletesLater_StillRefreshesTheList()
    {
        const string list = "GET /api/me/app-passwords";
        Stub.Reply(list, HttpStatusCode.OK, """[{"id":"6f1c1c10-0000-4000-8000-000000000001","label":"Phone","created_at":"2026-01-01T00:00:00Z","last_used":null}]""")
            .Reply("DELETE /api/me/app-passwords/6f1c1c10-0000-4000-8000-000000000001", HttpStatusCode.NoContent, "");
        var cut = Render<AppPasswords>();
        var gate = new TaskCompletionSource();
        Stub.Gate = gate.Task; // as in the real app: the answer arrives after the click handler has returned

        Button(cut, "Delete").Click();
        cut.Find("dialog button:last-of-type").Click();
        Stub.Reply(list, HttpStatusCode.OK, "[]");
        gate.SetResult();

        cut.WaitForAssertion(() => Assert.Contains("No app passwords yet.", cut.Markup));
        Assert.Contains("Deleted app password Phone.", cut.Markup);
    }

    [Fact]
    public void Unauthorized_NavigatesToSessionExpired_WithForceLoad()
    {
        Stub.Reply("GET /api/me/app-passwords", HttpStatusCode.Unauthorized, Error("unauthorized"));
        Render<AppPasswords>();
        var nav = (BunitNavigationManager)Services.GetRequiredService<NavigationManager>();
        var last = nav.History.First();
        Assert.EndsWith("/session-expired", last.Uri);
        Assert.True(last.Options.ForceLoad);
    }

    [Fact]
    public void Admin_LastAdminConflict_IsShownNextToTheAction()
    {
        var root = UserJson("root", admin: true);
        Stub.Reply("GET /api/admin/users", HttpStatusCode.OK, $"[{root}]");
        var id = System.Text.Json.JsonDocument.Parse(root).RootElement.GetProperty("id").GetString();
        Stub.Reply($"PATCH /api/admin/users/{id}", HttpStatusCode.Conflict, Error("last_admin"));
        var cut = Render<Users>();

        Button(cut, "Remove admin").Click();
        cut.Find("dialog button:last-of-type").Click();

        cut.WaitForAssertion(() =>
            Assert.Contains("You can't remove or disable the last administrator.", cut.Find("td [role=alert]").TextContent));
    }

    [Fact]
    public void Admin_CreateUser_ShowsTemporaryPasswordOnce()
    {
        Stub.Reply("POST /api/admin/users", HttpStatusCode.Created, $$"""{"user":{{UserJson("newbie")}},"temporary_password":"temp-pass-123"}""");
        var cut = Render<Users>();
        cut.Find("#new-username").Change("newbie");
        cut.Find("#new-email").Change("newbie@x.test");
        cut.Find("form").Submit();

        cut.WaitForAssertion(() => Assert.Contains("temp-pass-123", cut.Markup));
        Button(cut, "I've saved it").Click();
        Assert.DoesNotContain("temp-pass-123", cut.Markup);
    }

    [Fact]
    public void FailedListLoad_ShowsRetry_AndRetryReissuesTheCall()
    {
        Stub.Reply("GET /api/me/app-passwords", HttpStatusCode.InternalServerError, Error("internal"));
        var cut = Render<AppPasswords>();
        Assert.Equal(1, Stub.Count("GET /api/me/app-passwords"));

        Stub.Reply("GET /api/me/app-passwords", HttpStatusCode.OK, "[]");
        Button(cut, "Try again").Click();

        cut.WaitForAssertion(() => Assert.Contains("No app passwords yet.", cut.Markup));
        Assert.Equal(2, Stub.Count("GET /api/me/app-passwords"));
    }
}
