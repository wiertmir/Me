mod support;
use axum::http::{Method, StatusCode};
use serde_json::Value;
use support::{SERVICE_SECRET, TestApp};

/// Sends a raw body with an arbitrary content type and the service secret.
async fn raw(
    app: &TestApp,
    method: Method,
    path: &str,
    content_type: &str,
    session: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut req = app
        .http
        .request(method, format!("{}{}", app.base, path))
        .header("X-Service-Secret", SERVICE_SECRET)
        .header("Content-Type", content_type)
        .body(body.to_string());
    if let Some(s) = session {
        req = req.bearer_auth(s);
    }
    let r = req.send().await.unwrap();
    let status = r.status();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn assert_validation((s, b): (StatusCode, Value), secret: &str) {
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["code"], "validation", "{b}");
    assert!(!b["message"].as_str().unwrap().contains(secret), "{b}");
}

#[tokio::test]
async fn malformed_json_is_json_422() {
    let app = TestApp::spawn().await;
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/signin",
            "application/json",
            None,
            r#"{"login": "hunter2-leak"#,
        )
        .await,
        "hunter2-leak",
    );
}

#[tokio::test]
async fn missing_or_wrong_typed_field_is_json_422() {
    let app = TestApp::spawn().await;
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/signin",
            "application/json",
            None,
            r#"{"login":"alice"}"#,
        )
        .await,
        "alice",
    );
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/signin",
            "application/json",
            None,
            r#"{"login":"alice","password":5}"#,
        )
        .await,
        "alice",
    );
}

#[tokio::test]
async fn wrong_content_type_is_json_422() {
    let app = TestApp::spawn().await;
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/signin",
            "text/plain",
            None,
            r#"{"login":"a","password":"b"}"#,
        )
        .await,
        "login",
    );
}

#[tokio::test]
async fn malformed_body_on_session_route_is_json_422() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/me/app-passwords",
            "application/json",
            Some(&admin),
            "nope",
        )
        .await,
        "nope",
    );
}

#[tokio::test]
async fn malformed_path_id_is_json_404() {
    let app = TestApp::spawn().await;
    let admin = app.admin_session().await;
    for (m, p) in [
        (Method::DELETE, "/api/me/sessions/not-a-uuid"),
        (Method::DELETE, "/api/me/app-passwords/not-a-uuid"),
        (Method::PATCH, "/api/admin/users/not-a-uuid"),
        (Method::POST, "/api/admin/users/not-a-uuid/reset-password"),
    ] {
        let body = if m == Method::PATCH {
            serde_json::json!({"disabled": true})
        } else {
            Value::Null
        };
        let (s, b) = app.api(m, p, Some(&admin), body).await;
        assert_eq!(
            (s, b["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("not_found")),
            "{p}"
        );
    }
}

#[tokio::test]
async fn method_not_allowed_on_known_api_path_is_json_405() {
    let app = TestApp::spawn().await;
    let (s, b) = app.api(Method::GET, "/api/signin", None, Value::Null).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::METHOD_NOT_ALLOWED, Some("method_not_allowed"))
    );
    let (s, b) = app.api(Method::GET, "/api/nope", None, Value::Null).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
}

#[tokio::test]
async fn token_endpoint_malformed_form_keeps_rfc_shape() {
    let app = TestApp::spawn().await;
    for (ct, body) in [
        ("application/json", r#"{"grant_type":"x"}"#),
        ("text/plain", "grant_type"),
    ] {
        let r = app
            .http
            .post(format!("{}/oauth/token", app.base))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{ct}");
        let b: Value = r.json().await.unwrap();
        assert_eq!(b["error"], "invalid_request", "{ct}: {b}");
    }
}

#[tokio::test]
async fn app_password_verify_wrong_credentials_stay_401_and_malformed_is_422() {
    let app = TestApp::spawn().await;
    let (s, b) = app
        .api(
            Method::POST,
            "/api/app-passwords/verify",
            None,
            serde_json::json!({"username": "wiertmir", "password": "wrong"}),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_credentials"))
    );
    assert_validation(
        raw(
            &app,
            Method::POST,
            "/api/app-passwords/verify",
            "application/json",
            None,
            r#"{"username":1}"#,
        )
        .await,
        "username",
    );
}
