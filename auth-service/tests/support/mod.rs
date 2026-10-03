#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use auth_service::{
    AppState, Config, app, build_state,
    mail::{Email, Mailer},
};
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
        Self::start(f, false).await
    }

    /// Like `spawn`, with an in-memory mailer so verification and reset mail is enabled.
    pub async fn spawn_with_mail() -> Self {
        Self::start(|_| {}, true).await
    }

    async fn start(f: impl FnOnce(&mut Config), mail: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests(dir.path().to_path_buf(), SERVICE_SECRET);
        f(&mut cfg);
        let (mut state, seed) = build_state(cfg).unwrap();
        if mail {
            state.mail = Mailer::Memory(Default::default());
        }
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

    fn outbox(&self) -> Arc<Mutex<Vec<Email>>> {
        match &self.state.mail {
            Mailer::Memory(m) => m.clone(),
            _ => panic!("app was not spawned with mail"),
        }
    }

    pub fn mail_count(&self) -> usize {
        self.outbox().lock().unwrap().len()
    }

    /// The `token=` value from the newest mail body.
    pub fn last_mail_token(&self) -> String {
        let outbox = self.outbox();
        let mails = outbox.lock().unwrap();
        let body = &mails.last().expect("no mail sent").body;
        let rest = body.split("token=").nth(1).expect("no token in mail");
        rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .next()
            .unwrap()
            .to_string()
    }

    pub fn last_mail_body(&self) -> String {
        self.outbox()
            .lock()
            .unwrap()
            .last()
            .expect("no mail sent")
            .body
            .clone()
    }

    /// Sends X-Service-Secret, optional bearer session; returns status and JSON (Null if empty).
    pub async fn api(
        &self,
        method: Method,
        path: &str,
        session: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
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
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, json)
    }

    /// Signs in the seed user, replaces the one-time password with "correct horse battery"; returns the session token.
    pub async fn admin_session(&self) -> String {
        let (s, b) = self
            .api(
                Method::POST,
                "/api/signin",
                None,
                serde_json::json!({"login": "wiertmir", "password": self.seed_password}),
            )
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
