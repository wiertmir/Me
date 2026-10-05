#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    http::{HeaderMap, Method, StatusCode},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use p256::{
    ecdsa::SigningKey,
    elliptic_curve::Generate as _,
    pkcs8::{EncodePrivateKey, LineEnding},
};
use serde_json::{Value, json};
use tasks_service::{Config, app, build_state_with_clock};
use uuid::Uuid;

pub const SERVICE_SECRET: &str = "test-service-secret";
pub const ALICE: Uuid = Uuid::from_u128(0xa11ce);
pub const BOB: Uuid = Uuid::from_u128(0xb0b);

pub struct TestKey {
    kid: String,
    enc: EncodingKey,
    pub jwk: Value,
}

impl TestKey {
    pub fn new(kid: &str) -> Self {
        let key = SigningKey::generate_from_rng(&mut rand::rng());
        let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
        let point = key.verifying_key().to_sec1_point(false);
        let jwk = json!({"kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": B64.encode(point.x().unwrap()), "y": B64.encode(point.y().unwrap())});
        Self {
            kid: kid.into(),
            enc: EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
            jwk,
        }
    }

    /// A valid access token for `user`, expiring in `exp_in` seconds.
    pub fn token(&self, issuer: &str, user: Uuid, exp_in: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = json!({"iss": issuer, "sub": user, "aud": "me-api", "exp": now + exp_in,
            "iat": now, "scope": "openid tasks", "preferred_username": "test", "client_id": "c"});
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&h, &claims, &self.enc).unwrap()
    }
}

/// Serves `keys` at `/.well-known/jwks.json` on a free port; returns the base URL, which doubles as issuer.
pub async fn serve_jwks(keys: Vec<Value>) -> String {
    let app = Router::new().route(
        "/.well-known/jwks.json",
        get(move || async move { Json(json!({"keys": keys})) }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    issuer
}

pub struct TestApp {
    pub base: String,
    pub http: reqwest::Client,
    pub issuer: String,
    key: TestKey,
    now: Arc<Mutex<DateTime<Utc>>>,
    _dir: tempfile::TempDir,
}

impl TestApp {
    pub async fn spawn() -> Self {
        let key = TestKey::new("k1");
        let issuer = serve_jwks(vec![key.jwk.clone()]).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_tests(dir.path().to_path_buf(), &issuer, SERVICE_SECRET);
        let now = Arc::new(Mutex::new("2026-10-05T12:00:00Z".parse().unwrap()));
        let clock = {
            let now = now.clone();
            Arc::new(move || *now.lock().unwrap())
        };
        let router = app(build_state_with_clock(cfg, clock).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            http: reqwest::Client::new(),
            issuer,
            key,
            now,
            _dir: dir,
        }
    }

    /// A direct connection to the service's database, for corrupting rows a test needs unreadable.
    pub fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self._dir.path().join("tasks.db")).unwrap()
    }

    /// Moves the service's clock to `rfc3339`.
    pub fn set_now(&self, rfc3339: &str) {
        *self.now.lock().unwrap() = rfc3339.parse().unwrap();
    }

    pub fn token(&self, user: Uuid) -> String {
        self.key.token(&self.issuer, user, 300)
    }

    /// Bearer-authenticated call as `user`; returns status, headers, JSON body (Null when empty).
    pub async fn call(
        &self,
        m: Method,
        path: &str,
        user: Uuid,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = self.req(m, path).bearer_auth(self.token(user));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = resp.bytes().await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
    }

    /// No credentials, for auth tests.
    pub fn req(&self, m: Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(m, format!("{}{}", self.base, path))
    }
}
