mod support;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::{SERVICE_SECRET, TestApp};

async fn signin(app: &TestApp, login: &str, pw: &str) -> (StatusCode, Value) {
    app.api(
        Method::POST,
        "/api/signin",
        None,
        json!({"login": login, "password": pw}),
    )
    .await
}

#[tokio::test]
async fn seed_user_must_change_password_first() {
    let app = TestApp::spawn().await;
    let (s, b) = signin(&app, "wiertmir", &app.seed_password).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["user"]["must_change_password"], true);
    assert_eq!(b["user"]["is_admin"], true);
    assert_eq!(b["user"]["email"], "wiertmir@localhost");
    let t = b["session_token"].as_str().unwrap().to_string();

    let (s, _) = app.api(Method::GET, "/api/me", Some(&t), Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = app
        .api(
            Method::PATCH,
            "/api/me",
            Some(&t),
            json!({"display_name": "x"}),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("password_change_required"))
    );

    let change = |new: &str| json!({"current_password": app.seed_password, "new_password": new});
    let (s, b) = app
        .api(
            Method::POST,
            "/api/password/change",
            Some(&t),
            change("short"),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation"))
    );
    let (s, _) = app
        .api(
            Method::POST,
            "/api/password/change",
            Some(&t),
            change("correct horse battery"),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, b) = app
        .api(
            Method::PATCH,
            "/api/me",
            Some(&t),
            json!({"display_name": "Mirek"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["display_name"], "Mirek");
    let (s, b) = signin(&app, "wiertmir", &app.seed_password).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_credentials"))
    );
}

