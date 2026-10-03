mod support;
use auth_service::{tokens::Signer, users};
use axum::http::{Method, StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use reqwest::header::{CACHE_CONTROL, LOCATION, PRAGMA, WWW_AUTHENTICATE};
use serde_json::{Value, json};
use support::{TestApp, pkce};
use uuid::Uuid;

const CB: &str = "http://127.0.0.1/callback";

fn query(u: &str, key: &str) -> Option<String> {
    url::Url::parse(u)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

async fn authorize(app: &TestApp, q: &[(&str, &str)]) -> reqwest::Response {
    app.http
        .get(format!("{}/oauth/authorize", app.base))
        .query(q)
        .send()
        .await
        .unwrap()
}

/// A valid authorize query with `overrides` replacing or removing (empty value) defaults.
async fn authorize_with(app: &TestApp, overrides: &[(&str, &str)]) -> reqwest::Response {
    let (_, ch) = pkce();
    let mut q: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", "test-client"),
        ("redirect_uri", CB),
        ("state", "xyz"),
        ("code_challenge", &ch),
        ("code_challenge_method", "S256"),
    ];
    for (k, v) in overrides {
        q.retain(|(qk, _)| qk != k);
        if !v.is_empty() {
            q.push((k, v));
        }
    }
    authorize(app, &q).await
}

fn location(r: &reqwest::Response) -> String {
    r.headers()[LOCATION].to_str().unwrap().to_string()
}

fn exchange<'a>(
    client: &'a str,
    code: &'a str,
    uri: &'a str,
    verifier: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("grant_type", "authorization_code"),
        ("client_id", client),
        ("code", code),
        ("redirect_uri", uri),
        ("code_verifier", verifier),
    ]
}

async fn refresh(app: &TestApp, client: &str, rt: &str) -> (StatusCode, Value) {
    let (s, _, b) = app
        .token_post(&[
            ("grant_type", "refresh_token"),
            ("client_id", client),
            ("refresh_token", rt),
        ])
        .await;
    (s, b)
}

fn jwt_parts(t: &str) -> (Value, Value) {
    let p: Vec<&str> = t.split('.').collect();
    (
        serde_json::from_slice(&B64.decode(p[0]).unwrap()).unwrap(),
        serde_json::from_slice(&B64.decode(p[1]).unwrap()).unwrap(),
    )
}

