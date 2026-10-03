use chrono::Utc;
use common::ApiResult;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::{Db, crypto};

#[derive(Clone, Copy)]
pub enum Purpose {
    Verify,
    Reset,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Reset => "reset",
        }
    }

    fn ttl_secs(self) -> i64 {
        match self {
            Self::Verify => 24 * 3600,
            Self::Reset => 3600,
        }
    }

    pub fn valid_for(self) -> &'static str {
        match self {
            Self::Verify => "24 hours",
            Self::Reset => "1 hour",
        }
    }
}

/// Returns the raw token (only its hash is stored).
pub fn create(db: &Db, user: Uuid, purpose: Purpose) -> ApiResult<String> {
    create_expiring(
        db,
        user,
        purpose,
        Utc::now().timestamp() + purpose.ttl_secs(),
    )
}

pub fn create_expiring(
    db: &Db,
    user: Uuid,
    purpose: Purpose,
    expires_at: i64,
) -> ApiResult<String> {
    let token = crypto::random_token();
    db.with(|c| {
        c.execute(
            "INSERT INTO email_tokens (token_hash, user_id, purpose, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![crypto::sha256_hex(&token), user.to_string(), purpose.name(), expires_at],
        )
    })?;
    Ok(token)
}

/// Single use: deletes the token and returns its user if it matched this purpose and has not expired.
pub fn consume(db: &Db, token: &str, purpose: Purpose) -> ApiResult<Option<Uuid>> {
    let found: Option<(String, i64)> = db.with(|c| {
        c.query_row(
            "DELETE FROM email_tokens WHERE token_hash = ?1 AND purpose = ?2 RETURNING user_id, expires_at",
            params![crypto::sha256_hex(token), purpose.name()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })?;
    Ok(found
        .filter(|(_, exp)| *exp > Utc::now().timestamp())
        .and_then(|(u, _)| Uuid::parse_str(&u).ok()))
}

pub fn delete_for_user(db: &Db, user: Uuid, purpose: Purpose) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "DELETE FROM email_tokens WHERE user_id = ?1 AND purpose = ?2",
            params![user.to_string(), purpose.name()],
        )
    })?;
    Ok(())
}
