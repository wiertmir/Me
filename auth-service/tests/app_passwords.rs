mod support;
use auth_service::{
    crypto,
    users::{self, NewUser},
};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::TestApp;

const PW: &str = "correct horse battery";

/// Creates an active, verified user; returns (id, session token).
async fn user(app: &TestApp, name: &str) -> (String, String) {
    let u = users::create(
        &app.state.db,
        NewUser {
            username: name.into(),
            email: format!("{name}@example.com"),
            email_verified: true,
            is_admin: false,
            must_change_password: false,
            password_hash: Some(crypto::hash_password(PW)),
        },
    )
    .unwrap();
    let (s, b) = app
        .api(
            Method::POST,
            "/api/signin",
            None,
            json!({"login": name, "password": PW}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    (
        u.id.to_string(),
        b["session_token"].as_str().unwrap().into(),
    )
}

/// Creates an app password; returns (id, password).
async fn create(app: &TestApp, tok: &str, label: &str) -> (String, String) {
    let (s, b) = app
        .api(
            Method::POST,
            "/api/me/app-passwords",
            Some(tok),
            json!({"label": label}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    (
        b["id"].as_str().unwrap().into(),
        b["password"].as_str().unwrap().into(),
    )
}

async fn verify(app: &TestApp, username: &str, password: &str) -> (StatusCode, Value) {
    app.api(
        Method::POST,
        "/api/app-passwords/verify",
        None,
        json!({"username": username, "password": password}),
    )
    .await
}

fn assert_invalid(r: &(StatusCode, Value)) {
    assert_eq!(r.0, StatusCode::UNAUTHORIZED, "{}", r.1);
    assert_eq!(r.1["code"], "invalid_credentials");
}

async fn list(app: &TestApp, tok: &str) -> Vec<Value> {
    let (s, b) = app
        .api(Method::GET, "/api/me/app-passwords", Some(tok), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK);
    b.as_array().unwrap().clone()
}

fn count(app: &TestApp, id: &str) -> i64 {
    app.state
        .db
        .with(|c| {
            c.query_row(
                "SELECT count(*) FROM app_passwords WHERE user_id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap()
}

#[tokio::test]
async fn created_password_verifies_and_is_listed_without_secret() {
    let app = TestApp::spawn().await;
    let (id, tok) = user(&app, "alice").await;
    let (apid, pw) = create(&app, &tok, "  Phone  ").await;

    let b = pw.as_bytes();
    assert_eq!(pw.len(), 19, "{pw}");
    for (i, c) in b.iter().enumerate() {
        if i % 5 == 4 {
            assert_eq!(*c, b'-', "{pw}");
        } else {
            assert!(c.is_ascii_lowercase(), "{pw}");
        }
    }

    let (s, v) = verify(&app, "alice", &pw).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"user_id": id, "username": "alice"}));

    let l = list(&app, &tok).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0]["id"], apid);
    assert_eq!(l[0]["label"], "Phone");
    assert!(l[0]["created_at"].is_string());
    assert!(l[0]["last_used"].is_string());
    assert!(l[0].get("password").is_none() && l[0].get("hash").is_none());
}

#[tokio::test]
async fn last_used_is_null_before_first_use_and_passwords_are_distinct() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, a) = create(&app, &tok, "a").await;
    let (_, b) = create(&app, &tok, "b").await;
    assert_ne!(a, b);
    let l = list(&app, &tok).await;
    assert_eq!(l.len(), 2);
    assert_eq!(l[0]["label"], "a");
    assert!(l[0]["last_used"].is_null());
}

#[tokio::test]
async fn verify_accepts_password_without_dashes_with_spaces_and_upper_case() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    let plain = pw.replace('-', "");
    assert_eq!(verify(&app, "alice", &plain).await.0, StatusCode::OK);
    let spaced = pw.replace('-', " ");
    assert_eq!(verify(&app, "alice", &spaced).await.0, StatusCode::OK);
    assert_eq!(
        verify(&app, "alice", &pw.to_uppercase()).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn verify_by_email_and_normalized_username() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    assert_eq!(
        verify(&app, " Alice@Example.com ", &pw).await.0,
        StatusCode::OK
    );
    assert_eq!(verify(&app, " ALICE ", &pw).await.0, StatusCode::OK);
}