/// Verifies a JWT against the published JWKS the way a resource server would; returns claims.
async fn verify_with_jwks(app: &TestApp, token: &str) -> Value {
    let jwks: Value = app
        .http
        .get(format!("{}/.well-known/jwks.json", app.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (header, claims) = jwt_parts(token);
    assert_eq!(header["alg"], "ES256");
    assert_eq!(header["typ"], "JWT");
    let key = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == header["kid"])
        .expect("kid in jwks");
    assert_eq!(
        (
            key["kty"].as_str(),
            key["crv"].as_str(),
            key["use"].as_str(),
            key["alg"].as_str()
        ),
        (Some("EC"), Some("P-256"), Some("sig"), Some("ES256"))
    );
    let mut sec1 = vec![4u8];
    sec1.extend(B64.decode(key["x"].as_str().unwrap()).unwrap());
    sec1.extend(B64.decode(key["y"].as_str().unwrap()).unwrap());
    let vk = VerifyingKey::from_sec1_bytes(&sec1).unwrap();
    let (signed, sig) = token.rsplit_once('.').unwrap();
    vk.verify(
        signed.as_bytes(),
        &Signature::from_slice(&B64.decode(sig).unwrap()).unwrap(),
    )
    .expect("signature");
    claims
}

async fn userinfo(app: &TestApp, token: &str) -> reqwest::Response {
    app.http
        .get(format!("{}/oauth/userinfo", app.base))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn full_code_flow_issues_verifiable_tokens() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let (verifier, challenge) = pkce();
    let resp = authorize(
        &app,
        &[
            ("response_type", "code"),
            ("client_id", "test-client"),
            ("redirect_uri", CB),
            ("scope", "openid profile email bogus"),
            ("state", "st4te"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("nonce", "n0nce"),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers()[CACHE_CONTROL], "no-store");
    let loc = location(&resp);
    assert!(
        loc.starts_with("http://localhost:5080/signin?challenge="),
        "{loc}"
    );
    let ch = query(&loc, "challenge").unwrap();

    let (s, b) = app
        .api(
            Method::GET,
            &format!("/api/auth-requests/{ch}"),
            None,
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        b,
        json!({"client_id": "test-client", "client_name": "Test Client", "scope": "openid profile email"})
    );

    let (s, b) = app
        .api(
            Method::POST,
            &format!("/api/auth-requests/{ch}/accept"),
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let to = b["redirect_to"].as_str().unwrap();
    assert!(to.starts_with(CB), "{to}");
    assert_eq!(query(to, "state").unwrap(), "st4te");
    let code = query(to, "code").unwrap();

    let (s, h, tok) = app
        .token_post(&exchange("test-client", &code, CB, &verifier))
        .await;
    assert_eq!(s, StatusCode::OK, "{tok}");
    assert_eq!(h[CACHE_CONTROL], "no-store");
    assert_eq!(h[PRAGMA], "no-cache");
    assert_eq!(tok["token_type"], "Bearer");
    assert_eq!(tok["expires_in"], 900);
    assert_eq!(tok["scope"], "openid profile email");
    assert!(tok["refresh_token"].as_str().is_some());

    let me = users::find_by_login(&app.state.db, "wiertmir")
        .unwrap()
        .unwrap()
        .0;
    let claims = verify_with_jwks(&app, tok["access_token"].as_str().unwrap()).await;
    assert_eq!(claims["iss"], app.state.cfg.issuer.as_str());
    assert_eq!(claims["aud"], "me-api");
    assert_eq!(claims["sub"], me.id.to_string());
    assert_eq!(claims["scope"], "openid profile email");
    assert_eq!(claims["preferred_username"], "wiertmir");
    assert_eq!(claims["client_id"], "test-client");
    assert!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap() == 900);

    let id = verify_with_jwks(&app, tok["id_token"].as_str().unwrap()).await;
    assert_eq!(
        (id["aud"].as_str(), id["nonce"].as_str(), id["sub"].as_str()),
        (
            Some("test-client"),
            Some("n0nce"),
            Some(me.id.to_string().as_str())
        )
    );
    assert_eq!(id["email"], me.email.as_str());
    assert_eq!(id["email_verified"], true);
    assert_eq!(id["name"], "wiertmir");
    assert_eq!(id["preferred_username"], "wiertmir");
    assert_eq!(
        id["exp"].as_i64().unwrap() - id["iat"].as_i64().unwrap(),
        900
    );

    let r = userinfo(&app, tok["access_token"].as_str().unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);
    let ui: Value = r.json().await.unwrap();
    assert_eq!(
        (
            ui["sub"].as_str(),
            ui["preferred_username"].as_str(),
            ui["email"].as_str()
        ),
        (
            Some(me.id.to_string().as_str()),
            Some("wiertmir"),
            Some(me.email.as_str())
        )
    );
    assert_eq!(ui["email_verified"], true);
}

#[tokio::test]
async fn scopes_filter_id_token_and_userinfo_email() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "profile").await;
    assert!(t.get("id_token").is_none());
    assert!(
        t["refresh_token"].as_str().is_some(),
        "refresh token is issued without offline_access"
    );
    let ui: Value = userinfo(&app, t["access_token"].as_str().unwrap())
        .await
        .json()
        .await
        .unwrap();
    assert!(ui.get("email").is_none());
    let t = app.oauth_tokens(&session, "").await;
    assert_eq!(t["scope"], "");
}

#[tokio::test]
async fn unregistered_redirect_uri_never_redirects() {
    let app = TestApp::spawn().await;
    for ov in [
        vec![("redirect_uri", "https://evil.example/cb")],
        vec![("redirect_uri", "https://app.example/cb/")],
        vec![("redirect_uri", "https://app.example/cb#frag")],
        vec![("redirect_uri", "https://app.example:8443/cb")],
        vec![("client_id", "nobody")],
        vec![("redirect_uri", "")],
        vec![("client_id", "")],
    ] {
        let r = authorize_with(&app, &ov).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{ov:?}");
        assert!(r.headers().get(LOCATION).is_none(), "{ov:?}");
        assert_eq!(r.headers()[CACHE_CONTROL], "no-store");
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }
}

#[tokio::test]
async fn loopback_redirect_ignores_port_only_for_loopback() {
    let app = TestApp::spawn().await;
    let r = authorize_with(&app, &[("redirect_uri", "http://127.0.0.1:53123/callback")]).await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert!(location(&r).starts_with("http://localhost:5080/signin?challenge="));
    for bad in [
        "http://127.0.0.1:53123/other",
        "http://localhost:53123/callback",
        "https://127.0.0.1:53123/callback",
        "http://127.0.0.1@evil.example/callback",
    ] {
        let r = authorize_with(&app, &[("redirect_uri", bad)]).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // A registered non-loopback URI is never matched loosely.
    assert_eq!(
        authorize_with(&app, &[("redirect_uri", "https://app.example:444/cb")])
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    // The loopback code flow returns to the exact port the client asked for.
    let session = app.admin_session().await;
    let (code, verifier) = app
        .auth_code(&session, "openid", "http://127.0.0.1:53123/callback", None)
        .await;
    assert_eq!(
        app.token_post(&exchange("test-client", &code, CB, &verifier))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn invalid_parameters_redirect_with_error_and_state() {
    let app = TestApp::spawn().await;
    for (ov, err) in [
        (vec![("code_challenge", "")], "invalid_request"),
        (vec![("code_challenge", "short")], "invalid_request"),
        (
            vec![(
                "code_challenge",
                "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
            )],
            "invalid_request",
        ),
        (vec![("code_challenge_method", "plain")], "invalid_request"),
        (vec![("code_challenge_method", "")], "invalid_request"),
        (
            vec![("response_type", "token")],
            "unsupported_response_type",
        ),
        (vec![("response_type", "")], "unsupported_response_type"),
    ] {
        let r = authorize_with(&app, &ov).await;
        assert_eq!(r.status(), StatusCode::FOUND, "{ov:?}");
        let loc = location(&r);
        assert!(loc.starts_with(CB), "{loc}");
        assert_eq!(query(&loc, "error").unwrap(), err, "{ov:?}");
        assert!(query(&loc, "error_description").is_some());
        assert_eq!(query(&loc, "state").unwrap(), "xyz");
    }
}

#[tokio::test]
async fn pkce_is_enforced() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let (code, _) = app.auth_code(&session, "openid", CB, None).await;
    let (s, _, b) = app
        .token_post(&exchange("test-client", &code, CB, &pkce().0))
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    // The failed attempt spent the code: even the right verifier cannot be used afterwards.
    let (code, verifier) = app.auth_code(&session, "openid", CB, None).await;
    let (s, _, b) = app
        .token_post(&exchange("test-client", &code, CB, &"a".repeat(42)))
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
    let (s, _, _) = app
        .token_post(&exchange("test-client", &code, CB, &verifier))
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "malformed request does not spend the code"
    );
}

#[tokio::test]
async fn token_endpoint_rejects_mismatched_parameters() {
    let app = TestApp::spawn_with(|c| {
        c.clients.push(auth_service::config::ClientConfig {
            id: "other".into(),
            name: "Other".into(),
            redirect_uris: vec![CB.into()],
        });
    })
    .await;
    let session = app.admin_session().await;
    let (code, v) = app.auth_code(&session, "openid", CB, None).await;
    let (s, _, b) = app
        .token_post(&exchange(
            "test-client",
            &code,
            "https://app.example/cb",
            &v,
        ))
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (code, v) = app.auth_code(&session, "openid", CB, None).await;
    let (s, _, b) = app.token_post(&exchange("other", &code, CB, &v)).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (s, _, b) = app.token_post(&exchange("ghost", &code, CB, &v)).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_client"))
    );
    let (s, _, b) = app
        .token_post(&[("grant_type", "password"), ("client_id", "test-client")])
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_grant_type"))
    );
    let (s, _, b) = app
        .token_post(&[
            ("grant_type", "authorization_code"),
            ("client_id", "test-client"),
        ])
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
    let (s, _, b) = app
        .token_post(&[("grant_type", "authorization_code")])
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );
}

