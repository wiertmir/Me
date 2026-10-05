mod support;

use axum::http::StatusCode;
use support::Stack;

#[tokio::test]
async fn health_is_public() {
    let s = Stack::spawn().await;
    let r = s.req("GET", "/health").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn no_credentials_is_401_with_challenge() {
    let s = Stack::spawn().await;
    let r = s.req("PROPFIND", "/dav/").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers()["www-authenticate"], "Basic realm=\"Me\"");
}

#[tokio::test]
async fn wrong_password_is_401() {
    let s = Stack::spawn().await;
    let r = s
        .req("GET", "/dav/")
        .basic_auth("alice", Some("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers()["www-authenticate"], "Basic realm=\"Me\"");
}

#[tokio::test]
async fn malformed_basic_is_401() {
    let s = Stack::spawn().await;
    for auth in ["Basic !!!", "Basic YWxpY2U=", "Bearer abc", "Basic"] {
        let r = s
            .req("GET", "/dav/")
            .header("authorization", auth)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{auth}");
    }
}

#[tokio::test]
async fn rate_limited_is_429_with_retry_after() {
    let s = Stack::spawn().await;
    let (status, headers, _) = s.dav("GET", "/dav/", "limited", &[], "").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "7");
}

#[tokio::test]
async fn signed_in_request_passes() {
    let s = Stack::spawn().await;
    let (status, _, _) = s.dav("OPTIONS", "/dav/", "alice", &[], "").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn client_address_is_passed_on() {
    let s = Stack::spawn().await;
    let h = [("x-forwarded-for", "203.0.113.9, 10.0.0.1")];
    s.dav("GET", "/dav/", "alice", &h, "").await;
    assert_eq!(s.last_forwarded_for().as_deref(), Some("203.0.113.9"));
    s.dav("GET", "/dav/", "alice", &[], "").await;
    assert_eq!(s.last_forwarded_for(), None);
}

#[tokio::test]
async fn auth_service_down_is_503() {
    let s = Stack::spawn_auth_down().await;
    let (status, _, _) = s.dav("GET", "/dav/", "alice", &[], "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn password_may_contain_colons() {
    let s = Stack::spawn().await;
    let r = s
        .req("OPTIONS", "/dav/")
        .basic_auth("alice", Some("pw:x:y"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}