#[tokio::test]
async fn password_change_revokes_other_sessions() {
    let app = TestApp::spawn().await;
    let (_, b) = signin(&app, "wiertmir", &app.seed_password).await;
    let other = b["session_token"].as_str().unwrap().to_string();
    let admin = app.admin_session().await;
    let (s, _) = app
        .api(Method::GET, "/api/me", Some(&other), Value::Null)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = app
        .api(Method::GET, "/api/me", Some(&admin), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
}

/// A voluntary change keeps only the session that made it: an intruder's other grants die with the old password.
#[tokio::test]
async fn password_change_revokes_refresh_tokens_app_passwords_and_other_sessions() {
    let app = TestApp::spawn().await;
    let current = app.admin_session().await;
    let (_, b) = signin(&app, "wiertmir", "correct horse battery").await;
    let other = b["session_token"].as_str().unwrap().to_string();
    let refresh = app.oauth_tokens(&current, "openid").await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b) = app
        .api(
            Method::POST,
            "/api/me/app-passwords",
            Some(&current),
            json!({"label": "phone"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let app_password = b["password"].as_str().unwrap().to_string();
    let verify = || {
        app.api(
            Method::POST,
            "/api/app-passwords/verify",
            None,
            json!({"username": "wiertmir", "password": app_password}),
        )
    };
    assert_eq!(verify().await.0, StatusCode::OK);

    let (s, b) = app
        .api(
            Method::POST,
            "/api/password/change",
            Some(&current),
            json!({"current_password": "correct horse battery", "new_password": "a brand new passphrase"}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");

    let (s, _, b) = app
        .token_post(&[
            ("grant_type", "refresh_token"),
            ("client_id", "test-client"),
            ("refresh_token", &refresh),
        ])
        .await;
    assert_eq!(
        (s, b["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    assert_eq!(verify().await.0, StatusCode::UNAUTHORIZED);
    for (token, expected) in [
        (&other, StatusCode::UNAUTHORIZED),
        (&current, StatusCode::OK),
    ] {
        let (s, _) = app
            .api(Method::GET, "/api/me", Some(token), Value::Null)
            .await;
        assert_eq!(s, expected);
    }
}

#[tokio::test]
async fn login_is_case_and_space_insensitive() {
    let app = TestApp::spawn().await;
    let (_, a) = signin(&app, "wiertmir", &app.seed_password).await;
    let (s, b) = signin(&app, "  WIERTMIR ", &app.seed_password).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(a["user"]["id"], b["user"]["id"]);
    let (s, _) = signin(&app, " WiertMir@Localhost", &app.seed_password).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_identical() {
    let app = TestApp::spawn().await;
    let (s1, b1) = signin(&app, "wiertmir", "definitely wrong password").await;
    let (s2, b2) = signin(&app, "nobody", "definitely wrong password").await;
    assert_eq!(
        (s1, s2),
        (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED)
    );
    assert_eq!(b1, b2);
}

#[tokio::test]
async fn sixth_failure_is_rate_limited() {
    let app = TestApp::spawn().await;
    for _ in 0..5 {
        let (s, _) = signin(&app, "wiertmir", "definitely wrong password").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let r = app
        .http
        .post(format!("{}/api/signin", app.base))
        .header("X-Service-Secret", SERVICE_SECRET)
        .json(&json!({"login": "wiertmir", "password": "definitely wrong password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry >= 1);
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["code"], "rate_limited");
    let (s, _) = signin(&app, "wiertmir", &app.seed_password).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn missing_or_wrong_service_secret_is_401() {
    let app = TestApp::spawn().await;
    let body = json!({"login": "wiertmir", "password": app.seed_password});
    let r = app
        .http
        .post(format!("{}/api/signin", app.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["code"], "unauthorized");
    let r = app
        .http
        .post(format!("{}/api/signin", app.base))
        .header("X-Service-Secret", "nope")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = app
        .http
        .get(format!("{}/api/nope", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signout_invalidates_session() {
    let app = TestApp::spawn().await;
    let t = app.admin_session().await;
    let (s, _) = app
        .api(Method::POST, "/api/signout", Some(&t), Value::Null)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.api(Method::GET, "/api/me", Some(&t), Value::Null).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn oversized_password_is_rejected_without_hashing() {
    let app = TestApp::spawn().await;
    let (s, b) = signin(&app, "wiertmir", &"a".repeat(2000)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("validation"))
    );
}

#[tokio::test]
async fn disabled_user_cannot_sign_in_and_session_dies() {
    let app = TestApp::spawn().await;
    let t = app.admin_session().await;
    app.state
        .db
        .with(|c| c.execute("UPDATE users SET disabled = 1", []))
        .unwrap();
    let (s, _) = app.api(Method::GET, "/api/me", Some(&t), Value::Null).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = signin(&app, "wiertmir", "correct horse battery").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_credentials"))
    );
}

#[test]
fn config_debug_hides_secrets() {
    let cfg = auth_service::Config::for_tests("/tmp".into(), "super-secret-value");
    assert!(!format!("{cfg:?}").contains("super-secret-value"));
}

#[tokio::test]
async fn parallel_wrong_guesses_cannot_outrun_the_lock() {
    let app = TestApp::spawn().await;
    let results = burst_of_wrong_guesses(&app).await;
    let unauthorized = results
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    let limited = results
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert!(unauthorized <= 5, "{unauthorized} guesses got through");
    assert_eq!(unauthorized + limited, 30);
}

async fn burst_of_wrong_guesses(app: &TestApp) -> Vec<StatusCode> {
    let tasks: Vec<_> = (0..30)
        .map(|_| {
            let (http, url) = (app.http.clone(), format!("{}/api/signin", app.base));
            tokio::spawn(async move {
                http.post(url)
                    .header("X-Service-Secret", SERVICE_SECRET)
                    .json(&json!({"login": "wiertmir", "password": "definitely wrong password"}))
                    .send()
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

#[test]
fn build_state_rejects_weak_service_secret() {
    for secret in ["", "too-short"] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = auth_service::Config::for_tests(dir.path().to_path_buf(), secret);
        assert!(
            auth_service::build_state(cfg).is_err(),
            "{secret:?} accepted"
        );
    }
}
