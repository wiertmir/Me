#![allow(dead_code)]
use auth_service::{AppState, Config, app, build_state};
use axum::http::{Method, StatusCode};
use serde_json::Value;

pub const SERVICE_SECRET: &str = "test-service-secret";

pub struct TestApp {
    pub base: String,
    pub http: reqwest::Client,
    pub state: AppState,
    pub seed_password: String,
    _dir: tempfile::TempDir,
}

impl TestApp {
    pub async fn spawn() -> Self {
        Self::spawn_with(|_| {}).await
    }

    pub async fn spawn_with(f: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests(dir.path().to_path_buf(), SERVICE_SECRET);
        f(&mut cfg);
        let (state, seed) = build_state(cfg).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            http: reqwest::Client::new(),
            state,
            seed_password: seed.unwrap_or_default(),
            _dir: dir,
        }
    }

    /// Sends X-Service-Secret, optional bearer session; returns status and JSON (Null if empty).
    pub async fn api(&self, method: Method, path: &str, session: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .header("X-Service-Secret", SERVICE_SECRET);
        if let Some(s) = session {
            req = req.bearer_auth(s);
        }
        if !body.is_null() {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap();
        let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        (status, json)
    }

    /// Signs in the seed user, replaces the one-time password with "correct horse battery"; returns the session token.
    pub async fn admin_session(&self) -> String {
        let (s, b) = self
            .api(Method::POST, "/api/signin", None, serde_json::json!({"login": "wiertmir", "password": self.seed_password}))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let token = b["session_token"].as_str().unwrap().to_string();
        let (s, b) = self
            .api(
                Method::POST,
                "/api/password/change",
                Some(&token),
                serde_json::json!({"current_password": self.seed_password, "new_password": "correct horse battery"}),
            )
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
        token
    }
}