#[tokio::test]
async fn expired_auth_code_is_rejected() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let (code, v) = app.auth_code(&session, "openid", CB, None).await;
    app.state
        .db
        .with(|c| c.execute("UPDATE auth_codes SET expires_at = 1", []))
        .unwrap();
    let (s, _, b) = app
        .token_post(&exchange("test-client", &code, CB, &v))
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test]
async fn auth_code_replay_revokes_tokens() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let (code, v) = app.auth_code(&session, "openid", CB, None).await;
    let (s, _, first) = app
        .token_post(&exchange("test-client", &code, CB, &v))
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, b) = app
        .token_post(&exchange("test-client", &code, CB, &v))
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (s, b) = refresh(
        &app,
        "test-client",
        first["refresh_token"].as_str().unwrap(),
    )
    .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test]
async fn expired_or_reused_challenge_is_rejected() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let ch = |r: reqwest::Response| query(&location(&r), "challenge").unwrap();
    let c1 = ch(authorize_with(&app, &[]).await);
    app.state
        .db
        .with(|c| c.execute("UPDATE auth_requests SET expires_at = 1", []))
        .unwrap();
    for (m, p) in [
        (Method::GET, format!("/api/auth-requests/{c1}")),
        (Method::POST, format!("/api/auth-requests/{c1}/accept")),
    ] {
        let (s, b) = app.api(m, &p, Some(&session), Value::Null).await;
        assert_eq!(
            (s, b["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_challenge"))
        );
    }
    let c2 = ch(authorize_with(&app, &[]).await);
    let p = format!("/api/auth-requests/{c2}/accept");
    assert_eq!(
        app.api(Method::POST, &p, Some(&session), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (s, b) = app.api(Method::POST, &p, Some(&session), Value::Null).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_challenge"))
    );
    let (s, b) = app
        .api(
            Method::GET,
            "/api/auth-requests/nonsense",
            None,
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_challenge"))
    );
}

