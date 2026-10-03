pub mod account;
pub mod auth;
pub mod signup;

use axum::http::{HeaderMap, StatusCode};
use common::ApiError;
use utoipa_axum::router::OpenApiRouter;

use crate::{AppState, crypto};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().merge(auth::router()).merge(signup::router()).merge(account::router())
}

/// Client address and user agent, as forwarded by auth-web.
pub(crate) fn client_info(h: &HeaderMap) -> (String, String) {
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_string();
    let ip = get("x-forwarded-for").split(',').next().unwrap_or_default().trim().to_string();
    (if ip.is_empty() { "unknown".into() } else { ip }, get("x-client-user-agent"))
}

/// Argon2 hashing on the blocking pool, under the same permit limit as verification.
pub(crate) async fn hash(s: &AppState, pw: String) -> Result<String, ApiError> {
    let _permit = s.hashing.acquire().await;
    tokio::task::spawn_blocking(move || crypto::hash_password(&pw)).await.map_err(|e| {
        tracing::error!(error = %e, "hashing task failed");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    })
}
