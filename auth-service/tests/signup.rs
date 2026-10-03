mod support;
use auth_service::{config::SignupMode, email_tokens::{self, Purpose}};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::TestApp;

const PW: &str = "correct horse battery";

async fn post(app: &TestApp, path: &str, body: Value) -> (StatusCode, Value) {
    app.api(Method::POST, path, None, body).await
}

async fn signup(app: &TestApp, username: &str, email: &str, pw: &str) -> (StatusCode, Value) {
    post(app, "/api/signup", json!({"username": username, "email": email, "password": pw})).await
}

async fn signin(app: &TestApp, login: &str, pw: &str) -> (StatusCode, Value) {
    post(app, "/api/signin", json!({"login": login, "password": pw})).await
}

fn code(r: &(StatusCode, Value)) -> (u16, Option<&str>) {
    (r.0.as_u16(), r.1["code"].as_str())
}

#[tokio::test]
async fn signup_without_mail_is_active_immediately() {
    let app = TestApp::spawn().await;
    let (s, b) = signup(&app, "alice", "alice@example.com", PW).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["verification_required"], false);
    assert_eq!(b["user"]["email_verified"], true);
    assert_eq!(signin(&app, "alice", PW).await.0, StatusCode::OK);
}

#[tokio::test]
async fn signup_with_mail_requires_verification() {
    let app = TestApp::spawn_with_mail().await;
    let (s, b) = signup(&app, "alice", "alice@example.com", PW).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["verification_required"], true);
    assert_eq!(b["user"]["email_verified"], false);
    assert!(app.last_mail_body().contains("http://localhost:5080/verify?token="));
    assert!(app.last_mail_body().contains("24 hours"));

    // Wrong password never reveals the account state.
    assert_eq!(code(&signin(&app, "alice", "wrong password here").await), (401, Some("invalid_credentials")));
    assert_eq!(code(&signin(&app, "alice", PW).await), (403, Some("email_not_verified")));

    let token = app.last_mail_token();
    let (s, _) = post(&app, "/api/email/verify", json!({"token": token})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(signin(&app, "alice", PW).await.0, StatusCode::OK);
    assert_eq!(code(&post(&app, "/api/email/verify", json!({"token": token})).await), (400, Some("invalid_token")));
}

#[tokio::test]
async fn signup_rejects_duplicates_and_bad_input() {
    let app = TestApp::spawn().await;
    assert_eq!(signup(&app, "alice", "alice@example.com", PW).await.0, StatusCode::CREATED);
    assert_eq!(code(&signup(&app, "ALICE", "other@example.com", PW).await), (409, Some("conflict")));
    assert_eq!(code(&signup(&app, "  Alice ", "other@example.com", PW).await), (409, Some("conflict")));
    assert_eq!(code(&signup(&app, "bob", "Alice@Example.com", PW).await), (409, Some("conflict")));
    for (u, e, p) in [
        ("a b", "x@example.com", PW),
        ("ab", "x@example.com", PW),
        ("bob", "nope", PW),
        ("bob", "a@b@example.com", PW),
        ("bob", "x@localhost", PW),
        ("bob", "x@example.com", "elevenchars"),
    ] {
        assert_eq!(code(&signup(&app, u, e, p).await), (422, Some("validation")), "{u} {e} {p}");
    }
}

#[tokio::test]
async fn signup_disabled_by_config() {
    let app = TestApp::spawn_with(|c| c.signup = SignupMode::Disabled).await;
    assert_eq!(code(&signup(&app, "alice", "alice@example.com", PW).await), (403, Some("signup_disabled")));
}

