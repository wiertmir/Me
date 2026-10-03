mod support;
use std::sync::{Arc, Mutex};

use axum::http::Method;
use serde_json::Value;
use support::TestApp;

struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captures this thread's log output. #[tokio::test] runs the server on the same thread.
fn capture() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = buf.clone();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || Sink(sink.clone()))
            .with_ansi(false)
            .finish(),
    );
    (buf, guard)
}

fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
async fn typed_login_cannot_forge_log_lines() {
    let (buf, _guard) = capture();
    let app = TestApp::spawn().await;
    let login = format!(
        "mallory\r\nevent=signin outcome=success {}",
        "x".repeat(200)
    );
    let (s, _) = app
        .api(
            Method::POST,
            "/api/signin",
            None,
            serde_json::json!({"login": login, "password": "definitely wrong password"}),
        )
        .await;
    assert_eq!(s, 401);
    let log = text(&buf);
    assert!(log.contains("login=malloryevent=signin"), "{log}");
    assert!(
        !log.contains("mallory\r") && !log.contains("mallory\n"),
        "{log}"
    );
    assert!(
        !log.contains(&"x".repeat(65)),
        "login not truncated:\n{log}"
    );
}

#[tokio::test]
async fn example_service_secret_is_warned_about_on_loopback() {
    let (buf, _guard) = capture();
    TestApp::spawn_with(|c| c.service_secret = auth_service::EXAMPLE_SERVICE_SECRET.into()).await;
    let log = text(&buf);
    assert!(
        log.contains("WARN") && log.contains("example service_secret"),
        "{log}"
    );
}

#[tokio::test]
async fn path_secrets_are_not_logged() {
    let (buf, _guard) = capture();
    let app = TestApp::spawn().await;
    let (_, challenge) = support::pkce();
    let r = app
        .http
        .get(format!("{}/oauth/authorize", app.base))
        .query(&[
            ("response_type", "code"),
            ("client_id", "test-client"),
            ("redirect_uri", "https://app.example/cb"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    let loc = url::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let secret = loc
        .query_pairs()
        .find(|(k, _)| k == "challenge")
        .unwrap()
        .1
        .to_string();
    let (s, _) = app
        .api(
            Method::GET,
            &format!("/api/auth-requests/{secret}"),
            None,
            Value::Null,
        )
        .await;
    assert_eq!(s, 200);
    let (s, _) = app
        .api(
            Method::POST,
            &format!("/api/auth-requests/{secret}/accept"),
            None,
            Value::Null,
        )
        .await;
    assert_eq!(s, 401);
    let log = text(&buf);
    assert!(
        log.contains("path=/api/auth-requests/{challenge}"),
        "route template is logged:\n{log}"
    );
    assert!(!log.contains(&secret), "challenge leaked into logs:\n{log}");
}
