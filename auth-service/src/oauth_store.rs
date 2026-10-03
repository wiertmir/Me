//! Queries for authorization requests, codes and refresh tokens. Every secret is stored as its SHA-256 hex.
use chrono::Utc;
use common::ApiResult;
use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use crate::{
    Db,
    crypto::{random_token, sha256_hex},
};

const REQUEST_SECS: i64 = 10 * 60;
const CODE_SECS: i64 = 60;
const REFRESH_SECS: i64 = 30 * 24 * 3600;
/// Used codes are kept this long past expiry so a late replay still revokes its family.
const USED_CODE_GRACE_SECS: i64 = 24 * 3600;

pub struct AuthRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub nonce: Option<String>,
}

const REQ_COLS: &str = "client_id, redirect_uri, scope, state, code_challenge, nonce";

fn req_row(r: &Row) -> rusqlite::Result<AuthRequest> {
    Ok(AuthRequest {
        client_id: r.get(0)?,
        redirect_uri: r.get(1)?,
        scope: r.get(2)?,
        state: r.get(3)?,
        code_challenge: r.get(4)?,
        nonce: r.get(5)?,
    })
}

/// Returns the raw challenge id.
pub fn create_request(db: &Db, r: &AuthRequest) -> ApiResult<String> {
    let challenge = random_token();
    db.with(|c| {
        c.execute(
            "INSERT INTO auth_requests (challenge, client_id, redirect_uri, scope, state, code_challenge, nonce, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![sha256_hex(&challenge), r.client_id, r.redirect_uri, r.scope, r.state, r.code_challenge, r.nonce, Utc::now().timestamp() + REQUEST_SECS],
        )
    })?;
    Ok(challenge)
}

pub fn get_request(db: &Db, challenge: &str) -> ApiResult<Option<AuthRequest>> {
    db.with(|c| {
        c.query_row(
            &format!(
                "SELECT {REQ_COLS} FROM auth_requests WHERE challenge = ?1 AND expires_at > ?2"
            ),
            params![sha256_hex(challenge), Utc::now().timestamp()],
            req_row,
        )
        .optional()
    })
}

/// Consumes the challenge (one use) and mints a code for `user`. None when unknown, expired or used.
/// Also sweeps expired rows.
pub fn accept(db: &Db, challenge: &str, user: Uuid) -> ApiResult<Option<(String, AuthRequest)>> {
    let (code, now) = (random_token(), Utc::now().timestamp());
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        tx.execute("DELETE FROM auth_requests WHERE expires_at <= ?1", [now])?;
        tx.execute("DELETE FROM auth_codes WHERE expires_at <= ?1", [now - USED_CODE_GRACE_SECS])?;
        tx.execute("DELETE FROM refresh_tokens WHERE expires_at <= ?1", [now])?;
        let Some(req) = tx
            .query_row(&format!("DELETE FROM auth_requests WHERE challenge = ?1 RETURNING {REQ_COLS}"), [sha256_hex(challenge)], req_row)
            .optional()?
        else {
            return Ok(None);
        };
        tx.execute(
            "INSERT INTO auth_codes (code_hash, user_id, client_id, redirect_uri, scope, code_challenge, nonce, family_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![sha256_hex(&code), user.to_string(), req.client_id, req.redirect_uri, req.scope, req.code_challenge, req.nonce, Uuid::new_v4().to_string(), now + CODE_SECS],
        )?;
        tx.commit()?;
        Ok(Some((code, req)))
    })
}

pub struct Redeemed {
    pub user_id: Uuid,
    pub client_id: String,
    pub scope: String,
    pub nonce: Option<String>,
    pub refresh_token: String,
}

pub enum Redeem {
    Ok(Redeemed),
    Invalid,
    /// The code was already used; its refresh-token family has been revoked.
    Replay {
        user_id: Uuid,
        client_id: String,
    },
}

