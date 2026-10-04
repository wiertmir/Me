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
    /// Lower-case, 3-32 characters of `a-z 0-9 . _ -`.
    pub username: String,
    /// Lower-case.
    pub email: String,
    pub email_verified: bool,
    /// Empty when never set.
    pub display_name: String,
    pub is_admin: bool,
    /// True after an admin set a temporary password; only sign-out and password change work until then.
    pub must_change_password: bool,
    /// False for accounts created through social sign-in that never set a password.
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

/// What `revoke_sign_in_state` leaves in place. The default keeps nothing.
#[derive(Clone, Copy, Default)]
pub struct Keep<'a> {
    /// Id of the one session that survives (the one making the request).
    pub session: Option<&'a str>,
    pub app_passwords: bool,
}

/// Every table that holds something a user can sign in or stay signed in with, and the condition that
/// selects one user's rows (`?1` is the user id). `revoke_sign_in_state` deletes exactly these; a test
/// fails when the schema has a table that is neither here nor on its allow-list.
/// EVERY new credential table must be added here.
pub const SIGN_IN_STATE: &[(&str, &str)] = &[
    ("sessions", "user_id = ?1"),
    ("refresh_tokens", "user_id = ?1"),
    ("auth_codes", "user_id = ?1"),
    ("social_states", "link_user_id = ?1"),
    ("social_tickets", "user_id = ?1"),
    ("social_link_tickets", "user_id = ?1"),
    ("social_link_intents", "user_id = ?1"),
    ("app_passwords", "user_id = ?1"),
    ("email_tokens", "user_id = ?1 AND purpose = 'reset'"),
];

/// Revokes everything that lets someone get into the account without the password (see
/// `SIGN_IN_STATE`), except what `keep` names. The only place that does so: password reset, admin reset,
/// password change, unlink and disable all call it inside the transaction that makes their change.
pub(crate) fn revoke_sign_in_state(
    tx: &rusqlite::Connection,
    id: &str,
    keep: Keep,
) -> rusqlite::Result<()> {
    for (table, cond) in SIGN_IN_STATE {
        let sql = format!("DELETE FROM {table} WHERE {cond}");
        match (*table, keep) {
            (
                "app_passwords",
                Keep {
                    app_passwords: true,
                    ..
                },
            ) => {}
            (
                "sessions",
                Keep {
                    session: Some(session),
                    ..
                },
            ) => {
                tx.execute(&format!("{sql} AND id != ?2"), [id, session])?;
            }
            _ => {
                tx.execute(&sql, [id])?;
            }
        }
    }
    Ok(())
}

/// Voluntary change: stores the new hash (computed by the caller, off the async thread), clears the
/// forced-change flag and, in the same transaction, revokes all sign-in state except `session`.
/// `expected` is the hash the caller checked the current password against (None: the account had no
/// password); false when it is no longer the stored one, in which case nothing changes.
pub fn change_password(
    db: &Db,
    id: Uuid,
    expected: Option<&str>,
    hash: &str,
    session: Uuid,
) -> ApiResult<bool> {
    let (id, session) = (id.to_string(), session.to_string());
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 0 WHERE id = ?2 AND password_hash IS ?3",
            params![hash, id, expected],
        )?;
        if n == 0 {
            return Ok(false);
        }
        let keep = Keep {
            session: Some(&session),
            app_passwords: false,
        };
        revoke_sign_in_state(&tx, &id, keep)?;
        tx.commit()?;
        Ok(true)
    })
}

/// Whoever proves an address that was never proven (by a reset or by the verification link) reclaims the
/// account, so identities attached while it was unproven (e.g. a pre-hijacking sign-up with the victim's
/// address) are removed. Call before the update; returns how many were removed.
fn purge_identities_if_unverified(tx: &rusqlite::Connection, id: &str) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM identities WHERE user_id = ?1
         AND EXISTS (SELECT 1 FROM users WHERE id = ?1 AND email_verified = 0)",
        [id],
    )
}

fn log_purged(id: &str, count: usize) {
    if count > 0 {
        tracing::warn!(event = "identities_purged_on_reset", user_id = %id, count);
    }
}

/// Hash computed by the caller (off the async thread). Reset proves control of the email, so it also
/// verifies it, and revokes all sign-in state in the same transaction.
pub fn complete_reset(db: &Db, id: Uuid, hash: &str) -> ApiResult<()> {
    let id = id.to_string();
    let purged = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let purged = purge_identities_if_unverified(&tx, &id)?;
        tx.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 0, email_verified = 1 WHERE id = ?2",
            params![hash, id],
        )?;
        revoke_sign_in_state(&tx, &id, Keep::default())?;
        tx.commit()?;
        Ok(purged)
    })?;
    log_purged(&id, purged);
    Ok(())
}

/// The mailed link was followed: the address is now proven. Whatever was attached to the account or
/// signed in to it before that moment may belong to someone who registered another person's address, so
/// the step from unverified to verified removes every linked identity and revokes all sign-in state, in
/// the same transaction as the update. An already verified account is left untouched.
pub fn mark_verified(db: &Db, id: Uuid) -> ApiResult<()> {
    let id = id.to_string();
    let purged = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        // Reads the flag, so it runs before the update.
        let purged = purge_identities_if_unverified(&tx, &id)?;
        let newly = tx.execute(
            "UPDATE users SET email_verified = 1 WHERE id = ?1 AND email_verified = 0",
            [&id],
        )?;
        if newly > 0 {
            revoke_sign_in_state(&tx, &id, Keep::default())?;
        }
        tx.commit()?;
        Ok(purged)
    })?;
    if purged > 0 {
        tracing::warn!(event = "identities_purged_on_verify", user_id = %id, count = purged);
    }
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
/// revocation of all sign-in state on disable share one transaction under the single connection lock, so concurrent
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
            revoke_sign_in_state(&tx, &id, Keep::default())?;
        }
        tx.commit()?;
        Ok(FlagsOutcome::Updated)
    })
}

