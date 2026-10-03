use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::client_info;
use crate::{
    AppState, crypto,
    sessions::{self, PendingUser},
    users::{self, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(signin))
        .routes(routes!(signout))
        .routes(routes!(change_password))
}

#[derive(Deserialize, ToSchema)]
struct SigninRequest {
    /// Username or email.
    login: String,
    password: String,
}

#[derive(Serialize, ToSchema)]
struct SigninResponse {
    session_token: String,
    user: User,
}

pub(super) enum SigninError {
    Api(ApiError),
    Limited(Duration),
}

impl From<ApiError> for SigninError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl IntoResponse for SigninError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Limited(wait) => {
                let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
                let mut r = ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "too many failed attempts, try again later",
                )
                .into_response();
                r.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
                r
            }
        }
    }
}

fn invalid_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "invalid credentials",
    )
}

/// Argon2 on the blocking pool, with at most `HASH_PERMITS` verifications in flight.
/// `hash = None` verifies against a dummy hash (constant cost for unknown users).
async fn verify(s: &AppState, pw: String, hash: Option<String>) -> bool {
    let _permit = s.hashing.acquire().await;
    tokio::task::spawn_blocking(move || match hash {
        Some(h) => crypto::verify_password(&pw, &h),
        None => {
            crypto::verify_dummy(&pw);
            false
        }
    })
    .await
    .unwrap_or(false)
}

/// Password sign-in. Locked keys answer 429 without counting as failures.
#[utoipa::path(post, path = "/api/signin", request_body = SigninRequest, responses(
    (status = 200, body = SigninResponse),
    (status = 401, description = "invalid credentials", body = ErrorBody),
    (status = 403, description = "email_not_verified (only after a correct password)", body = ErrorBody),
    (status = 422, body = ErrorBody),
    (status = 429, description = "rate limited; see Retry-After", body = ErrorBody),
))]
async fn signin(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SigninRequest>,
) -> Result<Json<SigninResponse>, SigninError> {
    crypto::check_password_size(&req.password)?;
    let (ip, ua) = client_info(&headers);
    let login = users::normalize(&req.login);
    // Bound the key so arbitrary input cannot grow the limiter map without limit.
    let user_key = format!("user:{}", login.chars().take(254).collect::<String>());
    let ip_key = format!("ip:{ip}");
    // Attempts are counted before the slow verification so parallel guesses cannot slip past the lock.
    let limited = |wait| {
        tracing::warn!(event = "signin", ip = %ip, outcome = "rate_limited");
        SigninError::Limited(wait)
    };
    s.limiter.begin(&ip_key).map_err(limited)?;
    if let Err(wait) = s.limiter.begin(&user_key) {
        s.limiter.undo(&ip_key);
        return Err(limited(wait));
    }

    let found = users::find_by_login(&s.db, &login)?;
    // Unknown, disabled and passwordless accounts still pay for one hash verification.
    let hash = found
        .as_ref()
        .filter(|(u, _)| !u.disabled)
        .and_then(|(_, h)| h.clone());
    let verified = verify(&s, req.password, hash).await;
    let Some((user, _)) = found.filter(|_| verified) else {
        tracing::warn!(event = "signin", login = %login.chars().take(64).collect::<String>(), ip = %ip, outcome = "failure");
        return Err(invalid_credentials().into());
    };
    s.limiter.clear(&user_key);
    s.limiter.undo(&ip_key);
    if s.mail.enabled() && !user.email_verified {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "email_not_verified",
            "verify your email address first",
        )
        .into());
    }
    let session_token = sessions::create(&s.db, user.id, &ua, &ip)?;
    tracing::info!(event = "signin", user_id = %user.id, ip = %ip, outcome = "success");
    Ok(Json(SigninResponse {
        session_token,
        user,
    }))
}

/// Ends the current session. Allowed while a password change is pending.
#[utoipa::path(post, path = "/api/signout", responses((status = 204), (status = 401, body = ErrorBody)))]
async fn signout(
    State(s): State<AppState>,
    PendingUser(me): PendingUser,
) -> Result<StatusCode, ApiError> {
    sessions::delete(&s.db, me.session_id)?;
    tracing::info!(event = "signout", user_id = %me.user.id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// Changes the password, clears the forced-change flag and revokes the user's other sessions.
#[utoipa::path(post, path = "/api/password/change", request_body = ChangePasswordRequest, responses(
    (status = 204),
    (status = 401, body = ErrorBody),
    (status = 422, body = ErrorBody),
    (status = 429, body = ErrorBody),
))]
async fn change_password(
    State(s): State<AppState>,
    PendingUser(me): PendingUser,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<StatusCode, SigninError> {
    crypto::check_password_size(&req.current_password)?;
    crypto::validate_password(&req.new_password)?;
    let key = format!("user:{}", me.user.username);
    s.limiter.begin(&key).map_err(SigninError::Limited)?;
    let ok = match users::password_hash(&s.db, me.user.id)? {
        Some(hash) => verify(&s, req.current_password, Some(hash)).await,
        None => false,
    };
    if !ok {
        return Err(invalid_credentials().into());
    }
    s.limiter.clear(&key);
    users::set_password(&s.db, me.user.id, &req.new_password, false)?;
    sessions::delete_others(&s.db, me.user.id, me.session_id)?;
    tracing::info!(event = "password_changed", user_id = %me.user.id);
    Ok(StatusCode::NO_CONTENT)
}
