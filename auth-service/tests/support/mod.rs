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
        cfg.clients.push(auth_service::config::ClientConfig {
            id: "test-client".into(),
            name: "Test Client".into(),
            redirect_uris: vec![
                "http://127.0.0.1/callback".into(),
                "https://app.example/cb".into(),
            ],
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        cfg.issuer = base.clone(); // the issuer must be reachable: resource servers fetch its JWKS
        f(&mut cfg);
        let (mut state, seed) = build_state(cfg).unwrap();
        if mail {
            state.mail = Mailer::Memory(Default::default());
        }
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
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

    /// Runs authorize, accept and the code exchange for the test client; returns the code and its PKCE verifier.
    pub async fn auth_code(
        &self,
        session: &str,
        scope: &str,
        redirect_uri: &str,
        nonce: Option<&str>,
    ) -> (String, String) {
        let (verifier, challenge) = pkce();
        let mut q = vec![
            ("response_type", "code"),
            ("client_id", "test-client"),
            ("redirect_uri", redirect_uri),
            ("scope", scope),
            ("state", "st4te"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ];
        if let Some(n) = nonce {
            q.push(("nonce", n));
        }
        let resp = self
            .http
            .get(format!("{}/oauth/authorize", self.base))
            .query(&q)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        let loc = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
        let ch = loc
            .query_pairs()
            .find(|(k, _)| k == "challenge")
            .unwrap()
            .1
            .to_string();
        let (s, b) = self
            .api(
                Method::POST,
                &format!("/api/auth-requests/{ch}/accept"),
                Some(session),
                Value::Null,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let to = url::Url::parse(b["redirect_to"].as_str().unwrap()).unwrap();
        (
            to.query_pairs()
                .find(|(k, _)| k == "code")
                .unwrap()
                .1
                .to_string(),
            verifier,
        )
    }

    /// POSTs a form to /oauth/token; returns status, headers and JSON.
    pub async fn token_post(
        &self,
        form: &[(&str, &str)],
    ) -> (StatusCode, reqwest::header::HeaderMap, Value) {
        let resp = self
            .http
            .post(format!("{}/oauth/token", self.base))
            .form(form)
            .send()
            .await
            .unwrap();
        (
            resp.status(),
            resp.headers().clone(),
            resp.json().await.unwrap_or(Value::Null),
        )
    }

    /// Runs authorize → accept → token for the test client; returns the token response JSON.
    pub async fn oauth_tokens(&self, session: &str, scope: &str) -> Value {
        let (code, verifier) = self
            .auth_code(session, scope, "http://127.0.0.1/callback", None)
            .await;
        let (s, _, b) = self
            .token_post(&[
                ("grant_type", "authorization_code"),
                ("client_id", "test-client"),
                ("code", &code),
                ("redirect_uri", "http://127.0.0.1/callback"),
                ("code_verifier", &verifier),
            ])
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }
}

/// A PKCE (verifier, S256 challenge) pair.
pub fn pkce() -> (String, String) {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    let verifier = auth_service::crypto::random_token();
    let challenge = B64.encode(<sha2::Sha256 as sha2::Digest>::digest(verifier.as_bytes()));
    (verifier, challenge)
}
