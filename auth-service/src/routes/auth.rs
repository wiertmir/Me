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
    extract::ApiJson,
    logging,
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
    /// Bearer token for user-scoped operations; valid 30 days.
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

/// Sign in with a password
///
/// Username or email plus password. Locked keys answer 429 without counting as failures; the `Retry-After` header gives the wait in seconds.
#[utoipa::path(
    post, path = "/api/signin",
    tag = "auth",
    request_body(content = SigninRequest, example = json!({"login": "alice", "password": "correct horse battery staple"})),
    security(("service_secret" = [])),
    responses(
    (status = 200, description = "signed in; `session_token` is the bearer token for user-scoped operations", body = SigninResponse),
    (status = 401, description = "`invalid_credentials`: unknown login or wrong password; `unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 403, description = "`email_not_verified`: the password is correct but the email address is not verified (only when mail is configured)", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, or password longer than 1024 bytes", body = ErrorBody),
    (status = 429, description = "`rate_limited`: too many failed attempts; see `Retry-After`", body = ErrorBody),
)
)]
async fn signin(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<SigninRequest>,
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
        tracing::warn!(event = "signin", login = %logging::for_log(&login), ip = %ip, outcome = "failure");
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

/// Sign out
///
/// Ends the current session. Allowed while a password change is pending.
#[utoipa::path(
    post, path = "/api/signout",
    tag = "auth",
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "session ended"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
)
)]
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
    /// Required when the account has a password. An account without one (created through social
    /// sign-in) omits it or sends an empty string, and so sets its first password; a value is ignored.
    #[serde(default)]
    current_password: Option<String>,
    new_password: String,
}

/// Change or set the password
///
/// Sets a new password and clears the forced-change flag. An account that has a password must give the
/// current one; an account without one (`has_password` false) sets its first password without it.
/// Signs the user out everywhere else: revokes their other sessions, all refresh tokens, authorization
/// codes, pending social tickets and app passwords; only the session making the request survives.
/// Allowed while a password change is pending.
#[utoipa::path(
    post, path = "/api/password/change",
    tag = "auth",
    request_body(content = ChangePasswordRequest, example = json!({"current_password": "correct horse battery staple", "new_password": "another long passphrase"})),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "password changed; everything but this session revoked"),
    (status = 401, description = "`invalid_credentials`: the current password is wrong, or missing although the account has a password; `unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, new password shorter than 8 characters, or a password longer than 1024 bytes", body = ErrorBody),
    (status = 429, description = "`rate_limited`: too many failed attempts; see `Retry-After`", body = ErrorBody),
)
)]
async fn change_password(
    State(s): State<AppState>,
    PendingUser(me): PendingUser,
    ApiJson(req): ApiJson<ChangePasswordRequest>,
) -> Result<StatusCode, SigninError> {
    let given = req.current_password.unwrap_or_default();
    crypto::check_password_size(&given)?;
    crypto::validate_password(&req.new_password)?;
    let current = users::password_hash(&s.db, me.user.id)?;
    // Without a stored hash there is nothing to verify (and nothing to guess, so nothing is counted):
    // the session is the proof. `change_password` below re-checks that the hash is still absent.
    if let Some(hash) = current.clone() {
        let key = format!("user:{}", me.user.username);
        s.limiter.begin(&key).map_err(SigninError::Limited)?;
        if given.is_empty() || !verify(&s, given, Some(hash)).await {
            return Err(invalid_credentials().into());
        }
        s.limiter.clear(&key);
    }
    let new_hash = super::hash(&s, req.new_password).await?;
    // False when the stored password changed while this request was being checked against the old state.
    if !users::change_password(
        &s.db,
        me.user.id,
        current.as_deref(),
        &new_hash,
        me.session_id,
    )? {
        return Err(invalid_credentials().into());
    }
    tracing::info!(event = "password_changed", user_id = %me.user.id, first_password = current.is_none());
    Ok(StatusCode::NO_CONTENT)
}
