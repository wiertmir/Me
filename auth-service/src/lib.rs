pub mod config;
pub mod db;
pub mod logging;
pub mod openapi;

use std::sync::Arc;

use axum::{Json, Router, http::StatusCode, middleware};
use common::ApiError;
use serde::Serialize;
use utoipa::{OpenApi as _, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub use config::Config;
pub use db::Db;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
}

/// Opens the database under `cfg.data_dir`. Returns the one-time seed password when a user was seeded.
pub fn build_state(cfg: Config) -> anyhow::Result<(AppState, Option<String>)> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = Db::open(&cfg.data_dir.join("auth.db"))?;
    Ok((AppState { cfg: Arc::new(cfg), db }, None))
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: &'static str,
}

/// Liveness probe. Public: no service secret.
#[utoipa::path(get, path = "/health", responses((status = 200, body = Health)))]
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

#[derive(utoipa::OpenApi)]
#[openapi(info(title = "Me auth-service", version = "0.1.0"))]
struct ApiDoc;

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

pub fn app(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .split_for_parts();
    router
        .with_state(state)
        .merge(openapi::routes(api))
        .fallback(not_found)
        .layer(middleware::from_fn(logging::request_layer))
}
