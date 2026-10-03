mod support;
use auth_service::users::{self, NewUser};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use support::TestApp;
use uuid::Uuid;

const PW: &str = "correct horse battery";

async fn call(app: &TestApp, m: Method, path: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    app.api(m, path, Some(tok), body).await
}

async fn signin(app: &TestApp, login: &str, pw: &str) -> (StatusCode, Value) {
    app.api(
        Method::POST,
        "/api/signin",
        None,
        json!({"login": login, "password": pw}),
    )
    .await
}

fn code(r: &(StatusCode, Value)) -> (u16, Option<&str>) {
    (r.0.as_u16(), r.1["code"].as_str())
}

/// Admin creates the user; returns (id, temporary password).
async fn create(app: &TestApp, admin: &str, name: &str) -> (String, String) {
    let (s, b) = call(
        app,
        Method::POST,
        "/api/admin/users",
        admin,
        json!({"username": name, "email": format!("{name}@example.com")}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    (
        b["user"]["id"].as_str().unwrap().into(),
        b["temporary_password"].as_str().unwrap().into(),
    )
}

/// Creates a user and completes the forced password change; returns (id, usable session).
async fn active_user(app: &TestApp, admin: &str, name: &str) -> (String, String) {
    let (id, temp) = create(app, admin, name).await;
    let (_, b) = signin(app, name, &temp).await;
    let tok = b["session_token"].as_str().unwrap().to_string();
    let (s, _) = call(
        app,
        Method::POST,
        "/api/password/change",
        &tok,
        json!({"current_password": temp, "new_password": PW}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    (id, tok)
}

async fn me_status(app: &TestApp, tok: &str) -> StatusCode {
    call(app, Method::GET, "/api/me", tok, Value::Null).await.0
}

fn patch(id: &str) -> String {
    format!("/api/admin/users/{id}")
}

#[tokio::test]
async fn admin_creates_user_who_must_change_password() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (s, b) = call(
        &app,
        Method::POST,
        "/api/admin/users",
        &admin,
        json!({"username": " Bob ", "email": "Bob@Example.com", "display_name": " Bobby "}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let temp = b["temporary_password"].as_str().unwrap();
    assert_eq!(temp.len(), 16);
    assert!(!temp.contains(['0', 'O', '1', 'l', 'I']));
    assert_eq!(b["user"]["username"], "bob");
    assert_eq!(b["user"]["display_name"], "Bobby");
    assert_eq!(b["user"]["email_verified"], true);
    assert_eq!(b["user"]["is_admin"], false);
    assert_eq!(b["user"]["disabled"], false);
    let (s, b) = signin(&app, "bob", temp).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["user"]["must_change_password"], true);

    // Same conflict and validation rules as sign-up.
    for (u, e, want) in [
        ("BOB", "o@example.com", 409),
        ("carol", "bob@example.com", 409),
        ("a b", "c@example.com", 422),
        ("carol", "x<a@b.c>", 422),
    ] {
        let (s, _) = call(
            &app,
            Method::POST,
            "/api/admin/users",
            &admin,
            json!({"username": u, "email": e}),
        )
        .await;
        assert_eq!(s.as_u16(), want, "{u} {e}");
    }
}

#[tokio::test]
async fn non_admin_gets_403() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = active_user(&app, &admin, "bob").await;
    let routes = [
        (Method::GET, "/api/admin/users".to_string(), Value::Null),
        (
            Method::POST,
            "/api/admin/users".into(),
            json!({"username": "eve", "email": "eve@example.com"}),
        ),
        (Method::PATCH, patch(&id), json!({"is_admin": true})),
        (
            Method::POST,
            format!("{}/reset-password", patch(&id)),
            Value::Null,
        ),
    ];
    for (m, path, body) in routes {
        let r = call(&app, m.clone(), &path, &tok, body.clone()).await;
        assert_eq!(code(&r), (403, Some("forbidden")), "{m} {path}");
        assert_eq!(
            app.api(m.clone(), &path, None, body).await.0,
            StatusCode::UNAUTHORIZED,
            "{m} {path}"
        );
    }
    // Still not an admin afterwards.
    let (_, me) = call(&app, Method::GET, "/api/me", &tok, Value::Null).await;
    assert_eq!(me["is_admin"], false);
}

#[tokio::test]
async fn admin_who_must_change_password_is_blocked() {
    let app = TestApp::spawn().await;
    let (_, b) = signin(&app, "wiertmir", &app.seed_password).await;
    let tok = b["session_token"].as_str().unwrap();
    let r = call(&app, Method::GET, "/api/admin/users", tok, Value::Null).await;
    assert_eq!(code(&r), (403, Some("password_change_required")));
}

#[tokio::test]
async fn lists_users_ordered_by_username() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    create(&app, &admin, "zed").await;
    create(&app, &admin, "amy").await;
    let (s, b) = call(&app, Method::GET, "/api/admin/users", &admin, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let names: Vec<_> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["amy", "wiertmir", "zed"]);
}

#[tokio::test]
async fn last_admin_cannot_be_demoted_or_disabled() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (_, me) = call(&app, Method::GET, "/api/me", &admin, Value::Null).await;
    let me = patch(me["id"].as_str().unwrap());
    for body in [
        json!({"is_admin": false}),
        json!({"disabled": true}),
        json!({"disabled": true, "is_admin": false}),
    ] {
        let r = call(&app, Method::PATCH, &me, &admin, body).await;
        assert_eq!(code(&r), (409, Some("last_admin")));
    }
    assert_eq!(me_status(&app, &admin).await, StatusCode::OK);
}

#[tokio::test]
async fn demoting_one_of_two_admins_succeeds_and_disabled_admins_do_not_count() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (_, me) = call(&app, Method::GET, "/api/me", &admin, Value::Null).await;
    let me = patch(me["id"].as_str().unwrap());
    let (bob, bob_tok) = active_user(&app, &admin, "bob").await;
    let (s, b) = call(
        &app,
        Method::PATCH,
        &patch(&bob),
        &admin,
        json!({"is_admin": true}),
    )
    .await;
    assert_eq!((s, &b["is_admin"]), (StatusCode::OK, &json!(true)), "{b}");

    // Bob is disabled, so the seeded admin is the only enabled one again.
    let (s, _) = call(
        &app,
        Method::PATCH,
        &patch(&bob),
        &admin,
        json!({"disabled": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        code(&call(&app, Method::PATCH, &me, &admin, json!({"is_admin": false})).await),
        (409, Some("last_admin"))
    );

    // Re-enabled, either can be demoted.
    call(
        &app,
        Method::PATCH,
        &patch(&bob),
        &admin,
        json!({"disabled": false}),
    )
    .await;
    let (_, b) = signin(&app, "bob", PW).await;
    let bob_tok2 = b["session_token"].as_str().unwrap();
    let (s, b) = call(
        &app,
        Method::PATCH,
        &me,
        bob_tok2,
        json!({"is_admin": false}),
    )
    .await;
    assert_eq!((s, &b["is_admin"]), (StatusCode::OK, &json!(false)), "{b}");
    assert_eq!(me_status(&app, &bob_tok).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn concurrent_demotions_leave_one_admin() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (_, me) = call(&app, Method::GET, "/api/me", &admin, Value::Null).await;
    let me = me["id"].as_str().unwrap().to_string();
    let (bob, _) = active_user(&app, &admin, "bob").await;
    call(
        &app,
        Method::PATCH,
        &patch(&bob),
        &admin,
        json!({"is_admin": true}),
    )
    .await;
    let (_, b) = signin(&app, "bob", PW).await;
    let bob_tok = b["session_token"].as_str().unwrap().to_string();
    let (bob_path, me_path) = (patch(&bob), patch(&me));
    let (a, b) = tokio::join!(
        call(
            &app,
            Method::PATCH,
            &bob_path,
            &admin,
            json!({"is_admin": false})
        ),
        call(
            &app,
            Method::PATCH,
            &me_path,
            &bob_tok,
            json!({"is_admin": false})
        ),
    );
    let mut got = [a.0.as_u16(), b.0.as_u16()];
    got.sort();
    assert!(
        got[0] == 200 && [401, 403, 409].contains(&got[1]),
        "{got:?}"
    );
    let enabled_admins = users::list_all(&app.state.db)
        .unwrap()
        .iter()
        .filter(|u| u.is_admin && !u.disabled)
        .count();
    assert_eq!(enabled_admins, 1);
}

#[tokio::test]
async fn disabling_user_kills_sessions_and_blocks_signin() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = active_user(&app, &admin, "bob").await;
    assert_eq!(me_status(&app, &tok).await, StatusCode::OK);
    let (s, b) = call(
        &app,
        Method::PATCH,
        &patch(&id),
        &admin,
        json!({"disabled": true}),
    )
    .await;
    assert_eq!((s, &b["disabled"]), (StatusCode::OK, &json!(true)));
    assert_eq!(me_status(&app, &tok).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        code(&signin(&app, "bob", PW).await),
        (401, Some("invalid_credentials"))
    );
    let sessions: i64 = app
        .state
        .db
        .with(|c| {
            c.query_row(
                "SELECT count(*) FROM sessions WHERE user_id = ?1",
                [&id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(sessions, 0);

    let (s, b) = call(
        &app,
        Method::PATCH,
        &patch(&id),
        &admin,
        json!({"disabled": false}),
    )
    .await;
    assert_eq!((s, &b["disabled"]), (StatusCode::OK, &json!(false)));
    assert_eq!(signin(&app, "bob", PW).await.0, StatusCode::OK);
}

#[tokio::test]
async fn disabling_deletes_refresh_tokens() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, _) = active_user(&app, &admin, "bob").await;
    app.state
        .db
        .with(|c| c.execute("INSERT INTO refresh_tokens (token_hash, family_id, user_id, client_id, scope, expires_at) VALUES ('h', 'f', ?1, 'c', '', 9999999999)", [&id]))
        .unwrap();
    call(
        &app,
        Method::PATCH,
        &patch(&id),
        &admin,
        json!({"disabled": true}),
    )
    .await;
    let n: i64 = app
        .state
        .db
        .with(|c| c.query_row("SELECT count(*) FROM refresh_tokens", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn patch_validates_body_and_id() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, _) = create(&app, &admin, "bob").await;
    assert_eq!(
        code(&call(&app, Method::PATCH, &patch(&id), &admin, json!({})).await),
        (422, Some("validation"))
    );
    let unknown = Uuid::new_v4().to_string();
    assert_eq!(
        code(
            &call(
                &app,
                Method::PATCH,
                &patch(&unknown),
                &admin,
                json!({"disabled": true})
            )
            .await
        ),
        (404, Some("not_found"))
    );
    let reset = format!("{}/reset-password", patch(&unknown));
    assert_eq!(
        code(&call(&app, Method::POST, &reset, &admin, Value::Null).await),
        (404, Some("not_found"))
    );
}

#[tokio::test]
async fn admin_reset_password_replaces_password_and_revokes_everything() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (id, tok) = active_user(&app, &admin, "bob").await;
    let uid = Uuid::parse_str(&id).unwrap();
    auth_service::email_tokens::create(
        &app.state.db,
        uid,
        auth_service::email_tokens::Purpose::Reset,
    )
    .unwrap();
    app.state
        .db
        .with(|c| c.execute("INSERT INTO refresh_tokens (token_hash, family_id, user_id, client_id, scope, expires_at) VALUES ('h', 'f', ?1, 'c', '', 9999999999)", [&id]))
        .unwrap();

    let (s, b) = call(
        &app,
        Method::POST,
        &format!("{}/reset-password", patch(&id)),
        &admin,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let temp = b["temporary_password"].as_str().unwrap();
    assert_eq!(temp.len(), 16);
    assert_eq!(me_status(&app, &tok).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        code(&signin(&app, "bob", PW).await),
        (401, Some("invalid_credentials"))
    );
    let (s, b) = signin(&app, "bob", temp).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["user"]["must_change_password"], true);
    let left: i64 = app
        .state
        .db
        .with(|c| c.query_row("SELECT (SELECT count(*) FROM refresh_tokens) + (SELECT count(*) FROM email_tokens WHERE user_id = ?1)", [&id], |r| r.get(0)))
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn admin_reset_gives_social_only_user_a_password() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let u = users::create(
        &app.state.db,
        NewUser {
            username: "sue".into(),
            email: "sue@example.com".into(),
            email_verified: true,
            is_admin: false,
            must_change_password: false,
            password_hash: None,
        },
    )
    .unwrap();
    assert!(!u.has_password);
    let (s, b) = call(
        &app,
        Method::POST,
        &format!("{}/reset-password", patch(&u.id.to_string())),
        &admin,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        signin(&app, "sue", b["temporary_password"].as_str().unwrap())
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn user_lists_and_revokes_sessions() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (_, first) = active_user(&app, &admin, "bob").await;
    let (_, b) = signin(&app, "bob", PW).await;
    let second = b["session_token"].as_str().unwrap().to_string();

    // password change in active_user kept `first`; two live sessions.
    let (s, list) = call(&app, Method::GET, "/api/me/sessions", &second, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|x| x["current"] == true).count(), 1);
    let other = list.iter().find(|x| x["current"] == false).unwrap();
    for k in ["id", "created_at", "last_seen", "user_agent", "ip"] {
        assert!(other.get(k).is_some(), "{k}");
    }
    let (s, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/me/sessions/{}", other["id"].as_str().unwrap()),
        &second,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(me_status(&app, &first).await, StatusCode::UNAUTHORIZED);
    assert_eq!(me_status(&app, &second).await, StatusCode::OK);
    let (_, list) = call(&app, Method::GET, "/api/me/sessions", &second, Value::Null).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Deleting the current session works like sign-out.
    let cur = list[0]["id"].as_str().unwrap();
    let (s, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/me/sessions/{cur}"),
        &second,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(me_status(&app, &second).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cannot_revoke_someone_elses_or_unknown_session() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    let (_, bob) = active_user(&app, &admin, "bob").await;
    let (_, adm_list) = call(&app, Method::GET, "/api/me/sessions", &admin, Value::Null).await;
    let admins_id = adm_list[0]["id"].as_str().unwrap();
    let r = call(
        &app,
        Method::DELETE,
        &format!("/api/me/sessions/{admins_id}"),
        &bob,
        Value::Null,
    )
    .await;
    assert_eq!(code(&r), (404, Some("not_found")));
    assert_eq!(me_status(&app, &admin).await, StatusCode::OK);
    let r = call(
        &app,
        Method::DELETE,
        &format!("/api/me/sessions/{}", Uuid::new_v4()),
        &bob,
        Value::Null,
    )
    .await;
    assert_eq!(code(&r), (404, Some("not_found")));
}
