//! Social sign-in persistence: OAuth state, link intents, tickets and linked identities.
//! Every secret is stored as its SHA-256 hex and consumed with a single `DELETE … RETURNING`.
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::{ApiError, ApiResult};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    Db,
    crypto::{random_token, sha256_hex},
    users,
};

const STATE_SECS: i64 = 600;
const TICKET_SECS: i64 = 60;

pub struct SocialState {
    pub provider: String,
    pub verifier: String,
    pub challenge: Option<String>,
    pub link_user: Option<Uuid>,
}

fn uuid_col(s: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Returns the raw state value (also the browser cookie value).
pub fn create_state(
    db: &Db,
    provider: &str,
    verifier: &str,
    challenge: Option<&str>,
    link_user: Option<Uuid>,
) -> ApiResult<String> {
    let state = random_token();
    let now = Utc::now().timestamp();
    db.with(|c| {
        c.execute("DELETE FROM social_states WHERE expires_at <= ?1", [now])?;
        c.execute(
            "INSERT INTO social_states (state_hash, provider, pkce_verifier, challenge, link_user_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sha256_hex(&state), provider, verifier, challenge, link_user.map(|u| u.to_string()), now + STATE_SECS],
        )
    })?;
    Ok(state)
}

/// Consumes the state (single use). None when unknown, expired or already used.
pub fn take_state(db: &Db, state: &str) -> ApiResult<Option<SocialState>> {
    db.with(|c| {
        c.query_row(
            "DELETE FROM social_states WHERE state_hash = ?1 AND expires_at > ?2
             RETURNING provider, pkce_verifier, challenge, link_user_id",
            params![sha256_hex(state), Utc::now().timestamp()],
            |r| {
                Ok(SocialState {
                    provider: r.get(0)?,
                    verifier: r.get(1)?,
                    challenge: r.get(2)?,
                    link_user: r.get::<_, Option<String>>(3)?.map(uuid_col).transpose()?,
                })
            },
        )
        .optional()
    })
}

pub fn create_link_intent(db: &Db, user: Uuid, provider: &str) -> ApiResult<String> {
    let token = random_token();
    db.with(|c| {
        c.execute(
            "INSERT INTO social_link_intents (token_hash, user_id, provider, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![sha256_hex(&token), user.to_string(), provider, Utc::now().timestamp() + STATE_SECS],
        )
    })?;
    Ok(token)
}

/// Consumes the intent; returns the user it was created for.
pub fn take_link_intent(db: &Db, token: &str, provider: &str) -> ApiResult<Option<Uuid>> {
    db.with(|c| {
        c.query_row(
            "DELETE FROM social_link_intents WHERE token_hash = ?1 AND provider = ?2 AND expires_at > ?3 RETURNING user_id",
            params![sha256_hex(token), provider, Utc::now().timestamp()],
            |r| uuid_col(r.get(0)?),
        )
        .optional()
    })
}

pub fn create_ticket(db: &Db, user: Uuid, challenge: Option<&str>) -> ApiResult<String> {
    let ticket = random_token();
    let now = Utc::now().timestamp();
    db.with(|c| {
        c.execute("DELETE FROM social_tickets WHERE expires_at <= ?1", [now])?;
        c.execute(
            "INSERT INTO social_tickets (ticket_hash, user_id, challenge, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![sha256_hex(&ticket), user.to_string(), challenge, now + TICKET_SECS],
        )
    })?;
    Ok(ticket)
}

/// Consumes the ticket; returns the user and the optional OAuth challenge.
pub fn take_ticket(db: &Db, ticket: &str) -> ApiResult<Option<(Uuid, Option<String>)>> {
    db.with(|c| {
        c.query_row(
            "DELETE FROM social_tickets WHERE ticket_hash = ?1 AND expires_at > ?2 RETURNING user_id, challenge",
            params![sha256_hex(ticket), Utc::now().timestamp()],
            |r| Ok((uuid_col(r.get(0)?)?, r.get(1)?)),
        )
        .optional()
    })
}

pub struct LinkTicket {
    pub user: Uuid,
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
}

/// One-time proof, from the link callback, that a provider identity may be attached to `user`.
pub fn create_link_ticket(
    db: &Db,
    user: Uuid,
    provider: &str,
    subject: &str,
    email: Option<&str>,
) -> ApiResult<String> {
    let ticket = random_token();
    let now = Utc::now().timestamp();
    db.with(|c| {
        c.execute("DELETE FROM social_link_tickets WHERE expires_at <= ?1", [now])?;
        c.execute(
            "INSERT INTO social_link_tickets (ticket_hash, user_id, provider, subject, email, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![sha256_hex(&ticket), user.to_string(), provider, subject, email, now + TICKET_SECS],
        )
    })?;
    Ok(ticket)
}

pub fn take_link_ticket(db: &Db, ticket: &str) -> ApiResult<Option<LinkTicket>> {
    db.with(|c| {
        c.query_row(
            "DELETE FROM social_link_tickets WHERE ticket_hash = ?1 AND expires_at > ?2
             RETURNING user_id, provider, subject, email",
            params![sha256_hex(ticket), Utc::now().timestamp()],
            |r| {
                Ok(LinkTicket {
                    user: uuid_col(r.get(0)?)?,
                    provider: r.get(1)?,
                    subject: r.get(2)?,
                    email: r.get(3)?,
                })
            },
        )
        .optional()
    })
}

#[derive(Serialize, ToSchema)]
pub struct Identity {
    pub provider: String,
    pub email: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub fn find_identity(db: &Db, provider: &str, subject: &str) -> ApiResult<Option<Uuid>> {
    db.with(|c| {
        c.query_row(
            "SELECT user_id FROM identities WHERE provider = ?1 AND provider_subject = ?2",
            [provider, subject],
            |r| uuid_col(r.get(0)?),
        )
        .optional()
    })
}

/// False when the identity or the user's identity for this provider already exists.
pub fn insert_identity(
    db: &Db,
    user: Uuid,
    provider: &str,
    subject: &str,
    email: Option<&str>,
) -> ApiResult<bool> {
    let n = db.with(|c| {
        c.execute(
            "INSERT OR IGNORE INTO identities (user_id, provider, provider_subject, email, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user.to_string(), provider, subject, email, Utc::now().timestamp()],
        )
    })?;
    Ok(n > 0)
}

pub fn list_identities(db: &Db, user: Uuid) -> ApiResult<Vec<Identity>> {
    db.with(|c| {
        let mut st = c.prepare(
            "SELECT provider, email, created_at FROM identities WHERE user_id = ?1 ORDER BY created_at, provider",
        )?;
        st.query_map([user.to_string()], |r| {
            Ok(Identity {
                provider: r.get(0)?,
                email: r.get(1)?,
                created_at: DateTime::from_timestamp(r.get(2)?, 0).unwrap_or_default(),
            })
        })?
        .collect()
    })
}

pub enum Unlink {
    Done,
    NotLinked,
    LastMethod,
}

/// Removes the identity unless it is the user's only way to sign in (no password, no other identity).
/// Whoever signed in through it may still hold sessions or refresh tokens, so the same transaction
/// revokes all sign-in state except `session` (the one asking) and the app passwords.
pub fn unlink(db: &Db, user: Uuid, provider: &str, session: Uuid) -> ApiResult<Unlink> {
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let u = user.to_string();
        let linked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM identities WHERE user_id = ?1 AND provider = ?2)",
            [&u, provider],
            |r| r.get(0),
        )?;
        if !linked {
            return Ok(Unlink::NotLinked);
        }
        let has_password: bool = tx.query_row(
            "SELECT password_hash IS NOT NULL FROM users WHERE id = ?1",
            [&u],
            |r| r.get(0),
        )?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM identities WHERE user_id = ?1",
            [&u],
            |r| r.get(0),
        )?;
        if !has_password && count <= 1 {
            return Ok(Unlink::LastMethod);
        }
        tx.execute(
            "DELETE FROM identities WHERE user_id = ?1 AND provider = ?2",
            [&u, provider],
        )?;
        let session = session.to_string();
        let keep = users::Keep {
            session: Some(&session),
            app_passwords: true,
        };
        users::revoke_sign_in_state(&tx, &u, keep)?;
        tx.commit()?;
        Ok(Unlink::Done)
    })
}

pub fn conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "last_sign_in_method",
        "this is your only way to sign in",
    )
}
