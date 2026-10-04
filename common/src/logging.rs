use std::{
    fs::File,
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
    time::Instant,
};

use axum::{
    extract::{MatchedPath, Request},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use chrono::{Local, NaiveDate};
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
use tracing_subscriber::{
    EnvFilter, Layer,
    field::RecordFields,
    fmt::{
        FormatFields,
        format::{DefaultFields, Writer},
        time::ChronoLocal,
    },
    prelude::*,
};

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
    /// When set, the log is also written to `<dir>/<service>-YYYYMMDD.log`, a new file each day.
    pub dir: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            level: "debug".into(),
            dir: None,
        }
    }
}

/// Log files older than the newest this many are deleted; the same number Serilog keeps for auth-web.
const KEEP_FILES: usize = 31;

/// Appends to `<dir>/<service>-YYYYMMDD.log` by local date, moves to a new file when the date changes and
/// then deletes all but the newest `KEEP_FILES`.
struct DailyFile {
    dir: PathBuf,
    service: &'static str,
    open: Option<(NaiveDate, File)>,
}

impl DailyFile {
    fn write_on(&mut self, day: NaiveDate, buf: &[u8]) -> io::Result<()> {
        if self.open.as_ref().is_none_or(|(d, _)| *d != day) {
            std::fs::create_dir_all(&self.dir)?;
            let name = format!("{}-{}.log", self.service, day.format("%Y%m%d"));
            let file = File::options()
                .create(true)
                .append(true)
                .open(self.dir.join(name))?;
            self.open = Some((day, file));
            self.prune();
        }
        self.open.as_mut().expect("opened above").1.write_all(buf)
    }

    /// Failing to delete an old file must not stop the logging, so errors are ignored.
    fn prune(&self) {
        let prefix = format!("{}-", self.service);
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut ours: Vec<PathBuf> = entries
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.strip_prefix(&prefix)
                        .and_then(|rest| rest.strip_suffix(".log"))
                        .is_some_and(|day| {
                            day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit())
                        })
                })
            })
            .collect();
        ours.sort(); // the date in the name sorts as text
        for old in ours.iter().rev().skip(KEEP_FILES) {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// The console's field format as a type of its own. A span's fields are formatted once per formatter type
/// and kept; sharing the console's type would put its colour codes into the file.
struct PlainFields(DefaultFields);

impl<'w> FormatFields<'w> for PlainFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'w>, fields: R) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

impl Write for DailyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_on(Local::now().date_naive(), buf)
            .map(|()| buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // nothing is buffered here
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
    // The same lines again, without colours, in a file per day. A directory that cannot be written is
    // reported once here; the service runs on with the console log alone.
    let file = cfg.dir.clone().and_then(|dir| {
        let mut out = DailyFile {
            dir,
            service,
            open: None,
        };
        match out.write_on(Local::now().date_naive(), b"") {
            Ok(()) => Some(out),
            Err(e) => {
                eprintln!("no log file in {}: {e}", out.dir.display());
                None
            }
        }
    });
    let file = file.map(|out| {
        let layer = tracing_subscriber::fmt::layer()
            .with_timer(ChronoLocal::new(time.into()))
            .with_ansi(false)
            .fmt_fields(PlainFields(DefaultFields::new()))
            .with_writer(Mutex::new(out));
        match cfg.format {
            LogFormat::Pretty => layer.boxed(),
            LogFormat::Json => layer.json().boxed(),
        }
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(file)
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
    use super::*;

    #[test]
    fn daily_file_starts_a_file_per_day_and_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("me-log-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // 40 earlier days of this service, and files that are not its own
        let day = |n: u64| NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(n);
        for n in 0..40 {
            std::fs::write(
                dir.join(format!("svc-{}.log", day(n).format("%Y%m%d"))),
                "old",
            )
            .unwrap();
        }
        std::fs::write(dir.join("svc-notes.log"), "keep").unwrap();
        std::fs::write(dir.join("other-20260101.log"), "keep").unwrap();

        let mut out = DailyFile {
            dir: dir.clone(),
            service: "svc",
            open: None,
        };
        out.write_on(day(40), b"first\n").unwrap();
        out.write_on(day(40), b"second\n").unwrap();
        out.write_on(day(41), b"next day\n").unwrap();

        let read =
            |n| std::fs::read_to_string(dir.join(format!("svc-{}.log", day(n).format("%Y%m%d"))));
        assert_eq!(read(40).unwrap(), "first\nsecond\n");
        assert_eq!(read(41).unwrap(), "next day\n");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("svc-2"))
            .collect();
        names.sort();
        assert_eq!(names.len(), KEEP_FILES);
        assert_eq!(names[0], format!("svc-{}.log", day(11).format("%Y%m%d")));
        assert!(dir.join("svc-notes.log").exists() && dir.join("other-20260101.log").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn for_log_strips_control_characters_and_truncates() {
        assert_eq!(for_log("alice"), "alice");
        assert_eq!(for_log("a\r\nb\tc\u{0}d\u{1b}[31m\u{85}e"), "abcd[31me");
        assert_eq!(for_log(&"é".repeat(200)), "é".repeat(64));
        assert_eq!(for_log(""), "");
    }
}
