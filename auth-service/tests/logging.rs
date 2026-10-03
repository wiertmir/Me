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

#[tokio::test]
async fn path_secrets_are_not_logged() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = buf.clone();
    // Thread-local default: #[tokio::test] runs the server on this same thread.
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || Sink(sink.clone()))
            .with_ansi(false)
            .finish(),
    );
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
    let log = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("path=/api/auth-requests/{challenge}"),
        "route template is logged:\n{log}"
    );
    assert!(!log.contains(&secret), "challenge leaked into logs:\n{log}");
}