/// Admin reset: sets the (already hashed) temporary password, forces a change, and clears
/// everything `revoke_sign_in_state` clears. False when the user does not exist.
pub fn admin_reset(db: &Db, id: Uuid, hash: &str) -> ApiResult<bool> {
    let id = id.to_string();
    let purged = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        let purged = purge_identities_if_unverified(&tx, &id)?;
        let n = tx.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 1 WHERE id = ?2",
            params![hash, id],
        )?;
        if n == 0 {
            return Ok(None);
        }
        revoke_sign_in_state(&tx, &id, Keep::default())?;
        tx.commit()?;
        Ok(Some(purged))
    })?;
    log_purged(&id, purged.unwrap_or(0));
    Ok(purged.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tables `revoke_sign_in_state` deliberately leaves alone.
    const NOT_SIGN_IN_STATE: &[&str] = &[
        // The account itself.
        "users",
        // A pending OAuth authorization request belongs to a browser, not to a user: it has no user
        // column, and it only becomes a grant (an `auth_codes` row) when a live session accepts it.
        "auth_requests",
        // Linked providers are sign-in methods, like the password. Resets remove them only when the
        // email was never proven (`purge_identities_if_unverified`); unlink removes the one named.
        "identities",
    ];

    /// Adding a table without deciding whether a reset must clear it fails here.
    #[test]
    fn every_table_is_revoked_or_deliberately_kept() {
        let db = crate::db::open_in_memory().unwrap();
        let tables: Vec<String> = db
            .with(|c| {
                c.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .unwrap();
        assert!(!tables.is_empty());
        for t in &tables {
            let revoked = SIGN_IN_STATE.iter().any(|(name, _)| name == t);
            let kept = NOT_SIGN_IN_STATE.contains(&t.as_str());
            assert!(
                revoked != kept,
                "table `{t}`: add it to users::SIGN_IN_STATE, or to NOT_SIGN_IN_STATE with the reason"
            );
        }
        for (name, _) in SIGN_IN_STATE {
            assert!(tables.contains(&name.to_string()), "no table `{name}`");
        }
    }

    /// The list is not only complete, it works: one row per table for a user, then keep-nothing.
    #[test]
    fn keep_nothing_empties_every_listed_table_and_keep_spares_what_it_names() {
        let db = crate::db::open_in_memory().unwrap();
        let user = |name: &str| {
            let new = NewUser {
                username: name.into(),
                email: format!("{name}@example.com"),
                email_verified: true,
                is_admin: false,
                must_change_password: false,
                password_hash: None,
            };
            create(&db, new).unwrap().id.to_string()
        };
        let fill = |id: &str| {
            db.with(|c| {
                c.execute_batch(&format!(
                    "INSERT INTO sessions VALUES ('s1-{id}', 'h1-{id}', '{id}', 0, 0, 9, '', '');
                     INSERT INTO sessions VALUES ('s2-{id}', 'h2-{id}', '{id}', 0, 0, 9, '', '');
                     INSERT INTO refresh_tokens VALUES ('r-{id}', 'f', '{id}', 'c', '', 9, 0);
                     INSERT INTO auth_codes VALUES ('c-{id}', '{id}', 'c', 'u', '', 'x', NULL, 'f', 9, 0);
                     INSERT INTO social_states VALUES ('st-{id}', 'google', 'v', NULL, '{id}', 9);
                     INSERT INTO social_tickets VALUES ('t-{id}', '{id}', NULL, 9);
                     INSERT INTO social_link_tickets VALUES ('lt-{id}', '{id}', 'google', 'sub', NULL, 9);
                     INSERT INTO social_link_intents VALUES ('li-{id}', '{id}', 'google', 9);
                     INSERT INTO app_passwords VALUES ('a-{id}', '{id}', 'l', 'h', 0, NULL);
                     INSERT INTO email_tokens VALUES ('e-{id}', '{id}', 'reset', 9);"
                ))
            })
            .unwrap();
        };
        let rows = |id: &str| -> Vec<(String, i64)> {
            SIGN_IN_STATE
                .iter()
                .map(|(table, cond)| {
                    let sql = format!("SELECT count(*) FROM {table} WHERE {cond}");
                    let n = db.with(|c| c.query_row(&sql, [id], |r| r.get(0))).unwrap();
                    (table.to_string(), n)
                })
                .collect()
        };
        let (a, b, bystander) = (user("a"), user("b"), user("c"));
        for id in [&a, &b, &bystander] {
            fill(id);
            assert!(rows(id).iter().all(|(_, n)| *n > 0), "{:?}", rows(id));
        }

        db.with(|c| revoke_sign_in_state(c, &a, Keep::default()))
            .unwrap();
        assert!(rows(&a).iter().all(|(_, n)| *n == 0), "{:?}", rows(&a));

        let session = format!("s1-{b}");
        let keep = Keep {
            session: Some(&session),
            app_passwords: true,
        };
        db.with(|c| revoke_sign_in_state(c, &b, keep)).unwrap();
        for (table, n) in rows(&b) {
            let expected = i64::from(table == "sessions" || table == "app_passwords");
            assert_eq!(n, expected, "{table}");
        }
        let left: String = db
            .with(|c| {
                c.query_row("SELECT id FROM sessions WHERE user_id = ?1", [&b], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(left, session);

        // Another user's rows are never touched.
        assert!(rows(&bystander).iter().all(|(_, n)| *n > 0));
    }
}
