pub mod app_password_store;
pub mod config;
pub mod crypto;
pub mod db;
pub mod email_tokens;
pub mod mail;
pub mod oauth_store;
pub mod openapi;
pub mod providers;
pub mod ratelimit;
pub mod routes;
pub mod sessions;
pub mod social_store;
pub mod tokens;
pub mod users;

use std::sync::Arc;

use axum::{Json, Router, http::StatusCode, middleware};
use common::ApiError;
use serde::Serialize;
use utoipa::{OpenApi as _, ToSchema};

use utoipa_axum::{router::OpenApiRouter, routes};

pub use common::EXAMPLE_SERVICE_SECRET;
pub use config::Config;
pub use db::Db;

// ponytail: 4 concurrent hashes; tune to cores/memory if sign-in latency under load matters
const HASH_PERMITS: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub limiter: Arc<ratelimit::RateLimiter>,
    /// Bounds concurrent Argon2 work (CPU and ~19 MiB each).
    pub hashing: Arc<tokio::sync::Semaphore>,
    pub mail: mail::Mailer,
    pub signer: Arc<tokens::Signer>,
    /// Outbound calls to identity providers.
    pub http: reqwest::Client,
}

/// Opens the database under `cfg.data_dir`. Returns the one-time seed password when a user was seeded.
pub fn build_state(cfg: Config) -> anyhow::Result<(AppState, Option<String>)> {
    if common::secret::check(&cfg.service_secret, cfg.listen, "ME_AUTH__SERVICE_SECRET")? {
        tracing::warn!(
            "service_secret is the example service_secret from config.example.toml; change it before anything but local development"
        );
    }
    std::fs::create_dir_all(&cfg.data_dir)?;
    let db = db::open(&cfg.data_dir.join("auth.db"))?;
    let email = cfg
        .seed_email
        .clone()
        .unwrap_or_else(|| format!("{}@localhost", cfg.seed_username));
    let seed_password = users::seed(&db, &cfg.seed_username, &email)?;
    let mail = mail::Mailer::from_config(cfg.smtp.as_ref())?;
    let signer = Arc::new(tokens::Signer::load_or_create(
        &cfg.data_dir.join("signing.key"),
    )?);
    Ok((
        AppState {
            cfg: Arc::new(cfg),
            db,
            limiter: Default::default(),
            hashing: Arc::new(tokio::sync::Semaphore::new(HASH_PERMITS)),
            mail,
            signer,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        },
        seed_password,
    ))
}

/// The `reset-password` command: the way back in for an admin who is locked out. Gives `username` a new
/// one-time password that must be changed at the next sign-in, through the same code path as the admin
/// reset (so every session, token and app password of that user is revoked), and returns it. Works on
/// the database file directly, whether or not the server is running (SQLite WAL; one short transaction).
pub fn reset_password(cfg: &Config, username: &str) -> anyhow::Result<String> {
    let path = cfg.data_dir.join("auth.db");
    anyhow::ensure!(path.exists(), "no database at {}", path.display());
    let db = db::open(&path)?;
    let failed = |e: ApiError| anyhow::anyhow!("database error: {}", e.message);
    let name = users::normalize(username);
    let user = users::find_by_login(&db, &name)
        .map_err(failed)?
        .map(|(u, _)| u)
        .filter(|u| u.username == name)
        .ok_or_else(|| anyhow::anyhow!("no user named {name:?}"))?;
    let password = crypto::temporary_password();
    let hash = crypto::hash_password(&password);
    anyhow::ensure!(
        users::admin_reset(&db, user.id, &hash).map_err(failed)?,
        "no user named {name:?}"
    );
    Ok(password)
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: &'static str,
}

/// Liveness probe
///
/// Public: no service secret.
#[utoipa::path(get, path = "/health", security(()), responses((status = 200, description = "the service is up", body = Health)))]
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// Only `/api/*` answers with the JSON error shape; the RFC endpoints keep the framework default.
async fn method_not_allowed(uri: axum::http::Uri) -> axum::response::Response {
    use axum::response::IntoResponse;
    if uri.path().starts_with("/api/") {
        ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed on this route",
        )
        .into_response()
    } else {
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    }
}

pub fn app(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .routes(routes!(health))
        .merge(routes::router())
        .split_for_parts();
    router
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state.clone())
        .merge(openapi::routes(api))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state,
            sessions::require_service_secret,
        ))
        .layer(middleware::from_fn(common::logging::request_layer))
}
