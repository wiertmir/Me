mod support;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use auth_service::{
    config::{ProviderConfig, SignupMode},
    crypto, social_store, users,
};
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use serde_json::{Value, json};
use sha2::Digest;
use support::TestApp;
use uuid::Uuid;

const WEB: &str = "http://localhost:5080";
const PASSWORD: &str = "correct horse battery";

#[derive(Default)]
struct StubState {
    profile: Value,
    emails: Value,
    challenge: Option<String>,
    deny: bool,
    accepts: Vec<String>,
}
type Shared = Arc<Mutex<StubState>>;

/// A fake identity provider: authorize, token (checks PKCE), userinfo and GitHub-style emails.
struct Stub {
    base: String,
    state: Shared,
}

impl Stub {
    async fn spawn() -> Self {
        let state: Shared = Default::default();
        let router = Router::new()
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .route("/userinfo", get(userinfo))
            .route("/emails", get(emails))
            .with_state(state.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        Stub { base, state }
    }
    fn profile(&self, v: Value) {
        self.state.lock().unwrap().profile = v;
    }
    fn emails(&self, v: Value) {
        self.state.lock().unwrap().emails = v;
    }
    fn deny(&self) {
        self.state.lock().unwrap().deny = true;
    }
    fn config(&self) -> ProviderConfig {
        ProviderConfig {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            auth_url: Some(format!("{}/authorize", self.base)),
            token_url: Some(format!("{}/token", self.base)),
            userinfo_url: Some(format!("{}/userinfo", self.base)),
            emails_url: Some(format!("{}/emails", self.base)),
        }
    }
}

async fn authorize(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "cid");
    assert_eq!(q["code_challenge_method"], "S256");
    let mut s = s.lock().unwrap();
    s.challenge = Some(q["code_challenge"].clone());
    let mut to = url::Url::parse(&q["redirect_uri"]).unwrap();
    if s.deny {
        to.query_pairs_mut().append_pair("error", "access_denied");
    } else {
        to.query_pairs_mut().append_pair("code", "stub-code");
    }
    to.query_pairs_mut().append_pair("state", &q["state"]);
    Redirect::to(to.as_str())
}

async fn token(
    State(s): State<Shared>,
    Form(f): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let want = s.lock().unwrap().challenge.clone().unwrap_or_default();
    let got = B64.encode(sha2::Sha256::digest(
        f.get("code_verifier")
            .cloned()
            .unwrap_or_default()
            .as_bytes(),
    ));
    if f["code"] != "stub-code" || f["client_secret"] != "csecret" || got != want {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"access_token": "stub-token", "token_type": "bearer"})),
    )
}

fn authed(h: &HeaderMap) -> bool {
    h.get(header::AUTHORIZATION)
        .is_some_and(|v| v == "Bearer stub-token")
}

fn note_accept(s: &Shared, h: &HeaderMap) {
    let a = h
        .get(header::ACCEPT)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    s.lock().unwrap().accepts.push(a);
}

async fn userinfo(State(s): State<Shared>, h: HeaderMap) -> impl IntoResponse {
    note_accept(&s, &h);
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, Json(Value::Null));
    }
    (StatusCode::OK, Json(s.lock().unwrap().profile.clone()))
}

async fn emails(State(s): State<Shared>, h: HeaderMap) -> impl IntoResponse {
    note_accept(&s, &h);
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, Json(Value::Null));
    }
    (StatusCode::OK, Json(s.lock().unwrap().emails.clone()))
}

async fn setup(providers: &[&str]) -> (TestApp, Stub) {
    setup_with(providers, |_| {}).await
}

async fn setup_with(
    providers: &[&str],
    f: impl FnOnce(&mut auth_service::Config),
) -> (TestApp, Stub) {
    let stub = Stub::spawn().await;
    let app = TestApp::spawn_with(|cfg| {
        cfg.web_url = WEB.into();
        for p in providers {
            cfg.providers.insert(p.to_string(), stub.config());
        }
        f(cfg);
    })
    .await;
    (app, stub)
}

fn google(sub: &str, email: Option<&str>, verified: bool) -> Value {
    json!({"sub": sub, "email": email, "email_verified": verified, "name": "Some Person"})
}

