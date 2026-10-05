pub mod calendars;
pub mod config;
pub mod db;
pub mod events;
pub mod openapi;
pub mod range;
pub mod recur;

use std::sync::Arc;

use axum::{Json, Router, extract::FromRef, http::StatusCode, middleware, response::IntoResponse};
use common::{ApiError, ServiceSecret, TokenVerifier};
use serde::Serialize;
use utoipa::{OpenApi as _, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub use common::Caller;
pub use common::EXAMPLE_SERVICE_SECRET;
pub use config::Config;
pub use db::Db;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub verifier: Arc<TokenVerifier>,
}

impl FromRef<AppState> for Arc<TokenVerifier> {
    fn from_ref(s: &AppState) -> Self {
        s.verifier.clone()
    }
}

impl FromRef<AppState> for ServiceSecret {
    fn from_ref(s: &AppState) -> Self {
        ServiceSecret(s.cfg.service_secret.as_str().into())
    }
}

/// Opens the database under `cfg.data_dir`.
pub fn build_state(cfg: Config) -> anyhow::Result<AppState> {
    if common::secret::check(
        &cfg.service_secret,
        cfg.listen,
        "ME_CALENDAR__SERVICE_SECRET",
    )? {
        tracing::warn!(
            "service_secret is the example service_secret from config.example.toml; change it before anything but local development"
        );
    }
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = Db::open(&cfg.data_dir.join("calendar.db"), db::SCHEMA)?;
    let mut verifier = TokenVerifier::new(cfg.issuer.clone(), cfg.audience.clone());
    if let Some(url) = &cfg.jwks_url {
        verifier = verifier.with_jwks_url(url.clone());
    }
    Ok(AppState {
        cfg: Arc::new(cfg),
        db,
        verifier: Arc::new(verifier),
    })
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: &'static str,
}

/// Liveness probe
///
/// Public: no credentials.
#[utoipa::path(get, path = "/health", security(()), responses((status = 200, description = "the service is up", body = Health)))]
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

async fn method_not_allowed() -> impl IntoResponse {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed on this route",
    )
}

pub fn app(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .routes(routes!(health))
        .merge(calendars::router())
        .merge(events::router())
        .merge(range::router())
        .split_for_parts();
    router
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
        .merge(openapi::routes(api))
        .fallback(not_found)
        .layer(middleware::from_fn(common::logging::request_layer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_secret_is_refused_off_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg =
            Config::for_tests(dir.path().to_path_buf(), "http://i", EXAMPLE_SERVICE_SECRET);
        cfg.listen = "0.0.0.0:8083".parse().unwrap();
        assert!(build_state(cfg.clone()).is_err());
        cfg.listen = "127.0.0.1:8083".parse().unwrap();
        cfg.service_secret = "a".repeat(15);
        assert!(build_state(cfg).is_err());
    }
}