#[tokio::test]
async fn refresh_rotates_and_detects_reuse() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "openid email").await;
    let r1 = t["refresh_token"].as_str().unwrap();
    let (s, b) = refresh(&app, "test-client", r1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r2 = b["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(r1, r2);
    assert_eq!(b["scope"], "openid email");
    let claims = verify_with_jwks(&app, b["access_token"].as_str().unwrap()).await;
    assert_eq!(claims["scope"], "openid email");
    let (s, b) = refresh(&app, "test-client", r1).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (s, b) = refresh(&app, "test-client", &r2).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test]
async fn refresh_keeps_family_expiry_and_is_bound_to_client() {
    let app = TestApp::spawn_with(|c| {
        c.clients.push(auth_service::config::ClientConfig {
            id: "other".into(),
            name: "Other".into(),
            redirect_uris: vec![CB.into()],
        });
    })
    .await;
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "openid").await;
    app.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE refresh_tokens SET expires_at = expires_at - 1000",
                [],
            )
        })
        .unwrap();
    let before: i64 = app
        .state
        .db
        .with(|c| c.query_row("SELECT expires_at FROM refresh_tokens", [], |r| r.get(0)))
        .unwrap();
    let (s, b) = refresh(&app, "other", t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (s, b) = refresh(&app, "test-client", t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "wrong-client attempt must not burn the token: {b}"
    );
    let max: i64 = app
        .state
        .db
        .with(|c| {
            c.query_row("SELECT max(expires_at) FROM refresh_tokens", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(max, before, "rotation never extends the family lifetime");
    // Expired family
    app.state
        .db
        .with(|c| c.execute("UPDATE refresh_tokens SET expires_at = 1", []))
        .unwrap();
    let (s, b) = refresh(&app, "test-client", b["refresh_token"].as_str().unwrap()).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test]
async fn parallel_refresh_with_same_token_succeeds_once() {
    let app = std::sync::Arc::new(TestApp::spawn().await);
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "openid").await;
    let rt = t["refresh_token"].as_str().unwrap().to_string();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (app, rt) = (app.clone(), rt.clone());
            tokio::spawn(async move { refresh(&app, "test-client", &rt).await.0 })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1);
}

#[tokio::test]
async fn revoke_kills_the_family_and_always_answers_200() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "openid").await;
    let (s, b) = refresh(&app, "test-client", t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    let current = b["refresh_token"].as_str().unwrap();
    let post = |form: Vec<(&str, String)>| {
        let r = app
            .http
            .post(format!("{}/oauth/revoke", app.base))
            .form(&form);
        async move { r.send().await.unwrap().status() }
    };
    // Wrong client: silently ignored.
    assert_eq!(
        post(vec![
            ("token", current.into()),
            ("client_id", "nobody".into())
        ])
        .await,
        StatusCode::OK
    );
    assert_eq!(
        refresh(&app, "test-client", current).await.0,
        StatusCode::OK
    );
    let (_, b) = refresh(
        &app,
        "test-client",
        app.oauth_tokens(&session, "openid").await["refresh_token"]
            .as_str()
            .unwrap(),
    )
    .await;
    let live = b["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(
        post(vec![
            ("token", live.clone()),
            ("client_id", "test-client".into())
        ])
        .await,
        StatusCode::OK
    );
    assert_eq!(
        refresh(&app, "test-client", &live).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(vec![("token", "unknown".into())]).await,
        StatusCode::OK
    );
    assert_eq!(post(vec![]).await, StatusCode::OK);
}

