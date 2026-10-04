use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{auth::SigninError, client_info};
use crate::{
    AppState,
    app_password_store::{self as store, AppPasswordInfo},
    crypto,
    extract::{ApiJson, PathId},
    sessions::SessionUser,
    users,
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_app_password, list_app_passwords))
        .routes(routes!(delete_app_password))
        .routes(routes!(verify))
}

#[derive(Deserialize, ToSchema)]
struct CreateRequest {
    /// 1 to 64 characters, e.g. the device name.
    label: String,
}

#[derive(Serialize, ToSchema)]
struct CreateResponse {
    id: Uuid,
    label: String,
    /// Shown once and never retrievable again.
    password: String,
    created_at: DateTime<Utc>,
}

/// Create an app password
///
/// The response is the only time the password is shown.
#[utoipa::path(
    post, path = "/api/me/app-passwords",
    tag = "app-passwords",
    request_body(content = CreateRequest, example = json!({"label": "Alice phone calendar"})),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 201, description = "created; `password` is shown once", body = CreateResponse),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 409, description = "`conflict`: 25 app passwords already exist", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, or label not 1 to 64 characters", body = ErrorBody),
)
)]
async fn create_app_password(
    State(s): State<AppState>,
    me: SessionUser,
    ApiJson(req): ApiJson<CreateRequest>,
) -> Result<(StatusCode, Json<CreateResponse>), ApiError> {
    let label = req.label.trim();
    if label.is_empty() || label.chars().count() > 64 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation",
            "label must be 1 to 64 characters",
        ));
    }
    let raw = crypto::app_password();
    let id = Uuid::new_v4();
    // SHA-256 instead of Argon2: the secret is ~75 bits of CSPRNG output, so there is nothing to
    // brute-force and a slow hash would only make every CalDAV request expensive.
    if !store::create(&s.db, me.user.id, id, label, &crypto::sha256_hex(&raw))? {
        return Err(store::too_many());
    }
    tracing::info!(event = "app_password_created", user_id = %me.user.id, app_password_id = %id);
    Ok((
        StatusCode::CREATED,
        Json(CreateResponse {
            id,
            label: label.to_string(),
            password: crypto::format_app_password(&raw),
            created_at: Utc::now(),
        }),
    ))
}

/// List app passwords
///
/// The caller's app passwords, never the secrets.
#[utoipa::path(
    get, path = "/api/me/app-passwords",
    tag = "app-passwords",
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 200, description = "the app passwords", body = Vec<AppPasswordInfo>),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
)
)]
async fn list_app_passwords(
    State(s): State<AppState>,
    me: SessionUser,
) -> Result<Json<Vec<AppPasswordInfo>>, ApiError> {
    Ok(Json(store::list(&s.db, me.user.id)?))
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such app password")
}

/// Delete an app password
///
/// Deletes one of the caller's own app passwords.
#[utoipa::path(
    delete, path = "/api/me/app-passwords/{id}",
    tag = "app-passwords",
    params(("id" = String, Path, description = "UUID")),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "deleted"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
)
)]
async fn delete_app_password(
    State(s): State<AppState>,
    me: SessionUser,
    PathId(id): PathId,
) -> Result<StatusCode, ApiError> {
    if !store::delete(&s.db, me.user.id, id)? {
        return Err(not_found());
    }
    tracing::info!(event = "app_password_deleted", user_id = %me.user.id, app_password_id = %id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct VerifyRequest {
    /// Username or email.
    username: String,
    password: String,
}

#[derive(Serialize, ToSchema)]
struct VerifyResponse {
    user_id: Uuid,
    username: String,
}

fn invalid_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "invalid credentials",
    )
}

/// Verify an app password
///
/// Checks a username and app password for the CalDAV bridge (service secret only). Every failure gives the same 401.
#[utoipa::path(
    post, path = "/api/app-passwords/verify",
    tag = "app-passwords",
    request_body(content = VerifyRequest, example = json!({"username": "alice", "password": "abcd-efgh-jklm-npqr"})),
    security(("service_secret" = [])),
    responses(
    (status = 200, description = "credentials are valid", body = VerifyResponse),
    (status = 401, description = "`invalid_credentials`: wrong username or password; `unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 422, description = "`validation`: the body is missing or malformed, or a field is invalid (see `message`)", body = ErrorBody),
    (status = 429, description = "`rate_limited`: too many failed attempts; see `Retry-After`", body = ErrorBody),
)
)]
async fn verify(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<VerifyRequest>,
) -> Result<Json<VerifyResponse>, SigninError> {
    let (ip, _) = client_info(&headers);
    let login = users::normalize(&req.username);
    // Bound the key so arbitrary input cannot grow the limiter map without limit.
    let user_key = format!("app:{}", login.chars().take(254).collect::<String>());
    let ip_key = format!("ip:{ip}");
    let limited = |wait| {
        tracing::warn!(event = "app_password_verify", ip = %ip, outcome = "rate_limited");
        SigninError::Limited(wait)
    };
    s.limiter.begin(&ip_key).map_err(limited)?;
    if let Err(wait) = s.limiter.begin(&user_key) {
        s.limiter.undo(&ip_key);
        return Err(limited(wait));
    }

    // Always hash and compare, also for unknown users, so timing does not reveal which exist.
    // Oversized input cannot match anything; hash only a fixed-size stand-in for it.
    let ok = req.password.len() <= 256;
    let presented = crypto::sha256_hex(&match ok {
        true => crypto::normalize_app_password(&req.password),
        false => String::new(),
    });
    let found = users::find_by_login(&s.db, &login)?.map(|(u, _)| u);
    let user = found
        .as_ref()
        .filter(|u| !u.disabled && !u.must_change_password);
    let stored = match user {
        Some(u) => store::hashes(&s.db, u.id)?,
        None => Vec::new(),
    };
    let mut matched = None;
    for (id, hash) in &stored {
        if bool::from(hash.as_bytes().ct_eq(presented.as_bytes())) {
            matched = Some(*id);
        }
    }
    if stored.is_empty() {
        let dummy = crypto::sha256_hex("dummy app password for timing");
        let _ = dummy.as_bytes().ct_eq(presented.as_bytes());
    }
    let (Some(user), Some(id), true) = (user, matched, ok) else {
        tracing::warn!(event = "app_password_verify", outcome = "failure",
            user_id = found.as_ref().map(|u| u.id.to_string()), ip = %ip);
        return Err(invalid_credentials().into());
    };
    s.limiter.clear(&user_key);
    s.limiter.undo(&ip_key);
    store::touch(&s.db, id)?;
    tracing::info!(event = "app_password_verify", outcome = "success", user_id = %user.id);
    Ok(Json(VerifyResponse {
        user_id: user.id,
        username: user.username.clone(),
    }))
}
