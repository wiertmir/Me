use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::{ApiError, ApiResult};
use rusqlite::params;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::Db;

pub const MAX_PER_USER: i64 = 25;
/// `last_used` is written at most this often per password.
const LAST_USED_GRANULARITY_SECS: i64 = 60;

#[derive(Serialize, ToSchema)]
pub struct AppPasswordInfo {
    pub id: Uuid,
    pub label: String,
    pub created_at: DateTime<Utc>,
    /// Updated at most once a minute; null if never used.
    pub last_used: Option<DateTime<Utc>>,
}

fn ts(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// Stores a new password hash. `Ok(false)` when the user already has `MAX_PER_USER`.
/// Count and insert share the single connection lock, so concurrent requests cannot exceed the limit.
pub fn create(db: &Db, user: Uuid, id: Uuid, label: &str, hash: &str) -> ApiResult<bool> {
    db.with(|c| {
        let n: i64 = c.query_row(
            "SELECT count(*) FROM app_passwords WHERE user_id = ?1",
            [user.to_string()],
            |r| r.get(0),
        )?;
        if n >= MAX_PER_USER {
            return Ok(false);
        }
        c.execute(
            "INSERT INTO app_passwords (id, user_id, label, hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.to_string(), user.to_string(), label, hash, Utc::now().timestamp()],
        )?;
        Ok(true)
    })
}

pub fn list(db: &Db, user: Uuid) -> ApiResult<Vec<AppPasswordInfo>> {
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT id, label, created_at, last_used FROM app_passwords WHERE user_id = ?1
             ORDER BY created_at, rowid",
        )?;
        st.query_map([user.to_string()], |r| {
            let id: String = r.get(0)?;
            Ok(AppPasswordInfo {
                id: Uuid::parse_str(&id).unwrap_or_default(),
                label: r.get(1)?,
                created_at: ts(r.get(2)?),
                last_used: r.get::<_, Option<i64>>(3)?.map(ts),
            })
        })?
        .collect()
    })
}

/// Deletes the user's own password; false when it does not exist or belongs to someone else.
pub fn delete(db: &Db, user: Uuid, id: Uuid) -> ApiResult<bool> {
    db.with(|c| {
        c.execute(
            "DELETE FROM app_passwords WHERE id = ?1 AND user_id = ?2",
            [id.to_string(), user.to_string()],
        )
    })
    .map(|n| n > 0)
}

/// `(id, sha256 hex)` of every app password of the user.
pub fn hashes(db: &Db, user: Uuid) -> ApiResult<Vec<(Uuid, String)>> {
    db.with(|c| {
        let mut st = c.prepare("SELECT id, hash FROM app_passwords WHERE user_id = ?1")?;
        st.query_map([user.to_string()], |r| {
            let id: String = r.get(0)?;
            Ok((Uuid::parse_str(&id).unwrap_or_default(), r.get(1)?))
        })?
        .collect()
    })
}

/// A CalDAV client verifies on every request, so skip the write when the stored value is recent.
pub fn touch(db: &Db, id: Uuid) -> ApiResult<()> {
    let now = Utc::now().timestamp();
    db.with(|c| {
        c.execute(
            "UPDATE app_passwords SET last_used = ?1 WHERE id = ?2 AND (last_used IS NULL OR last_used <= ?1 - ?3)",
            params![now, id.to_string(), LAST_USED_GRANULARITY_SECS],
        )
    })?;
    Ok(())
}

pub fn too_many() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "conflict",
        "at most 25 app passwords per user; delete one first",
    )
}
