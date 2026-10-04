use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

use crate::config::{LogConfig, LogFormat};

static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn init(cfg: &LogConfig) {
    init_to(cfg, std::io::stdout)
}

/// For one-shot commands whose standard output is their result: the log goes to standard error.
pub fn init_stderr(cfg: &LogConfig) {
    init_to(cfg, std::io::stderr)
}

fn init_to<W>(cfg: &LogConfig, writer: W)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(&cfg.level).unwrap_or_else(|_| EnvFilter::new("info"));
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match cfg.format {
        LogFormat::Pretty => b.init(),
        LogFormat::Json => b.json().init(),
    }
}

/// Text a client typed, made safe to log: control characters (CR, LF, escapes…) removed so it cannot
/// forge or break log lines, and cut to 64 characters.
pub fn for_log(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// The route template (`/api/auth-requests/{challenge}`) when a route matched, so path parameters that
/// are secrets never reach the logs; the raw path only for unmatched requests.
pub(crate) fn logged_path(req: &Request) -> String {
    req.extensions()
        .get::<MatchedPath>()
        .map_or_else(|| req.uri().path().to_owned(), |m| m.as_str().to_owned())
}

/// Reuses or generates `X-Request-Id`, wraps the request in a span and echoes the header.
pub async fn request_layer(req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(&REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let path = logged_path(&req);
    let span = tracing::info_span!(
        "request",
        method = %req.method(),
        path = %path,
        request_id = %request_id,
        status = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    let start = Instant::now();
    let mut resp = next.run(req).instrument(span.clone()).await;
    span.record("status", resp.status().as_u16());
    span.record("duration_ms", start.elapsed().as_millis() as u64);
    span.in_scope(|| tracing::info!("request handled"));
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert(REQUEST_ID.clone(), v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::for_log;

    #[test]
    fn for_log_strips_control_characters_and_truncates() {
        assert_eq!(for_log("alice"), "alice");
        assert_eq!(for_log("a\r\nb\tc\u{0}d\u{1b}[31m\u{85}e"), "abcd[31me");
        assert_eq!(for_log(&"é".repeat(200)), "é".repeat(64));
        assert_eq!(for_log(""), "");
    }
}
