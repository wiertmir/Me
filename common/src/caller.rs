use std::sync::Arc;

use axum::{
    extract::{FromRef, FromRequestParts},
    http::request::Parts,
};
use uuid::Uuid;

use crate::{ApiError, AuthUser, TokenVerifier};

/// The shared secret trusted backends present in `X-Service-Secret`; a service gives its state
/// `FromRef<State> for ServiceSecret`.
#[derive(Clone)]
pub struct ServiceSecret(pub Arc<str>);

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

impl<S> FromRequestParts<S> for Caller
where
    Arc<TokenVerifier>: FromRef<S>,
    ServiceSecret: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        // A secret header decides alone: a wrong secret is never rescued by a valid token.
        if parts.headers.contains_key("x-service-secret") {
            let ServiceSecret(secret) = ServiceSecret::from_ref(state);
            if !crate::secret::secret_ok(&parts.headers, &secret) {
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
