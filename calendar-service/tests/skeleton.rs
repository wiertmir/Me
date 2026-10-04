mod support;
use axum::http::Method;
use serde_json::{Value, json};
use support::{ALICE, SERVICE_SECRET, TestApp, TestKey};

const LIST: &str = "/calendar/v1/calendars";

async fn status_and_body(r: reqwest::RequestBuilder) -> (u16, Value) {
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn health_is_public() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(app.req(Method::GET, "/health")).await;
    assert_eq!((s, b), (200, json!({"status": "ok"})));
}

#[tokio::test]
async fn no_credentials_is_401() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(app.req(Method::GET, LIST)).await;
    assert_eq!(s, 401);
    assert_eq!(b["code"], "unauthorized");
}

#[tokio::test]
async fn bearer_token_is_accepted() {
    let app = TestApp::spawn().await;
    let (s, _, b) = app.call(Method::GET, LIST, ALICE, None).await;
    assert_eq!((s.as_u16(), &b[0]["name"]), (200, &json!("Personal")));
}

#[tokio::test]
async fn expired_or_foreign_token_is_401() {
    let app = TestApp::spawn().await;
    let foreign = TestKey::new("k1").token(&app.issuer, ALICE, 300);
    let expired = TestKey::new("k1").token(&app.issuer, ALICE, -120);
    for t in [foreign, expired] {
        let (s, _) = status_and_body(app.req(Method::GET, LIST).bearer_auth(t)).await;
        assert_eq!(s, 401);
    }
}

#[tokio::test]
async fn service_secret_with_user_id() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(
        app.req(Method::GET, LIST)
            .header("X-Service-Secret", SERVICE_SECRET)
            .header("X-User-Id", ALICE.to_string()),
    )
    .await;
    assert_eq!((s, &b[0]["name"]), (200, &json!("Personal")));
}

#[tokio::test]
async fn wrong_secret_is_401_even_with_a_valid_token() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(
        app.req(Method::GET, LIST)
            .header("X-Service-Secret", "nope")
            .header("X-User-Id", ALICE.to_string())
            .bearer_auth(app.token(ALICE)),
    )
    .await;
    assert_eq!(s, 401);
    assert_eq!(b["code"], "unauthorized");
}

#[tokio::test]
async fn secret_without_user_id_is_401() {
    let app = TestApp::spawn().await;
    let base = || {
        app.req(Method::GET, LIST)
            .header("X-Service-Secret", SERVICE_SECRET)
    };
    assert_eq!(status_and_body(base()).await.0, 401);
    assert_eq!(
        status_and_body(base().header("X-User-Id", "abc")).await.0,
        401
    );
}

#[tokio::test]
async fn unknown_route_is_json_404() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(app.req(Method::GET, "/nope")).await;
    assert_eq!(s, 404);
    assert_eq!(b["code"], "not_found");
}

#[tokio::test]
async fn wrong_method_is_json_405() {
    let app = TestApp::spawn().await;
    let (s, b) = status_and_body(app.req(Method::DELETE, "/health")).await;
    assert_eq!(s, 405);
    assert_eq!(b["code"], "method_not_allowed");
}

#[tokio::test]
async fn request_id_is_echoed() {
    let app = TestApp::spawn().await;
    let r = app
        .req(Method::GET, "/health")
        .header("X-Request-Id", "abc")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["x-request-id"], "abc");
}