fn parse_uuid(s: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn insert_refresh(
    c: &rusqlite::Connection,
    token: &str,
    family: &str,
    user: &str,
    client: &str,
    scope: &str,
    expires: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO refresh_tokens (token_hash, family_id, user_id, client_id, scope, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![sha256_hex(token), family, user, client, scope, expires],
    )
    .map(|_| ())
}

/// Atomically marks the code used (it is spent even if `check` then fails), and when `check` accepts the
/// request and the user may sign in, issues the first refresh token of the code's family.
pub fn redeem_code(
    db: &Db,
    code: &str,
    check: impl FnOnce(&AuthRequest) -> bool,
) -> ApiResult<Redeem> {
    let (hash, now, refresh) = (sha256_hex(code), Utc::now().timestamp(), random_token());
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let row = tx
            .query_row(
                "SELECT user_id, family_id, used, expires_at, client_id, redirect_uri, scope, NULL, code_challenge, nonce,
                        (SELECT disabled = 0 AND must_change_password = 0 FROM users WHERE id = auth_codes.user_id)
                 FROM auth_codes WHERE code_hash = ?1",
                [&hash],
                |r| {
                    let req = AuthRequest {
                        client_id: r.get(4)?,
                        redirect_uri: r.get(5)?,
                        scope: r.get(6)?,
                        state: r.get(7)?,
                        code_challenge: r.get(8)?,
                        nonce: r.get(9)?,
                    };
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, i64>(3)?, req, r.get::<_, Option<bool>>(10)?))
                },
            )
            .optional()?;
        let Some((user, family, used, expires, req, user_ok)) = row else { return Ok(Redeem::Invalid) };
        if used {
            tx.execute("DELETE FROM refresh_tokens WHERE family_id = ?1", [&family])?;
            tx.commit()?;
            return Ok(Redeem::Replay { user_id: parse_uuid(user)?, client_id: req.client_id });
        }
        tx.execute("UPDATE auth_codes SET used = 1 WHERE code_hash = ?1", [&hash])?;
        if expires <= now || user_ok != Some(true) || !check(&req) {
            tx.commit()?;
            return Ok(Redeem::Invalid);
        }
        insert_refresh(&tx, &refresh, &family, &user, &req.client_id, &req.scope, now + REFRESH_SECS)?;
        tx.commit()?;
        Ok(Redeem::Ok(Redeemed { user_id: parse_uuid(user)?, client_id: req.client_id, scope: req.scope, nonce: req.nonce, refresh_token: refresh }))
    })
}

pub struct Rotated {
    pub user_id: Uuid,
    pub scope: String,
    pub refresh_token: String,
}

pub enum Rotation {
    Ok(Rotated),
    Invalid,
    /// An already-used token was presented; the whole family has been revoked.
    Reuse {
        user_id: Uuid,
    },
}

/// Rotation with reuse detection. Check, mark-used and insert share one transaction under the single
/// connection lock, so two parallel requests with the same token cannot both succeed.
pub fn rotate_refresh(db: &Db, token: &str, client_id: &str) -> ApiResult<Rotation> {
    let (hash, now, new) = (sha256_hex(token), Utc::now().timestamp(), random_token());
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let row = tx
            .query_row(
                "SELECT family_id, user_id, client_id, scope, expires_at, used,
                        (SELECT disabled = 0 AND must_change_password = 0 FROM users WHERE id = refresh_tokens.user_id)
                 FROM refresh_tokens WHERE token_hash = ?1",
                [&hash],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, bool>(5)?, r.get::<_, Option<bool>>(6)?)),
            )
            .optional()?;
        let Some((family, user, client, scope, expires, used, user_ok)) = row else { return Ok(Rotation::Invalid) };
        if client != client_id {
            return Ok(Rotation::Invalid);
        }
        if used {
            tx.execute("DELETE FROM refresh_tokens WHERE family_id = ?1", [&family])?;
            tx.commit()?;
            return Ok(Rotation::Reuse { user_id: parse_uuid(user)? });
        }
        if expires <= now || user_ok != Some(true) {
            tx.execute("DELETE FROM refresh_tokens WHERE family_id = ?1", [&family])?;
            tx.commit()?;
            return Ok(Rotation::Invalid);
        }
        tx.execute("UPDATE refresh_tokens SET used = 1 WHERE token_hash = ?1", [&hash])?;
        // The family keeps its original expiry: rotation never extends it.
        insert_refresh(&tx, &new, &family, &user, &client, &scope, expires)?;
        tx.commit()?;
        Ok(Rotation::Ok(Rotated { user_id: parse_uuid(user)?, scope, refresh_token: new }))
    })
}

/// Revokes the family of `token`; with `client_id` given, only if the token belongs to that client.
pub fn revoke_family(db: &Db, token: &str, client_id: Option<&str>) -> ApiResult<()> {
    db.with(|c| {
        c.execute(
            "DELETE FROM refresh_tokens WHERE family_id IN
               (SELECT family_id FROM refresh_tokens WHERE token_hash = ?1 AND (?2 IS NULL OR client_id = ?2))",
            params![sha256_hex(token), client_id],
        )
    })?;
    Ok(())
}
