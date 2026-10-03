mod support;
use axum::http::{Method, StatusCode};
use serde_json::Value;
use support::TestApp;

#[tokio::test] async fn health_is_ok() {
    let app = TestApp::spawn().await;
    let r = app.http.get(format!("{}/health", app.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
}
#[tokio::test] async fn request_id_is_echoed_or_generated() {
    let app = TestApp::spawn().await;
    let r = app.http.get(format!("{}/health", app.base)).header("X-Request-Id", "abc").send().await.unwrap();
    assert_eq!(r.headers()["x-request-id"], "abc");
    let r = app.http.get(format!("{}/health", app.base)).send().await.unwrap();
    assert!(!r.headers()["x-request-id"].is_empty());
}
#[tokio::test] async fn openapi_document_is_served() {
    let app = TestApp::spawn().await;
    let doc: Value = app.http.get(format!("{}/api/openapi.json", app.base)).send().await.unwrap().json().await.unwrap();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1"));
    assert!(doc["paths"]["/health"].is_object());
}
#[tokio::test] async fn unknown_api_route_is_json_404() {
    let app = TestApp::spawn().await;
    let (s, b) = app.api(Method::GET, "/api/nope", None, Value::Null).await;
    assert_eq!((s, b["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
}
