use axum::{extract::FromRequestParts, http::request::Parts};
use common::{ApiError, AuthUser};
use uuid::Uuid;

use crate::AppState;

/// The user a request acts for: a trusted backend names one with `X-Service-Secret` + `X-User-Id`,
/// anyone else presents the user's own bearer access token.
pub struct Caller(pub Uuid);

fn unauthorized(message: &'static str) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        "unauthorized",
        message,
    )
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        // A secret header decides alone: a wrong secret is never rescued by a valid token.
        if parts.headers.contains_key("x-service-secret") {
            if !common::secret::secret_ok(&parts.headers, &state.cfg.service_secret) {
                return Err(unauthorized("missing or wrong X-Service-Secret"));
            }
            return parts
                .headers
                .get("x-user-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .map(Caller)
                .ok_or_else(|| unauthorized("X-User-Id must be a user id (UUID)"));
        }
        AuthUser::from_request_parts(parts, state)
            .await
            .map(|AuthUser(c)| Caller(c.sub))
    }
}