fn loc(r: &reqwest::Response) -> String {
    r.headers()[LOCATION].to_str().unwrap().to_string()
}

fn query(u: &str, key: &str) -> Option<String> {
    url::Url::parse(u)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

/// Hits `/social/{provider}/start{q}`; returns the response and the `me_social_state=…` cookie pair.
async fn start(app: &TestApp, provider: &str, q: &str) -> (reqwest::Response, Option<String>) {
    let r = app
        .http
        .get(format!("{}/social/{provider}/start{q}", app.base))
        .send()
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get(SET_COOKIE)
        .map(|c| c.to_str().unwrap().split(';').next().unwrap().to_string());
    (r, cookie)
}

/// Start, then follow the stub's authorize redirect; returns the callback URL and the cookie.
async fn begin(app: &TestApp, provider: &str, q: &str) -> (String, String) {
    let (r, cookie) = start(app, provider, q).await;
    assert_eq!(r.status(), StatusCode::FOUND, "{:?}", r.headers());
    let r2 = app.http.get(loc(&r)).send().await.unwrap();
    assert!(r2.status().is_redirection());
    (loc(&r2), cookie.expect("state cookie"))
}

async fn callback(app: &TestApp, url: &str, cookie: Option<&str>) -> reqwest::Response {
    let mut req = app.http.get(url);
    if let Some(c) = cookie {
        req = req.header(COOKIE, c);
    }
    req.send().await.unwrap()
}

/// The whole browser journey with the cookie carried; returns the callback response.
async fn social(app: &TestApp, provider: &str, q: &str) -> reqwest::Response {
    let (url, cookie) = begin(app, provider, q).await;
    callback(app, &url, Some(&cookie)).await
}

fn ticket_of(r: &reqwest::Response) -> String {
    let l = loc(r);
    assert!(
        l.starts_with(&format!("{WEB}/social/complete?ticket=")),
        "{l}"
    );
    query(&l, "ticket").unwrap()
}

async fn exchange(app: &TestApp, ticket: &str) -> (StatusCode, Value) {
    app.api(
        axum::http::Method::POST,
        "/api/social/exchange",
        None,
        json!({"ticket": ticket}),
    )
    .await
}

/// Full social sign-in that must succeed; returns the exchange body.
async fn signin(app: &TestApp, provider: &str) -> Value {
    let r = social(app, provider, "").await;
    let (s, b) = exchange(app, &ticket_of(&r)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn link_ticket_of(r: &reqwest::Response) -> String {
    let l = loc(r);
    assert!(
        l.starts_with(&format!("{WEB}/account/security?link_ticket=")),
        "{l}"
    );
    query(&l, "link_ticket").unwrap()
}

async fn confirm(app: &TestApp, session: &str, ticket: &str) -> (StatusCode, Value) {
    app.api(
        axum::http::Method::POST,
        "/api/me/identities/confirm",
        Some(session),
        json!({"ticket": ticket}),
    )
    .await
}

/// Link intent for `session`'s user, then the whole browser journey; returns the callback response.
async fn link_flow(app: &TestApp, session: &str, provider: &str) -> reqwest::Response {
    let (s, b) = app
        .api(
            axum::http::Method::POST,
            "/api/social/link-intent",
            Some(session),
            json!({"provider": provider}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let u = b["start_url"].as_str().unwrap();
    social(app, provider, &u[u.find('?').unwrap()..]).await
}

fn assert_redirect(r: &reqwest::Response, path: &str) {
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(loc(r), format!("{WEB}{path}"));
}

fn make_user(app: &TestApp, name: &str, email: &str, verified: bool, password: bool) -> Uuid {
    users::create(
        &app.state.db,
        users::NewUser {
            username: name.into(),
            email: email.into(),
            email_verified: verified,
            is_admin: false,
            must_change_password: false,
            password_hash: password.then(|| crypto::hash_password(PASSWORD)),
        },
    )
    .unwrap()
    .id
}

async fn password_session(app: &TestApp, login: &str) -> String {
    let (s, b) = app
        .api(
            axum::http::Method::POST,
            "/api/signin",
            None,
            json!({"login": login, "password": PASSWORD}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["session_token"].as_str().unwrap().to_string()
}

async fn identities(app: &TestApp, session: &str) -> Value {
    let (s, b) = app
        .api(
            axum::http::Method::GET,
            "/api/me/identities",
            Some(session),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn link(app: &TestApp, user: Uuid, provider: &str, subject: &str) {
    assert!(social_store::insert_identity(&app.state.db, user, provider, subject, None).unwrap());
}

#[tokio::test]
async fn new_identity_creates_account_when_signup_open() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("New@Example.org"), true));
    let b = signin(&app, "google").await;
    assert_eq!(b["user"]["has_password"], false);
    assert_eq!(b["user"]["email"], "new@example.org");
    assert_eq!(b["user"]["email_verified"], true);
    assert_eq!(b["user"]["username"], "new");
    assert_eq!(b["user"]["display_name"], "Some Person");
    assert!(b["challenge"].is_null());
    assert!(b["session_token"].as_str().unwrap().len() > 20);
}

#[tokio::test]
async fn new_identity_refused_when_signup_disabled() {
    let (app, stub) = setup_with(&["google"], |c| c.signup = SignupMode::Disabled).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=signup_disabled",
    );
}

#[tokio::test]
async fn linked_identity_signs_in_existing_user() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", false, true);
    link(&app, id, "google", "g1");
    stub.profile(google("g1", Some("other@example.org"), false));
    let b = signin(&app, "google").await;
    assert_eq!(b["user"]["id"], id.to_string());
}

#[tokio::test]
async fn verified_email_match_links_automatically() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    stub.profile(google("g1", Some("alice@example.org"), true));
    let b = signin(&app, "google").await;
    assert_eq!(b["user"]["id"], id.to_string());
    let list = identities(&app, b["session_token"].as_str().unwrap()).await;
    assert_eq!(list[0]["provider"], "google");
    assert_eq!(list[0]["email"], "alice@example.org");
    assert!(list[0]["created_at"].is_string());
}

#[tokio::test]
async fn unverified_local_email_does_not_link() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", false, true);
    stub.profile(google("g1", Some("alice@example.org"), true));
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=account_exists",
    );
    assert!(
        social_store::list_identities(&app.state.db, id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unverified_provider_email_does_not_link() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    stub.profile(google("g1", Some("alice@example.org"), false));
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=account_exists",
    );
    assert!(
        social_store::list_identities(&app.state.db, id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn microsoft_email_never_auto_links() {
    let (app, stub) = setup(&["microsoft"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    stub.profile(
        json!({"sub": "m1", "email": "alice@example.org", "email_verified": true, "name": "A"}),
    );
    assert_redirect(
        &social(&app, "microsoft", "").await,
        "/signin?error=account_exists",
    );
    assert!(
        social_store::list_identities(&app.state.db, id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn github_uses_primary_verified_email_and_numeric_id() {
    let (app, stub) = setup(&["github"]).await;
    stub.profile(json!({"id": 4242, "login": "octo", "name": null}));
    stub.emails(json!([
        {"email": "other@example.org", "primary": false, "verified": true},
        {"email": "Octo@Example.org", "primary": true, "verified": true}
    ]));
    let b = signin(&app, "github").await;
    assert_eq!(b["user"]["email"], "octo@example.org");
    assert_eq!(b["user"]["email_verified"], true);
    let id: Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        social_store::find_identity(&app.state.db, "github", "4242").unwrap(),
        Some(id)
    );
}

#[tokio::test]
async fn github_unverified_primary_does_not_link() {
    let (app, stub) = setup(&["github"]).await;
    make_user(&app, "alice", "alice@example.org", true, true);
    stub.profile(json!({"id": 7, "login": "octo"}));
    stub.emails(json!([{"email": "alice@example.org", "primary": true, "verified": false}]));
    assert_redirect(
        &social(&app, "github", "").await,
        "/signin?error=account_exists",
    );
}

#[tokio::test]
async fn link_intent_attaches_identity_to_signed_in_user() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    let session = password_session(&app, "alice").await;
    stub.profile(google("g9", Some("elsewhere@example.org"), true));
    let (s, b) = app
        .api(
            axum::http::Method::POST,
            "/api/social/link-intent",
            Some(&session),
            json!({"provider": "google"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let start_url = b["start_url"].as_str().unwrap().to_string();
    assert!(start_url.starts_with(&format!("{}/social/google/start?link=", app.base)));
    let q = &start_url[start_url.find('?').unwrap()..];
    let r = social(&app, "google", q).await;
    let ticket = link_ticket_of(&r);
    // Nothing is attached until the session confirms.
    assert!(
        identities(&app, &session)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        confirm(&app, &session, &ticket).await.0,
        StatusCode::NO_CONTENT
    );
    let (s, b) = confirm(&app, &session, &ticket).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_ticket"))
    );
    assert_eq!(
        identities(&app, &session).await.as_array().unwrap().len(),
        1
    );
    assert_eq!(
        social_store::find_identity(&app.state.db, "google", "g9").unwrap(),
        Some(id)
    );
    // The intent is single use.
    let (r, _) = start(&app, "google", q).await;
    assert_redirect(&r, "/account/security?error=social_failed");
}

#[tokio::test]
async fn link_to_identity_owned_by_another_user_is_refused() {
    let (app, stub) = setup(&["google"]).await;
    let alice = make_user(&app, "alice", "alice@example.org", true, true);
    make_user(&app, "bob", "bob@example.org", true, true);
    link(&app, alice, "google", "g1");
    let session = password_session(&app, "bob").await;
    stub.profile(google("g1", Some("bob@example.org"), true));
    let (_, b) = app
        .api(
            axum::http::Method::POST,
            "/api/social/link-intent",
            Some(&session),
            json!({"provider": "google"}),
        )
        .await;
    let start_url = b["start_url"].as_str().unwrap();
    let r = social(&app, "google", &start_url[start_url.find('?').unwrap()..]).await;
    assert_redirect(&r, "/account/security?error=identity_in_use");
    assert!(
        identities(&app, &session)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cannot_unlink_last_sign_in_method() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let b = signin(&app, "google").await;
    let session = b["session_token"].as_str().unwrap();
    let (s, b) = app
        .api(
            axum::http::Method::DELETE,
            "/api/me/identities/google",
            Some(session),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["code"], "last_sign_in_method");
}

#[tokio::test]
async fn unlink_removes_identity_and_second_unlink_is_404() {
    let (app, _stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    link(&app, id, "google", "g1");
    let session = password_session(&app, "alice").await;
    assert_eq!(
        identities(&app, &session).await.as_array().unwrap().len(),
        1
    );
    let del = || {
        app.api(
            axum::http::Method::DELETE,
            "/api/me/identities/google",
            Some(&session),
            Value::Null,
        )
    };
    assert_eq!(del().await.0, StatusCode::NO_CONTENT);
    assert!(
        identities(&app, &session)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (s, b) = del().await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["code"], "not_found");
}

#[tokio::test]
async fn state_without_or_with_wrong_cookie_is_rejected() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let (url, cookie) = begin(&app, "google", "").await;
    assert_redirect(
        &callback(&app, &url, None).await,
        "/signin?error=social_failed",
    );
    // The state was consumed by the failed attempt, so even the right cookie cannot revive it.
    assert_redirect(
        &callback(&app, &url, Some(&cookie)).await,
        "/signin?error=social_failed",
    );
    let (url, _) = begin(&app, "google", "").await;
    assert_redirect(
        &callback(&app, &url, Some("me_social_state=something-else")).await,
        "/signin?error=social_failed",
    );
    assert!(
        social_store::list_identities(&app.state.db, Uuid::new_v4())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn state_replay_is_rejected() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let (url, cookie) = begin(&app, "google", "").await;
    let first = callback(&app, &url, Some(&cookie)).await;
    ticket_of(&first);
    assert_redirect(
        &callback(&app, &url, Some(&cookie)).await,
        "/signin?error=social_failed",
    );
}

#[tokio::test]
async fn ticket_replay_and_garbage_are_rejected() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let t = ticket_of(&social(&app, "google", "").await);
    assert_eq!(exchange(&app, &t).await.0, StatusCode::OK);
    let (s, b) = exchange(&app, &t).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_ticket"))
    );
    assert_eq!(exchange(&app, "nope").await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn callback_cookie_is_cleared_and_state_is_bound_to_provider() {
    let (app, stub) = setup(&["google", "github"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let (url, cookie) = begin(&app, "google", "").await;
    let r = callback(&app, &url, Some(&cookie)).await;
    let set = r.headers()[SET_COOKIE].to_str().unwrap();
    assert!(
        set.starts_with("me_social_state=;") && set.contains("Max-Age=0"),
        "{set}"
    );
    // A google state presented at the github callback fails.
    let (url, cookie) = begin(&app, "google", "").await;
    let other = url.replace("/social/google/", "/social/github/");
    assert_redirect(
        &callback(&app, &other, Some(&cookie)).await,
        "/signin?error=social_failed",
    );
}

#[tokio::test]
async fn provider_error_redirects_to_social_failed() {
    let (app, stub) = setup(&["google"]).await;
    stub.deny();
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=social_failed",
    );
}

#[tokio::test]
async fn disabled_user_is_not_signed_in() {
    let (app, stub) = setup(&["google"]).await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    link(&app, id, "google", "g1");
    app.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE users SET disabled = 1 WHERE id = ?1",
                [id.to_string()],
            )
        })
        .unwrap();
    stub.profile(google("g1", Some("alice@example.org"), true));
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=account_disabled",
    );
}

#[tokio::test]
async fn profile_without_email_cannot_sign_up() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", None, false));
    assert_redirect(
        &social(&app, "google", "").await,
        "/signin?error=email_required",
    );
}

#[tokio::test]
async fn generated_usernames_are_deduplicated_and_valid() {
    let (app, stub) = setup(&["google"]).await;
    let mut names = vec![];
    for (i, email) in ["dup@a.org", "dup@b.org", "x@c.org", "J.Doe+tag!@d.org"]
        .iter()
        .enumerate()
    {
        stub.profile(google(&format!("g{i}"), Some(email), true));
        names.push(
            signin(&app, "google").await["user"]["username"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(names, ["dup", "dup1", "x00", "j.doetag"]);
}

#[tokio::test]
async fn exchange_returns_the_challenge_given_at_start() {
    let (app, stub) = setup(&["google"]).await;
    stub.profile(google("g1", Some("n@example.org"), true));
    let r = social(&app, "google", "?challenge=abc123").await;
    let (s, b) = exchange(&app, &ticket_of(&r)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["challenge"], "abc123");
}

#[tokio::test]
async fn unconfigured_or_unknown_provider_is_unavailable() {
    let (app, _stub) = setup(&["google"]).await;
    for p in ["github", "facebook"] {
        let (r, cookie) = start(&app, p, "").await;
        assert_redirect(&r, "/signin?error=provider_unavailable");
        assert!(cookie.is_none());
    }
}

#[tokio::test]
async fn providers_lists_only_configured_in_fixed_order() {
    let (app, _stub) = setup(&["microsoft", "google"]).await;
    let (s, b) = app
        .api(axum::http::Method::GET, "/api/providers", None, Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, json!(["google", "microsoft"]));
}

#[tokio::test]
async fn state_cookie_attributes() {
    let (app, _stub) = setup(&["google"]).await;
    let (r, _) = start(&app, "google", "").await;
    let c = r.headers()[SET_COOKIE].to_str().unwrap();
    for part in [
        "me_social_state=",
        "HttpOnly",
        "SameSite=Lax",
        "Path=/social",
        "Max-Age=600",
    ] {
        assert!(c.contains(part), "{c}");
    }
    assert!(!c.contains("Secure"), "{c}");
    let (app, _stub) = setup_with(&["google"], |c| c.issuer = "https://auth.example".into()).await;
    let (r, _) = start(&app, "google", "").await;
    assert!(r.headers()[SET_COOKIE].to_str().unwrap().contains("Secure"));
}

#[tokio::test]
async fn start_with_both_challenge_and_link_fails() {
    let (app, _stub) = setup(&["google"]).await;
    let (r, _) = start(&app, "google", "?challenge=a&link=b").await;
    assert_redirect(&r, "/signin?error=social_failed");
}

#[tokio::test]
async fn link_ticket_cannot_be_confirmed_by_another_user() {
    let (app, stub) = setup(&["google"]).await;
    let a = make_user(&app, "alice", "alice@example.org", true, true);
    let b = make_user(&app, "bob", "bob@example.org", true, true);
    let (sa, sb) = (
        password_session(&app, "alice").await,
        password_session(&app, "bob").await,
    );
    stub.profile(google("victim", Some("victim@example.org"), true));
    let ticket = link_ticket_of(&link_flow(&app, &sa, "google").await);
    let (s, body) = confirm(&app, &sb, &ticket).await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    // The ticket is burned: the rightful owner cannot use it either.
    assert_eq!(confirm(&app, &sa, &ticket).await.0, StatusCode::BAD_REQUEST);
    assert!(
        social_store::list_identities(&app.state.db, a)
            .unwrap()
            .is_empty()
    );
    assert!(
        social_store::list_identities(&app.state.db, b)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn confirm_with_identity_taken_meanwhile_is_a_conflict() {
    let (app, stub) = setup(&["google"]).await;
    let a = make_user(&app, "alice", "alice@example.org", true, true);
    let b = make_user(&app, "bob", "bob@example.org", true, true);
    let sa = password_session(&app, "alice").await;
    stub.profile(google("g5", Some("x@example.org"), true));
    let ticket = link_ticket_of(&link_flow(&app, &sa, "google").await);
    link(&app, b, "google", "g5");
    let (s, body) = confirm(&app, &sa, &ticket).await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("identity_in_use"))
    );
    assert!(
        social_store::list_identities(&app.state.db, a)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn accept_header_is_github_specific() {
    let (app, stub) = setup(&["google", "github", "microsoft"]).await;
    stub.profile(google("g1", Some("g@example.org"), true));
    signin(&app, "google").await;
    stub.profile(json!({"sub": "m1", "email": "m@example.org", "name": "M"}));
    signin(&app, "microsoft").await;
    assert!(
        stub.state
            .lock()
            .unwrap()
            .accepts
            .iter()
            .all(|a| a == "application/json")
    );
    stub.state.lock().unwrap().accepts.clear();
    stub.profile(json!({"id": 1, "login": "octo"}));
    stub.emails(json!([{"email": "o@example.org", "primary": true, "verified": true}]));
    signin(&app, "github").await;
    let accepts = stub.state.lock().unwrap().accepts.clone();
    assert_eq!(accepts.len(), 2);
    assert!(accepts.iter().all(|a| a == "application/vnd.github+json"));
}

async fn forgot_and_reset(app: &TestApp, email: &str, new_password: &str) {
    let (s, _) = app
        .api(
            axum::http::Method::POST,
            "/api/password/forgot",
            None,
            json!({"email": email}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = app
        .api(
            axum::http::Method::POST,
            "/api/password/reset",
            None,
            json!({"token": app.last_mail_token(), "new_password": new_password}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
}

#[tokio::test]
async fn reset_purges_identities_of_pre_hijacked_account() {
    let app_stub = Stub::spawn().await;
    let app = TestApp::spawn_with_mail_and(|cfg| {
        cfg.web_url = WEB.into();
        cfg.providers.insert("microsoft".into(), app_stub.config());
    })
    .await;
    let stub = app_stub;
    // Attacker signs up through Microsoft with the victim's address (unverified).
    stub.profile(json!({"sub": "evil", "email": "victim@example.org", "name": "Mallory"}));
    let b = signin(&app, "microsoft").await;
    assert_eq!(b["user"]["email_verified"], false);
    let id: Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    // The victim reclaims the account via password reset.
    forgot_and_reset(&app, "victim@example.org", "brand new passphrase").await;
    assert!(
        social_store::list_identities(&app.state.db, id)
            .unwrap()
            .is_empty()
    );
    // The attacker's identity no longer works: the account now has a verified email, Microsoft's is not.
    assert_redirect(
        &social(&app, "microsoft", "").await,
        "/signin?error=account_exists",
    );
    let (s, body) = app
        .api(
            axum::http::Method::POST,
            "/api/signin",
            None,
            json!({"login": "victim@example.org", "password": "brand new passphrase"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn reset_keeps_identities_of_verified_account() {
    let app = TestApp::spawn_with_mail().await;
    let id = make_user(&app, "alice", "alice@example.org", true, true);
    link(&app, id, "google", "g1");
    forgot_and_reset(&app, "alice@example.org", "brand new passphrase").await;
    assert_eq!(
        social_store::list_identities(&app.state.db, id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn admin_reset_purges_identities_only_when_email_unverified() {
    let (app, _stub) = setup(&["google"]).await;
    let admin = app.admin_session().await;
    let unverified = make_user(&app, "unv", "unv@example.org", false, false);
    let verified = make_user(&app, "ver", "ver@example.org", true, true);
    link(&app, unverified, "google", "g1");
    link(&app, verified, "google", "g2");
    for id in [unverified, verified] {
        let (s, b) = app
            .api(
                axum::http::Method::POST,
                &format!("/api/admin/users/{id}/reset-password"),
                Some(&admin),
                Value::Null,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    assert!(
        social_store::list_identities(&app.state.db, unverified)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        social_store::list_identities(&app.state.db, verified)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn reset_revokes_tickets_and_intents_issued_before_it() {
    let stub = Stub::spawn().await;
    let app = TestApp::spawn_with_mail_and(|cfg| {
        cfg.web_url = WEB.into();
        for p in ["google", "microsoft"] {
            cfg.providers.insert(p.into(), stub.config());
        }
    })
    .await;
    stub.profile(json!({"sub": "evil", "email": "victim@example.org", "name": "Mallory"}));
    let b = signin(&app, "microsoft").await;
    let attacker = b["session_token"].as_str().unwrap().to_string();
    // Outstanding: a sign-in ticket, a link intent and a link ticket.
    let ticket = ticket_of(&social(&app, "microsoft", "").await);
    let (_, i) = app
        .api(
            axum::http::Method::POST,
            "/api/social/link-intent",
            Some(&attacker),
            json!({"provider": "google"}),
        )
        .await;
    let intent_q = {
        let u = i["start_url"].as_str().unwrap();
        u[u.find('?').unwrap()..].to_string()
    };
    let link_ticket = link_ticket_of(&link_flow(&app, &attacker, "google").await);

    forgot_and_reset(&app, "victim@example.org", "brand new passphrase").await;

    let (s, body) = exchange(&app, &ticket).await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_ticket"))
    );
    let (r, _) = start(&app, "google", &intent_q).await;
    assert_redirect(&r, "/account/security?error=social_failed");
    let (_, signed) = app
        .api(
            axum::http::Method::POST,
            "/api/signin",
            None,
            json!({"login": "victim@example.org", "password": "brand new passphrase"}),
        )
        .await;
    let victim = signed["session_token"].as_str().unwrap();
    let (s, body) = confirm(&app, victim, &link_ticket).await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_ticket"))
    );
    // The attacker's old session is gone too.
    let (s, _) = app
        .api(
            axum::http::Method::GET,
            "/api/me",
            Some(&attacker),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_verification_mail_for_passwordless_account() {
    let stub = Stub::spawn().await;
    let app = TestApp::spawn_with_mail_and(|cfg| {
        cfg.web_url = WEB.into();
        cfg.providers.insert("microsoft".into(), stub.config());
    })
    .await;
    stub.profile(json!({"sub": "evil", "email": "victim@example.org", "name": "Mallory"}));
    let b = signin(&app, "microsoft").await;
    let id: Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    let (s, _) = app
        .api(
            axum::http::Method::POST,
            "/api/email/resend",
            None,
            json!({"email": "victim@example.org"}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.mail_count(), 0);
    // Defence in depth: even a verify token that somehow exists cannot verify a passwordless account.
    let t = auth_service::email_tokens::create(
        &app.state.db,
        id,
        auth_service::email_tokens::Purpose::Verify,
    )
    .unwrap();
    let (s, body) = app
        .api(
            axum::http::Method::POST,
            "/api/email/verify",
            None,
            json!({"token": t}),
        )
        .await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_token"))
    );
    assert!(!users::get(&app.state.db, id).unwrap().email_verified);
}
