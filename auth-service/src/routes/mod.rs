pub mod account;
pub mod auth;

use axum::http::HeaderMap;
use utoipa_axum::router::OpenApiRouter;

use crate::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().merge(auth::router()).merge(account::router())
}

/// Client address and user agent, as forwarded by auth-web.
pub(crate) fn client_info(h: &HeaderMap) -> (String, String) {
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_string();
    let ip = get("x-forwarded-for").split(',').next().unwrap_or_default().trim().to_string();
    (if ip.is_empty() { "unknown".into() } else { ip }, get("x-client-user-agent"))
}
