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
use icalendar::parser::{Component, read_calendar, unfold};
use roxmltree::{Document, Node};
use serde_json::{Value, json};
use uuid::Uuid;

pub const ALICE: Uuid = Uuid::from_u128(0xa11ce);
pub const BOB: Uuid = Uuid::from_u128(0xb0b);

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";

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

/// The verify endpoint of auth-service, for two known users (one also by her email address), a
/// rate-limited one and no one else.
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
            ("alice" | "alice@example.com", Some("pw-alice")) => {
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
        Self::spawn_with(&auth, false, forwarded_for).await
    }

    /// The stack with auth-service at a closed port.
    pub async fn spawn_auth_down() -> Self {
        Self::spawn_with("http://127.0.0.1:1", false, Arc::default()).await
    }

    /// The stack with the bridge's tasks-service at a closed port.
    pub async fn spawn_tasks_down() -> Self {
        let forwarded_for = Arc::new(Mutex::new(None));
        let auth = verify_stub(forwarded_for.clone()).await;
        Self::spawn_with(&auth, true, forwarded_for).await
    }

    async fn spawn_with(
        auth: &str,
        tasks_down: bool,
        forwarded_for: Arc<Mutex<Option<String>>>,
    ) -> Self {
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
        let bridge_tasks = if tasks_down {
            "http://127.0.0.1:1"
        } else {
            &tasks
        };
        let cfg = Config::for_tests(
            [auth, &calendar, bridge_tasks],
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

pub fn named<'a>(n: Node<'a, 'a>, ns: &str, name: &str) -> Option<Node<'a, 'a>> {
    n.descendants().find(|d| {
        d.is_element() && d.tag_name().name() == name && d.tag_name().namespace() == Some(ns)
    })
}

pub fn text(n: Node, ns: &str, name: &str) -> String {
    named(n, ns, name).unwrap().text().unwrap_or("").to_owned()
}

/// The `response` elements of a multistatus, keyed by their href.
pub fn responses<'a>(doc: &'a Document) -> Vec<(String, Node<'a, 'a>)> {
    doc.descendants()
        .filter(|n| n.tag_name().name() == "response" && n.tag_name().namespace() == Some(DAV))
        .map(|r| (text(r, DAV, "href"), r))
        .collect()
}

// ---- events ----

/// The path of alice's calendar, without a trailing slash.
pub async fn calendar(s: &Stack) -> String {
    let (_, calendars) = s
        .rest(Method::GET, "/calendar/v1/calendars", ALICE, None)
        .await;
    format!(
        "/dav/calendars/alice/c-{}",
        calendars[0]["id"].as_str().unwrap()
    )
}

pub async fn get(s: &Stack, cal: &str, uid: &str) -> (StatusCode, HeaderMap, String) {
    s.dav("GET", &format!("{cal}/{uid}.ics"), "alice", &[], "")
        .await
}

pub async fn etag(s: &Stack, cal: &str, uid: &str) -> String {
    let (st, h, _) = get(s, cal, uid).await;
    assert_eq!(st, StatusCode::OK);
    h["etag"].to_str().unwrap().to_owned()
}

/// The events calendar-service has under that uid: the single one or the series first.
pub async fn stored(s: &Stack, cal: &str, uid: &str) -> Vec<Value> {
    let id = cal.rsplit("/c-").next().unwrap();
    let (st, found) = s
        .rest(
            Method::GET,
            &format!("/calendar/v1/calendars/{id}/by-uid?uid={uid}"),
            ALICE,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    found.as_array().unwrap().clone()
}

/// The lines of one `VEVENT` of an answer as (name, parameters, value), and the `TRIGGER`s of its alarms.
pub struct Event {
    pub props: Vec<(String, String, String)>,
    pub alarms: Vec<String>,
}

impl Event {
    fn of(c: &Component) -> Self {
        assert_eq!(c.name, "VEVENT");
        let props = c.properties.iter().map(|p| {
            let params = p.params.iter().map(|q| {
                let val = q.val.as_ref().map(|v| v.to_string());
                format!("{}={}", q.key, val.unwrap_or_default())
            });
            let params: Vec<_> = params.collect();
            (p.name.to_string(), params.join(";"), p.val.to_string())
        });
        let alarms = c.components.iter().map(|a| {
            assert_eq!(a.name, "VALARM");
            assert_eq!(a.find_prop("ACTION").unwrap().val, "DISPLAY");
            a.find_prop("TRIGGER").unwrap().val.to_string()
        });
        Self {
            props: props.collect(),
            alarms: alarms.collect(),
        }
    }

    /// Every `VEVENT` of a body.
    pub fn all(body: &str) -> Vec<Self> {
        let unfolded = unfold(body);
        let cal = read_calendar(&unfolded).unwrap();
        cal.components.iter().map(Self::of).collect()
    }

    /// The parameters and value of each line of that name.
    pub fn lines(&self, name: &str) -> Vec<(&str, &str)> {
        let named = self.props.iter().filter(|p| p.0 == name);
        named.map(|p| (p.1.as_str(), p.2.as_str())).collect()
    }

    pub fn val(&self, name: &str) -> Option<&str> {
        let all = self.lines(name);
        assert!(all.len() <= 1, "{name} more than once");
        all.first().map(|l| l.1)
    }
}