#[tokio::test]
async fn deleted_password_stops_working() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (id, pw) = create(&app, &tok, "Phone").await;
    let (s, _) = app
        .api(
            Method::DELETE,
            &format!("/api/me/app-passwords/{id}"),
            Some(&tok),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_invalid(&verify(&app, "alice", &pw).await);
    assert!(list(&app, &tok).await.is_empty());
}

#[tokio::test]
async fn account_password_is_not_an_app_password() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    create(&app, &tok, "Phone").await;
    assert_invalid(&verify(&app, "alice", PW).await);
}

#[tokio::test]
async fn failures_look_the_same() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, bob_tok) = user(&app, "bob").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    let (_, bob_pw) = create(&app, &bob_tok, "Phone").await;
    let wrong = verify(&app, "alice", "aaaa-bbbb-cccc-dddd").await;
    assert_invalid(&wrong);
    let unknown = verify(&app, "nobody", &pw).await;
    assert_invalid(&unknown);
    assert_eq!(wrong.1, unknown.1);
    // Another user's password does not work for alice.
    assert_invalid(&verify(&app, "alice", &bob_pw).await);
    assert_invalid(&verify(&app, "alice", "").await);
    assert_invalid(&verify(&app, "alice", &"x".repeat(5000)).await);
}

#[tokio::test]
async fn disabled_user_fails_verify() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    let (s, b) = app
        .api(
            Method::PATCH,
            &format!("/api/admin/users/{id}"),
            Some(&admin),
            json!({"disabled": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_invalid(&verify(&app, "alice", &pw).await);
}

#[tokio::test]
async fn disabling_a_user_deletes_app_passwords() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = user(&app, "alice").await;
    create(&app, &tok, "Phone").await;
    assert_eq!(count(&app, &id), 1);
    app.api(
        Method::PATCH,
        &format!("/api/admin/users/{id}"),
        Some(&admin),
        json!({"disabled": true}),
    )
    .await;
    assert_eq!(count(&app, &id), 0);
}

#[tokio::test]
async fn user_who_must_change_password_fails_verify_and_cannot_manage() {
    let app = TestApp::spawn().await;
    let (id, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    app.state
        .db
        .with(|c| {
            c.execute(
                "UPDATE users SET must_change_password = 1 WHERE id = ?1",
                [&id],
            )
        })
        .unwrap();
    assert_invalid(&verify(&app, "alice", &pw).await);
    let (s, b) = app
        .api(
            Method::GET,
            "/api/me/app-passwords",
            Some(&tok),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["code"], "password_change_required");
}

#[tokio::test]
async fn emailed_reset_deletes_app_passwords() {
    let app = TestApp::spawn_with_mail().await;
    let (id, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    app.api(
        Method::POST,
        "/api/password/forgot",
        None,
        json!({"email": "alice@example.com"}),
    )
    .await;
    let (s, b) = app
        .api(
            Method::POST,
            "/api/password/reset",
            None,
            json!({"token": app.last_mail_token(), "new_password": "another long password"}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(count(&app, &id), 0);
    assert_invalid(&verify(&app, "alice", &pw).await);
}

#[tokio::test]
async fn admin_reset_deletes_app_passwords() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = user(&app, "alice").await;
    create(&app, &tok, "Phone").await;
    let (s, b) = app
        .api(
            Method::POST,
            &format!("/api/admin/users/{id}/reset-password"),
            Some(&admin),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(count(&app, &id), 0);
}

#[tokio::test]
async fn label_is_validated() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    for label in ["", "   ", &"x".repeat(65)] {
        let (s, b) = app
            .api(
                Method::POST,
                "/api/me/app-passwords",
                Some(&tok),
                json!({"label": label}),
            )
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(b["code"], "validation");
    }
    create(&app, &tok, &"x".repeat(64)).await;
}

#[tokio::test]
async fn at_most_25_app_passwords() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, other) = user(&app, "bob").await;
    for i in 0..25 {
        create(&app, &tok, &format!("d{i}")).await;
    }
    let (s, b) = app
        .api(
            Method::POST,
            "/api/me/app-passwords",
            Some(&tok),
            json!({"label": "one too many"}),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["code"], "conflict");
    assert!(b["message"].as_str().unwrap().contains("25"));
    create(&app, &other, "fine").await;
}

#[tokio::test]
async fn cannot_delete_foreign_or_malformed_ids() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, bob) = user(&app, "bob").await;
    let (id, pw) = create(&app, &tok, "Phone").await;
    for path in [
        format!("/api/me/app-passwords/{id}"),
        format!("/api/me/app-passwords/{}", uuid::Uuid::new_v4()),
        "/api/me/app-passwords/not-a-uuid".to_string(),
    ] {
        let (s, b) = app
            .api(Method::DELETE, &path, Some(&bob), Value::Null)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(b["code"], "not_found");
    }
    assert_eq!(verify(&app, "alice", &pw).await.0, StatusCode::OK);
    assert!(list(&app, &bob).await.is_empty());
}

