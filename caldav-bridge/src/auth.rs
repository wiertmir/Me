use axum::{
    extract::FromRequestParts,
    http::{HeaderValue, StatusCode, header, request::Parts},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{AppState, DavError};

/// The user a `/dav` request is signed in as: HTTP Basic credentials checked with auth-service on every
/// request, nothing cached.
pub struct Signed {
    pub user_id: Uuid,
    pub username: String,
}

#[derive(Deserialize)]
struct Verified {
    user_id: Uuid,
    username: String,
}

fn unauthorized() -> DavError {
    DavError::new(StatusCode::UNAUTHORIZED, "sign in").header(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Me\""),
    )
}

fn unavailable() -> DavError {
    DavError::new(StatusCode::SERVICE_UNAVAILABLE, "sign-in is unavailable")
}

/// `Authorization: Basic base64(username:password)`; the password may contain colons.
fn credentials(parts: &Parts) -> Option<(String, String)> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.into(), password.into()))
}

impl FromRequestParts<AppState> for Signed {
    type Rejection = DavError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, DavError> {
        let (username, password) = credentials(parts).ok_or_else(unauthorized)?;
        let mut req = state
            .http
            .post(format!("{}/api/app-passwords/verify", state.cfg.auth_url))
            .header("x-service-secret", &state.cfg.auth_secret)
            .json(&json!({"username": username, "password": password}));
        // The client's address, for auth-service's rate limit: the first one Caddy put there.
        if let Some(ip) = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
        {
            req = req.header("x-forwarded-for", ip.trim());
        }
        let resp = req.send().await.map_err(|e| {
            tracing::warn!(error = %e, "auth-service unreachable");
            unavailable()
        })?;
        match resp.status() {
            StatusCode::OK => {
                let v: Verified = resp.json().await.map_err(|_| unavailable())?;
                Ok(Signed {
                    user_id: v.user_id,
                    username: v.username,
                })
            }
            StatusCode::UNAUTHORIZED => Err(unauthorized()),
            StatusCode::TOO_MANY_REQUESTS => {
                let mut e = DavError::new(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
                if let Some(wait) = resp.headers().get(header::RETRY_AFTER) {
                    e = e.header(header::RETRY_AFTER, wait.clone());
                }
                Err(e)
            }
            s => {
                tracing::warn!(status = %s, "auth-service answered unexpectedly");
                Err(unavailable())
            }
        }
    }
}