#[tokio::test]
async fn reset_flow() {
    let app = TestApp::spawn_with_mail().await;
    signup(&app, "alice", "alice@example.com", PW).await;
    post(&app, "/api/email/verify", json!({"token": app.last_mail_token()})).await;
    let (_, b) = signin(&app, "alice", PW).await;
    let old_session = b["session_token"].as_str().unwrap().to_string();
    let before = app.mail_count();

    let (s, _) = post(&app, "/api/password/forgot", json!({"email": "nobody@example.com"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.mail_count(), before);

    // A username is not an email: no mail either.
    let (s, _) = post(&app, "/api/password/forgot", json!({"email": "alice"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.mail_count(), before);

    let (s, _) = post(&app, "/api/password/forgot", json!({"email": " Alice@Example.com "})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(app.mail_count(), before + 1);
    assert!(app.last_mail_body().contains("http://localhost:5080/reset?token="));
    assert!(app.last_mail_body().contains("1 hour"));
    let token = app.last_mail_token();

    // Weak password is rejected without burning the token.
    let (s, _) = post(&app, "/api/password/reset", json!({"token": token, "new_password": "short"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let new_pw = "another long password";
    let (s, _) = post(&app, "/api/password/reset", json!({"token": token, "new_password": new_pw})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(code(&signin(&app, "alice", PW).await), (401, Some("invalid_credentials")));
    assert_eq!(signin(&app, "alice", new_pw).await.0, StatusCode::OK);
    assert_eq!(code(&post(&app, "/api/password/reset", json!({"token": token, "new_password": new_pw})).await), (400, Some("invalid_token")));
    let (s, _) = app.api(Method::GET, "/api/me", Some(&old_session), Value::Null).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn forgot_does_nothing_without_mail() {
    let app = TestApp::spawn().await;
    signup(&app, "alice", "alice@example.com", PW).await;
    let (s, _) = post(&app, "/api/password/forgot", json!({"email": "alice@example.com"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn reset_verifies_email_revokes_everything_and_drops_other_reset_tokens() {
    let app = TestApp::spawn_with_mail().await;
    signup(&app, "alice", "alice@example.com", PW).await;
    post(&app, "/api/password/forgot", json!({"email": "alice@example.com"})).await;
    let first = app.last_mail_token();
    post(&app, "/api/password/forgot", json!({"email": "alice@example.com"})).await;
    let second = app.last_mail_token();
    assert_ne!(first, second);

    let (s, _) = post(&app, "/api/password/reset", json!({"token": second, "new_password": "another long password"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(code(&post(&app, "/api/password/reset", json!({"token": first, "new_password": "yet another password"})).await), (400, Some("invalid_token")));
    // Reset proved control of the email, so sign-in no longer needs verification.
    assert_eq!(signin(&app, "alice", "another long password").await.0, StatusCode::OK);
}

#[tokio::test]
async fn tokens_are_purpose_bound_and_expire() {
    let app = TestApp::spawn_with_mail().await;
    let (_, b) = signup(&app, "alice", "alice@example.com", PW).await;
    let id: uuid::Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    let verify_token = app.last_mail_token();
    // A verify token cannot reset a password.
    let r = post(&app, "/api/password/reset", json!({"token": verify_token, "new_password": "another long password"})).await;
    assert_eq!(code(&r), (400, Some("invalid_token")));
    // A reset token cannot verify an email.
    let reset = email_tokens::create(&app.state.db, id, Purpose::Reset).unwrap();
    assert_eq!(code(&post(&app, "/api/email/verify", json!({"token": reset})).await), (400, Some("invalid_token")));
    // Expired tokens are refused.
    let old = email_tokens::create_expiring(&app.state.db, id, Purpose::Verify, 1).unwrap();
    assert_eq!(code(&post(&app, "/api/email/verify", json!({"token": old})).await), (400, Some("invalid_token")));
    assert_eq!(code(&post(&app, "/api/email/verify", json!({"token": "garbage"})).await), (400, Some("invalid_token")));
    // The right token still works (the verify token above was not consumed by the purpose mismatch).
    assert_eq!(post(&app, "/api/email/verify", json!({"token": verify_token})).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn resend_sends_only_for_unverified_accounts_and_is_rate_limited() {
    let app = TestApp::spawn_with_mail().await;
    signup(&app, "alice", "alice@example.com", PW).await;
    let n = app.mail_count();
    let (s, _) = post(&app, "/api/email/resend", json!({"email": "nobody@example.com"})).await;
    assert_eq!((s, app.mail_count()), (StatusCode::NO_CONTENT, n));
    let (s, _) = post(&app, "/api/email/resend", json!({"email": "ALICE@example.com"})).await;
    assert_eq!((s, app.mail_count()), (StatusCode::NO_CONTENT, n + 1));
    post(&app, "/api/email/verify", json!({"token": app.last_mail_token()})).await;
    let (s, _) = post(&app, "/api/email/resend", json!({"email": "alice@example.com"})).await;
    assert_eq!((s, app.mail_count()), (StatusCode::NO_CONTENT, n + 1));
    // The per-email limit stops a mailbox flood.
    let mut last = StatusCode::NO_CONTENT;
    for _ in 0..8 {
        last = post(&app, "/api/email/resend", json!({"email": "alice@example.com"})).await.0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}
