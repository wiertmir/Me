use std::time::Instant;

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

use crate::config::{LogConfig, LogFormat};

static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn init(cfg: &LogConfig) {
    let filter = EnvFilter::try_new(&cfg.level).unwrap_or_else(|_| EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter);
    match cfg.format {
        LogFormat::Pretty => b.init(),
        LogFormat::Json => b.json().init(),
    }
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
    let span = tracing::info_span!(
        "request",
        method = %req.method(),
        path = req.uri().path(),
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
