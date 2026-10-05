pub mod auth;
pub mod backend;
pub mod config;
mod dav;
mod ical;
mod path;
mod xml;

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use common::ApiError;
pub use config::Config;
use serde_json::json;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
}

pub fn build_state(cfg: Config) -> anyhow::Result<AppState> {
    let secrets = [
        (&cfg.auth_secret, "ME_CALDAV__AUTH_SECRET"),
        (&cfg.calendar_secret, "ME_CALDAV__CALENDAR_SECRET"),
        (&cfg.tasks_secret, "ME_CALDAV__TASKS_SECRET"),
    ];
    for (secret, var) in secrets {
        if common::secret::check(secret, cfg.listen, var)? {
            tracing::warn!(
                "{var} is the example secret from config.example.toml; change it before anything but local development"
            );
        }
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(AppState {
        cfg: Arc::new(cfg),
        http,
    })
}

/// What a client gets for a failed request: plain text, or, with a precondition, a CalDAV error document.
pub struct DavError {
    status: StatusCode,
    precondition: Option<&'static str>,
    message: String,
    headers: HeaderMap,
}

impl DavError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            precondition: None,
            message: message.into(),
            headers: HeaderMap::new(),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn precondition(mut self, name: &'static str) -> Self {
        self.precondition = Some(name);
        self
    }

    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }
}

impl IntoResponse for DavError {
    fn into_response(self) -> Response {
        let (content_type, body) = match self.precondition {
            Some(p) => {
                let text = self
                    .message
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                (
                    "application/xml; charset=utf-8",
                    format!(
                        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><C:{p}/>{text}</D:error>"
                    ),
                )
            }
            None => ("text/plain; charset=utf-8", self.message),
        };
        (
            self.status,
            self.headers,
            [(header::CONTENT_TYPE, content_type)],
            body,
        )
            .into_response()
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// `/dav/{*rest}` is one route, so the request log shows that template and never the item name in `rest`.
fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/dav", any(dav::handle))
        .route("/dav/", any(dav::handle))
        .route("/dav/{*rest}", any(dav::handle))
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

pub fn app(state: AppState) -> Router {
    routes()
        .with_state(state)
        .fallback(not_found)
        .layer(middleware::from_fn(common::logging::request_layer))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{extract::Request, middleware::Next};

    use super::*;

    #[tokio::test]
    async fn logged_path_has_no_item_name() {
        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
        async fn record(req: Request, next: Next) -> Response {
            SEEN.lock()
                .unwrap()
                .push(common::logging::logged_path(&req));
            next.run(req).await
        }
        let urls = ["http://127.0.0.1:1"; 3];
        let cfg = Config::for_tests(urls, ["test-service-secret"; 3]);
        let router = routes()
            .with_state(build_state(cfg).unwrap())
            .fallback(not_found)
            .layer(middleware::from_fn(record));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        for path in ["/dav/calendars/abc/secret-uid.ics", "/dav/", "/dav"] {
            reqwest::get(format!("{base}{path}")).await.unwrap();
        }
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen, ["/dav/{*rest}", "/dav/", "/dav"]);
    }

    #[tokio::test]
    async fn errors_are_xml_with_a_precondition_and_text_without() {
        let body = |e: DavError| async {
            let b = axum::body::to_bytes(e.into_response().into_body(), 4096).await;
            String::from_utf8(b.unwrap().to_vec()).unwrap()
        };
        let xml = DavError::new(StatusCode::FORBIDDEN, "a < b").precondition("valid-calendar-data");
        assert!(
            body(xml)
                .await
                .ends_with("<C:valid-calendar-data/>a &lt; b</D:error>")
        );
        assert_eq!(
            body(DavError::new(StatusCode::BAD_GATEWAY, "down")).await,
            "down"
        );
    }
}
