use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::{ApiError, ApiResult};
use rusqlite::{Row, params};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{Db, crypto};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub email_verified: bool,
    pub display_name: String,
    pub is_admin: bool,
    pub must_change_password: bool,
    pub has_password: bool,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

pub struct NewUser {
    pub username: String,
    pub email: String,
    pub email_verified: bool,
    pub is_admin: bool,
    pub must_change_password: bool,
    pub password_hash: Option<String>,
}

const COLS: &str = "id, username, email, email_verified, display_name, is_admin, must_change_password, \
                    password_hash IS NOT NULL, disabled, created_at, password_hash";

fn row(r: &Row) -> rusqlite::Result<(User, Option<String>)> {
    let id: String = r.get(0)?;
    let created: i64 = r.get(9)?;
    let user = User {
        id: Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        username: r.get(1)?,
        email: r.get(2)?,
        email_verified: r.get(3)?,
        display_name: r.get(4)?,
        is_admin: r.get(5)?,
        must_change_password: r.get(6)?,
        has_password: r.get(7)?,
        disabled: r.get(8)?,
        created_at: DateTime::from_timestamp(created, 0).unwrap_or_default(),
    };
    Ok((user, r.get(10)?))
}

pub fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Looks up by username or email (normalized). Returns the password hash alongside.
pub fn find_by_login(db: &Db, login: &str) -> ApiResult<Option<(User, Option<String>)>> {
    let login = normalize(login);
    db.with(|c| {
        let sql = format!("SELECT {COLS} FROM users WHERE username = ?1 OR email = ?1");
        match c.query_row(&sql, [&login], row) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    })
}

pub fn get(db: &Db, id: Uuid) -> ApiResult<User> {
    db.with(|c| {
        c.query_row(
            &format!("SELECT {COLS} FROM users WHERE id = ?1"),
            [id.to_string()],
            row,
        )
        .map(|(u, _)| u)
    })
}

pub fn password_hash(db: &Db, id: Uuid) -> ApiResult<Option<String>> {
    db.with(|c| {
        c.query_row(
            "SELECT password_hash FROM users WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
    })
}

pub fn create(db: &Db, new: NewUser) -> ApiResult<User> {
    let id = Uuid::new_v4();
    let (username, email) = (normalize(&new.username), normalize(&new.email));
    // One connection behind a mutex, so check-then-insert is atomic.
    let inserted = db.with(|c| {
        let taken: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE username IN (?1, ?2) OR email IN (?1, ?2))",
            params![username, email],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(false);
        }
        c.execute(
            "INSERT INTO users (id, username, email, email_verified, password_hash, is_admin, must_change_password, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id.to_string(), username, email, new.email_verified, new.password_hash, new.is_admin, new.must_change_password, Utc::now().timestamp()],
        )?;
        Ok(true)
    })?;
    if !inserted {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "username or email already in use",
        ));
    }
    get(db, id)
}

pub fn set_password(db: &Db, id: Uuid, pw: &str, must_change: bool) -> ApiResult<()> {
    let hash = crypto::hash_password(pw);
    db.with(|c| {
        c.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = ?2 WHERE id = ?3",
            params![hash, must_change, id.to_string()],
        )
    })?;
    Ok(())
}

/// Hash computed by the caller (off the async thread). Reset proves control of the email, so it also verifies it.
pub fn complete_reset(db: &Db, id: Uuid, hash: &str) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 0, email_verified = 1 WHERE id = ?2",
            params![hash, id.to_string()],
        )
    })?;
    Ok(())
}

pub fn mark_verified(db: &Db, id: Uuid) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "UPDATE users SET email_verified = 1 WHERE id = ?1",
            [id.to_string()],
        )
    })?;
    Ok(())
}

pub fn set_display_name(db: &Db, id: Uuid, name: &str) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "UPDATE users SET display_name = ?1 WHERE id = ?2",
            params![name, id.to_string()],
        )
    })?;
    Ok(())
}

/// Creates the admin with a random one-time password when the table is empty; returns that password.
pub fn seed(db: &Db, username: &str, email: &str) -> anyhow::Result<Option<String>> {
    let count = db.with(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0)));
    if count.map_err(|e| anyhow::anyhow!("counting users: {}", e.message))? > 0 {
        return Ok(None);
    }
    let password = crypto::random_token();
    create(
        db,
        NewUser {
            username: username.into(),
            email: email.into(),
            email_verified: true,
            is_admin: true,
            must_change_password: true,
            password_hash: Some(crypto::hash_password(&password)),
        },
    )
    .map_err(|e| anyhow::anyhow!("seeding admin: {}", e.message))?;
    Ok(Some(password))
}

/// Every user, ordered by username.
// ponytail: no pagination, add limit/offset when user count makes this slow
pub fn list_all(db: &Db) -> ApiResult<Vec<User>> {
    db.with(|c| {
        let mut st = c.prepare(&format!("SELECT {COLS} FROM users ORDER BY username"))?;
        st.query_map([], |r| row(r).map(|(u, _)| u))?.collect()
    })
}

pub enum FlagsOutcome {
    Updated,
    NotFound,
    LastAdmin,
}

/// Applies `disabled` / `is_admin`. Refuses to leave zero enabled admins; the check, the update and the
/// session revocation on disable share one transaction under the single connection lock, so concurrent
/// requests cannot both pass the check.
pub fn update_flags(
    db: &Db,
    id: Uuid,
    disabled: Option<bool>,
    is_admin: Option<bool>,
) -> ApiResult<FlagsOutcome> {
    let id = id.to_string();
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let cur = match tx.query_row(
            "SELECT is_admin, disabled FROM users WHERE id = ?1",
            [&id],
            |r| Ok((r.get::<_, bool>(0)?, r.get::<_, bool>(1)?)),
        ) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(FlagsOutcome::NotFound),
            Err(e) => return Err(e),
        };
        let (admin, dis) = (is_admin.unwrap_or(cur.0), disabled.unwrap_or(cur.1));
        if cur.0 && !cur.1 && (!admin || dis) {
            let others: i64 = tx.query_row(
                "SELECT count(*) FROM users WHERE is_admin = 1 AND disabled = 0 AND id != ?1",
                [&id],
                |r| r.get(0),
            )?;
            if others == 0 {
                return Ok(FlagsOutcome::LastAdmin);
            }
        }
        tx.execute(
            "UPDATE users SET is_admin = ?1, disabled = ?2 WHERE id = ?3",
            params![admin, dis, id],
        )?;
        if dis {
            tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
            tx.execute("DELETE FROM refresh_tokens WHERE user_id = ?1", [&id])?;
        }
        tx.commit()?;
        Ok(FlagsOutcome::Updated)
    })
}

/// Admin reset: sets the (already hashed) temporary password, forces a change, and revokes the user's
/// sessions, refresh tokens and outstanding reset tokens. False when the user does not exist.
pub fn admin_reset(db: &Db, id: Uuid, hash: &str) -> ApiResult<bool> {
    let id = id.to_string();
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 1 WHERE id = ?2",
            params![hash, id],
        )?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
        tx.execute("DELETE FROM refresh_tokens WHERE user_id = ?1", [&id])?;
        tx.execute(
            "DELETE FROM email_tokens WHERE user_id = ?1 AND purpose = 'reset'",
            [&id],
        )?;
        tx.commit()?;
        Ok(true)
    })
}
