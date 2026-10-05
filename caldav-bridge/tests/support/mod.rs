#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::post,
};
use caldav_bridge::{Config, app, build_state};
use serde_json::{Value, json};
use uuid::Uuid;

pub const ALICE: Uuid = Uuid::from_u128(0xa11ce);
pub const BOB: Uuid = Uuid::from_u128(0xb0b);

const AUTH_SECRET: &str = "test-auth-secret-0001";
const CALENDAR_SECRET: &str = "test-calendar-secret-0002";
const TASKS_SECRET: &str = "test-tasks-secret-0003";

pub struct Stack {
    pub base: String,
    http: reqwest::Client,
    calendar: String,
    tasks: String,
    forwarded_for: Arc<Mutex<Option<String>>>,
    _dirs: [tempfile::TempDir; 2],
}

/// Serves `router` on a free port; returns its base URL.
async fn serve(router: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    base
}

/// The verify endpoint of auth-service, for two known users, a rate-limited one and no one else.
async fn verify_stub(seen: Arc<Mutex<Option<String>>>) -> String {
    async fn verify(
        State(seen): State<Arc<Mutex<Option<String>>>>,
        headers: HeaderMap,
        Json(b): Json<Value>,
    ) -> impl IntoResponse {
        assert_eq!(headers["x-service-secret"], AUTH_SECRET);
        *seen.lock().unwrap() = headers
            .get("x-forwarded-for")
            .map(|v| v.to_str().unwrap().to_owned());
        let (user, pass) = (b["username"].as_str().unwrap(), b["password"].as_str());
        match (user, pass) {
            ("alice", Some("pw-alice")) => {
                Json(json!({"user_id": ALICE, "username": "alice"})).into_response()
            }
            ("alice", Some("pw:x:y")) => {
                Json(json!({"user_id": ALICE, "username": "alice"})).into_response()
            }
            ("bob", Some("pw-bob")) => {
                Json(json!({"user_id": BOB, "username": "bob"})).into_response()
            }
            ("limited", _) => {
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "7")]).into_response()
            }
            _ => StatusCode::UNAUTHORIZED.into_response(),
        }
    }
    serve(
        Router::new()
            .route("/api/app-passwords/verify", post(verify))
            .with_state(seen),
    )
    .await
}

impl Stack {
    pub async fn spawn() -> Self {
        let forwarded_for = Arc::new(Mutex::new(None));
        let auth = verify_stub(forwarded_for.clone()).await;
        Self::spawn_with_auth(&auth, forwarded_for).await
    }

    /// The stack with auth-service at a closed port.
    pub async fn spawn_auth_down() -> Self {
        Self::spawn_with_auth("http://127.0.0.1:1", Arc::default()).await
    }

    async fn spawn_with_auth(auth: &str, forwarded_for: Arc<Mutex<Option<String>>>) -> Self {
        let issuer = "http://127.0.0.1:1"; // only the service-secret path is used
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let cal_cfg = calendar_service::Config::for_tests(
            dirs[0].path().to_path_buf(),
            issuer,
            CALENDAR_SECRET,
        );
        let calendar = serve(calendar_service::app(
            calendar_service::build_state(cal_cfg).unwrap(),
        ))
        .await;
        let tasks_cfg =
            tasks_service::Config::for_tests(dirs[1].path().to_path_buf(), issuer, TASKS_SECRET);
        let tasks = serve(tasks_service::app(
            tasks_service::build_state(tasks_cfg).unwrap(),
        ))
        .await;
        let cfg = Config::for_tests(
            [auth, &calendar, &tasks],
            [AUTH_SECRET, CALENDAR_SECRET, TASKS_SECRET],
        );
        let base = serve(app(build_state(cfg).unwrap())).await;
        Self {
            base,
            http: reqwest::Client::new(),
            calendar,
            tasks,
            forwarded_for,
            _dirs: dirs,
        }
    }

    /// The `X-Forwarded-For` the verify stub saw last.
    pub fn last_forwarded_for(&self) -> Option<String> {
        self.forwarded_for.lock().unwrap().clone()
    }

    /// No credentials.
    pub fn req(&self, method: &str, path: &str) -> reqwest::RequestBuilder {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        self.http.request(m, format!("{}{}", self.base, path))
    }

    /// A `/dav` call with Basic auth as `user` and the password "pw-{user}".
    pub async fn dav(
        &self,
        method: &str,
        path: &str,
        user: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = self
            .req(method, path)
            .basic_auth(user, Some(format!("pw-{user}")))
            .body(body.to_owned());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        (status, headers, resp.text().await.unwrap())
    }

    /// Straight to the services as `user`; `url_path` starts with `/calendar/v1` or `/tasks/v1`.
    pub async fn rest(
        &self,
        m: Method,
        url_path: &str,
        user: Uuid,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (base, secret) = if url_path.starts_with("/calendar/") {
            (&self.calendar, CALENDAR_SECRET)
        } else {
            (&self.tasks, TASKS_SECRET)
        };
        let mut req = self
            .http
            .request(m, format!("{base}{url_path}"))
            .header("x-service-secret", secret)
            .header("x-user-id", user.to_string());
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }
}