#[tokio::test]
async fn refresh_fails_after_password_reset_or_disable() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let me = users::find_by_login(&app.state.db, "wiertmir")
        .unwrap()
        .unwrap()
        .0;
    // Admin password reset (revokes refresh tokens).
    let t = app.oauth_tokens(&session, "openid").await;
    assert!(users::admin_reset(&app.state.db, me.id, "x").unwrap());
    let (s, b) = refresh(&app, "test-client", t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    // Disabled user whose token rows somehow survive: still refused.
    app.state
        .db
        .with(|c| c.execute("UPDATE users SET must_change_password = 0", []))
        .unwrap();
    let session = auth_service::sessions::create(&app.state.db, me.id, "", "").unwrap();
    let t = app.oauth_tokens(&session, "openid").await;
    app.state
        .db
        .with(|c| c.execute("UPDATE users SET disabled = 1", []))
        .unwrap();
    let (s, b) = refresh(&app, "test-client", t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    // And userinfo refuses the still-unexpired access token.
    assert_eq!(
        userinfo(&app, t["access_token"].as_str().unwrap())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn user_who_must_change_password_cannot_accept() {
    let app = TestApp::spawn().await;
    let (_, b) = app
        .api(
            Method::POST,
            "/api/signin",
            None,
            json!({"login": "wiertmir", "password": app.seed_password}),
        )
        .await;
    let pending = b["session_token"].as_str().unwrap();
    let ch = query(&location(&authorize_with(&app, &[]).await), "challenge").unwrap();
    let (s, b) = app
        .api(
            Method::POST,
            &format!("/api/auth-requests/{ch}/accept"),
            Some(pending),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("password_change_required"))
    );
    let (s, _) = app
        .api(
            Method::POST,
            &format!("/api/auth-requests/{ch}/accept"),
            None,
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn userinfo_rejects_bad_tokens() {
    let app = TestApp::spawn().await;
    let session = app.admin_session().await;
    let t = app.oauth_tokens(&session, "openid email").await;
    let good = t["access_token"].as_str().unwrap();
    let (_, claims) = jwt_parts(good);
    let mut expired = claims.clone();
    expired["exp"] = json!(1);
    let mut wrong_iss = claims.clone();
    wrong_iss["iss"] = json!("https://evil.example");
    let mut wrong_aud = claims.clone();
    wrong_aud["aud"] = json!("someone-else");
    let parts: Vec<&str> = good.split('.').collect();
    let mut forged = claims.clone();
    forged["sub"] = json!(Uuid::new_v4());
    let alg_none = format!(
        "{}.{}.",
        B64.encode(r#"{"alg":"none","typ":"JWT"}"#),
        B64.encode(claims.to_string())
    );
    let other_key = Signer::load_or_create(&app.state.cfg.data_dir.join("other.key"))
        .unwrap()
        .sign(&claims);
    let bad = [
        app.state.signer.sign(&expired),
        app.state.signer.sign(&wrong_iss),
        app.state.signer.sign(&wrong_aud),
        format!(
            "{}.{}.{}",
            parts[0],
            B64.encode(forged.to_string()),
            parts[2]
        ),
        alg_none,
        other_key,
        t["id_token"].as_str().unwrap().to_string(),
        "garbage".into(),
    ];
    for token in &bad {
        let r = userinfo(&app, token).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{token}");
        assert_eq!(
            r.headers()[WWW_AUTHENTICATE],
            r#"Bearer error="invalid_token""#
        );
    }
    let r = app
        .http
        .get(format!("{}/oauth/userinfo", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(r.headers().get(WWW_AUTHENTICATE).is_some());
    assert_eq!(userinfo(&app, good).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn public_routes_need_no_secret_but_internal_ones_do() {
    let app = TestApp::spawn().await;
    for p in [
        "/.well-known/openid-configuration",
        "/.well-known/jwks.json",
    ] {
        assert_eq!(
            app.http
                .get(format!("{}{p}", app.base))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "{p}"
        );
    }
    assert_eq!(authorize_with(&app, &[]).await.status(), StatusCode::FOUND);
    let r = app
        .http
        .post(format!("{}/oauth/token", app.base))
        .form(&[("grant_type", "x"), ("client_id", "test-client")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app.http
            .post(format!("{}/oauth/revoke", app.base))
            .form(&[("token", "x")])
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for (m, p) in [
        (Method::GET, "/api/auth-requests/x"),
        (Method::POST, "/api/auth-requests/x/accept"),
    ] {
        let r = app
            .http
            .request(m, format!("{}{p}", app.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{p}");
    }
}

#[tokio::test]
async fn discovery_document_is_consistent() {
    let app = TestApp::spawn_with(|c| c.issuer = "https://auth.example".into()).await;
    let d: Value = app
        .http
        .get(format!("{}/.well-known/openid-configuration", app.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["issuer"], "https://auth.example");
    assert_eq!(
        d["authorization_endpoint"],
        "https://auth.example/oauth/authorize"
    );
    assert_eq!(d["token_endpoint"], "https://auth.example/oauth/token");
    assert_eq!(
        d["userinfo_endpoint"],
        "https://auth.example/oauth/userinfo"
    );
    assert_eq!(
        d["revocation_endpoint"],
        "https://auth.example/oauth/revoke"
    );
    assert_eq!(d["jwks_uri"], "https://auth.example/.well-known/jwks.json");
    assert_eq!(d["response_types_supported"], json!(["code"]));
    assert_eq!(
        d["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(d["subject_types_supported"], json!(["public"]));
    assert_eq!(d["id_token_signing_alg_values_supported"], json!(["ES256"]));
    assert_eq!(d["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        d["scopes_supported"],
        json!(["openid", "profile", "email", "offline_access"])
    );
    assert_eq!(d["token_endpoint_auth_methods_supported"], json!(["none"]));
}

#[tokio::test]
async fn signing_key_is_persisted_with_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("signing.key");
    let a = Signer::load_or_create(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("-----BEGIN PRIVATE KEY-----")
    );
    let b = Signer::load_or_create(&path).unwrap();
    assert_eq!(a.jwks(), b.jwks());
    let kid = a.jwks()["keys"][0]["kid"].as_str().unwrap().to_string();
    assert_eq!(kid.len(), 16);
    assert!(kid.bytes().all(|c| c.is_ascii_hexdigit()));
}
