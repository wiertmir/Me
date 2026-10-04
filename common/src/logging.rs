use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use opentelemetry::{
    Context,
    propagation::{Extractor, TextMapPropagator},
    trace::{SpanKind, TracerProvider},
};
use opentelemetry_sdk::{
    Resource,
    error::OTelSdkResult,
    propagation::TraceContextPropagator,
    trace::{BatchSpanProcessor, SdkTracerProvider, Span, SpanData, SpanProcessor},
};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{EnvFilter, Layer, fmt::time::ChronoLocal, prelude::*};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub format: LogFormat,
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            level: "debug".into(),
        }
    }
}

static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// `service` names this process in exported traces.
pub fn init(cfg: &LogConfig, service: &'static str) {
    init_to(cfg, service, std::io::stdout)
}

/// For one-shot commands whose standard output is their result: the log goes to standard error.
pub fn init_stderr(cfg: &LogConfig, service: &'static str) {
    init_to(cfg, service, std::io::stderr)
}

fn init_to<W>(cfg: &LogConfig, service: &'static str, writer: W)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let mut filter = EnvFilter::try_new(&cfg.level).unwrap_or_else(|_| EnvFilter::new("debug"));
    // The trace exporter and its HTTP/2 plumbing report every batch and frame at debug.
    for noisy in ["opentelemetry", "h2", "tonic", "tower", "hyper_util"] {
        filter = filter.add_directive(format!("{noisy}=info").parse().expect("valid directive"));
    }
    // Local time, the same shapes auth-web logs: JSON lines carry the UTC offset, the console format leaves it out.
    let time = match cfg.format {
        LogFormat::Pretty => "%Y-%m-%d %H:%M:%S%.3f",
        LogFormat::Json => "%Y-%m-%dT%H:%M:%S%.3f%:z",
    };
    let fmt = tracing_subscriber::fmt::layer()
        .with_timer(ChronoLocal::new(time.into()))
        .with_writer(writer);
    let fmt = match cfg.format {
        LogFormat::Pretty => fmt.boxed(),
        LogFormat::Json => fmt.json().boxed(),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel_tracer(service).map(|t| tracing_opentelemetry::layer().with_tracer(t)))
        .init();
}

/// Spans are exported over OTLP (gRPC) when the standard `OTEL_EXPORTER_OTLP_ENDPOINT` variable is set,
/// as .NET Aspire does for the processes it starts; without it nothing is collected.
fn otel_tracer(service: &'static str) -> Option<opentelemetry_sdk::trace::Tracer> {
    std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT")?;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .inspect_err(|e| eprintln!("tracing disabled: {e}"))
        .ok()?;
    // ponytail: no flush on shutdown, the last batch (up to OTEL_BSP_SCHEDULE_DELAY) is lost when the
    // process is stopped; add graceful shutdown if those spans matter
    let provider = SdkTracerProvider::builder()
        .with_resource(Resource::builder().with_service_name(service).build())
        .with_span_processor(RequestSpans(BatchSpanProcessor::builder(exporter).build()))
        .build();
    Some(provider.tracer(service))
}

/// Gives the request spans their exported name ("GET /api/users") and kind. Done here and not with
/// `otel.name` / `otel.kind` span fields, which would be repeated in every log line of the request.
#[derive(Debug)]
struct RequestSpans<P>(P);

impl<P: SpanProcessor> SpanProcessor for RequestSpans<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.0.on_start(span, cx)
    }
    fn on_end(&self, mut span: SpanData) {
        if span.name == "request" {
            let field = |k| span.attributes.iter().find(|a| a.key.as_str() == k);
            if let (Some(method), Some(path)) = (field("method"), field("path")) {
                span.name = format!("{} {}", method.value, path.value).into();
                span.span_kind = SpanKind::Server;
            }
        }
        self.0.on_end(span)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource)
    }
}

struct Headers<'a>(&'a axum::http::HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.to_str().ok()
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Text a client typed, made safe to log: control characters (CR, LF, escapes…) removed so it cannot
/// forge or break log lines, and cut to 64 characters.
pub fn for_log(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// The route template (`/api/auth-requests/{challenge}`) when a route matched, so path parameters that
/// are secrets never reach the logs; the raw path only for unmatched requests.
pub fn logged_path(req: &Request) -> String {
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
    // Continue the caller's trace (auth-web sends `traceparent`); does nothing when tracing is off.
    let _ = span.set_parent(TraceContextPropagator::new().extract(&Headers(req.headers())));
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
