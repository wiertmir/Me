use axum::{
    extract::{FromRequestParts, Request},
    http::{HeaderMap, StatusCode, request::Parts},
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use common::{ApiError, ApiResult};
use rusqlite::params;
use serde::Serialize;
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState, Db, crypto,
    users::{self, User},
};

const SESSION_SECS: i64 = 30 * 24 * 3600;

pub fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "authentication required",
    )
}

/// Returns the raw session token (only its hash is stored).
pub fn create(db: &Db, user: Uuid, ua: &str, ip: &str) -> ApiResult<String> {
    let token = crypto::random_token();
    let now = Utc::now().timestamp();
    db.with(|c| {
        c.execute(
            "INSERT INTO sessions (id, token_hash, user_id, created_at, last_seen, expires_at, user_agent, ip)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7)",
            params![Uuid::new_v4().to_string(), crypto::sha256_hex(&token), user.to_string(), now, now + SESSION_SECS, ua, ip],
        )
    })?;
    Ok(token)
}

pub fn delete(db: &Db, session_id: Uuid) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "DELETE FROM sessions WHERE id = ?1",
            [session_id.to_string()],
        )
    })?;
    Ok(())
}

pub fn delete_others(db: &Db, user: Uuid, keep: Uuid) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND id != ?2",
            [user.to_string(), keep.to_string()],
        )
    })?;
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub struct SessionInfo {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub user_agent: String,
    pub ip: String,
    pub current: bool,
}

/// The user's live sessions, newest first; `current` marks the one making the request.
pub fn list(db: &Db, user: Uuid, current: Uuid) -> ApiResult<Vec<SessionInfo>> {
    let ts = |t: i64| DateTime::from_timestamp(t, 0).unwrap_or_default();
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT id, created_at, last_seen, user_agent, ip FROM sessions
             WHERE user_id = ?1 AND expires_at > ?2 ORDER BY created_at DESC, rowid DESC",
        )?;
        st.query_map(params![user.to_string(), Utc::now().timestamp()], |r| {
            let id: String = r.get(0)?;
            let id = Uuid::parse_str(&id).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(SessionInfo {
                id,
                created_at: ts(r.get(1)?),
                last_seen: ts(r.get(2)?),
                user_agent: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ip: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                current: id == current,
            })
        })?
        .collect()
    })
}

/// Deletes the session only if it belongs to `user`; false when it does not exist or is someone else's.
pub fn delete_owned(db: &Db, user: Uuid, session_id: Uuid) -> ApiResult<bool> {
    let n = db.with(|c| {
        c.execute(
            "DELETE FROM sessions WHERE id = ?1 AND user_id = ?2",
            [session_id.to_string(), user.to_string()],
        )
    })?;
    Ok(n > 0)
}

/// Finds a live session, bumps `last_seen`, and loads its enabled user.
fn lookup(db: &Db, token: &str) -> ApiResult<Option<(Uuid, Uuid)>> {
    let now = Utc::now().timestamp();
    let found: Option<(String, String)> = db.with(|c| {
        let found = match c.query_row(
            "SELECT s.id, s.user_id FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND u.disabled = 0",
            params![crypto::sha256_hex(token), now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e),
        };
        if let Some((id, _)) = &found {
            c.execute(
                "UPDATE sessions SET last_seen = ?1 WHERE id = ?2",
                params![now, id],
            )?;
        }
        Ok(found)
    })?;
    Ok(found.and_then(|(s, u)| Some((Uuid::parse_str(&s).ok()?, Uuid::parse_str(&u).ok()?))))
}

fn secret_ok(headers: &HeaderMap, expected: &str) -> bool {
    // An absent header must never match, even against an (invalid) empty secret.
    headers
        .get("x-service-secret")
        .is_some_and(|v| v.as_bytes().ct_eq(expected.as_bytes()).into())
}

/// Rejects every `/api/*` request lacking the service secret, except the public API docs.
pub async fn require_service_secret(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = req.uri().path();
    let public = matches!(path, "/api/openapi.json" | "/api/docs");
    if path.starts_with("/api/") && !public && !secret_ok(req.headers(), &state.cfg.service_secret)
    {
        return Err(unauthorized());
    }
    Ok(next.run(req).await)
}

/// Extractor form of the same check (the middleware already covers all of `/api/*`).
pub struct ServiceAuth;

impl FromRequestParts<AppState> for ServiceAuth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        secret_ok(&parts.headers, &state.cfg.service_secret)
            .then_some(ServiceAuth)
            .ok_or_else(unauthorized)
    }
}

pub struct SessionUser {
    pub user: User,
    pub session_id: Uuid,
}

/// Like [`SessionUser`] but also accepts users who still must change their password.
pub struct PendingUser(pub SessionUser);

pub struct AdminUser(pub SessionUser);

async fn load(parts: &Parts, state: &AppState) -> ApiResult<SessionUser> {
    let token = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    let (session_id, user_id) = lookup(&state.db, token)?.ok_or_else(unauthorized)?;
    Ok(SessionUser {
        user: users::get(&state.db, user_id)?,
        session_id,
    })
}

impl FromRequestParts<AppState> for PendingUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        load(parts, state).await.map(PendingUser)
    }
}

impl FromRequestParts<AppState> for SessionUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let s = load(parts, state).await?;
        if s.user.must_change_password {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "password_change_required",
                "password change required",
            ));
        }
        Ok(s)
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let s = SessionUser::from_request_parts(parts, state).await?;
        if !s.user.is_admin {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "admin only",
            ));
        }
        Ok(AdminUser(s))
    }
}