#[tokio::test]
async fn management_requires_a_session() {
    let app = TestApp::spawn().await;
    let (s, _) = app
        .api(Method::GET, "/api/me/app-passwords", None, Value::Null)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn verify_requires_service_secret() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    let r = app
        .http
        .post(format!("{}/api/app-passwords/verify", app.base))
        .json(&json!({"username": "alice", "password": pw}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

/// Like `verify`, but as a device at `ip`.
async fn verify_from(app: &TestApp, ip: &str, username: &str, password: &str) -> StatusCode {
    app.http
        .post(format!("{}/api/app-passwords/verify", app.base))
        .header("X-Service-Secret", support::SERVICE_SECRET)
        .header("X-Forwarded-For", ip)
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
        .status()
}

/// Wrong passwords from `ip` until it is locked.
async fn lock_address(app: &TestApp, ip: &str) {
    for _ in 0..20 {
        assert_eq!(
            verify_from(app, ip, "alice", "aaaa-bbbb-cccc-dddd").await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        verify_from(app, ip, "alice", "aaaa-bbbb-cccc-dddd").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn wrong_passwords_lock_the_address() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    lock_address(&app, "10.0.0.1").await;
    assert_eq!(
        verify_from(&app, "10.0.0.1", "alice", &pw).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn wrong_passwords_do_not_lock_the_username() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    lock_address(&app, "10.0.0.1").await;
    assert_eq!(
        verify_from(&app, "10.0.0.2", "alice", &pw).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn successful_verifies_do_not_lock_the_bridge() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (_, pw) = create(&app, &tok, "Phone").await;
    for _ in 0..30 {
        assert_eq!(verify(&app, "alice", &pw).await.0, StatusCode::OK);
    }
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let app = TestApp::spawn().await;
    let (_, tok) = user(&app, "alice").await;
    let (id, pw) = create(&app, &tok, "Phone").await;
    let last = |app: &TestApp| -> Option<i64> {
        app.state
            .db
            .with(|c| {
                c.query_row(
                    "SELECT last_used FROM app_passwords WHERE id = ?1",
                    [&id],
                    |r| r.get(0),
                )
            })
            .unwrap()
    };
    assert_eq!(last(&app), None);
    verify(&app, "alice", &pw).await;
    let first = last(&app).unwrap();
    // Pretend the previous write was 10 seconds ago: no new write.
    app.state
        .db
        .with(|c| c.execute("UPDATE app_passwords SET last_used = ?1", [first - 10]))
        .unwrap();
    verify(&app, "alice", &pw).await;
    assert_eq!(last(&app), Some(first - 10));
    // Two minutes ago: written again.
    app.state
        .db
        .with(|c| c.execute("UPDATE app_passwords SET last_used = ?1", [first - 120]))
        .unwrap();
    verify(&app, "alice", &pw).await;
    assert!(last(&app).unwrap() >= first);
}
