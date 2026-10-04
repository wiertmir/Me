use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use axum::http::StatusCode;
use rusqlite::Connection;

use crate::{ApiError, ApiResult};

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// Opens (creating it if needed) the SQLite file and applies `schema`, which must be idempotent.
    pub fn open(path: &Path, schema: &str) -> anyhow::Result<Db> {
        Self::init(Connection::open(path)?, schema)
    }

    pub fn open_in_memory(schema: &str) -> anyhow::Result<Db> {
        Self::init(Connection::open_in_memory()?, schema)
    }

    fn init(conn: Connection, schema: &str) -> anyhow::Result<Db> {
        // journal_mode returns a row, so query it rather than execute it.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(schema)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    // ponytail: single connection, switch to a pool if lock contention shows up
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> ApiResult<T> {
        let conn = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn).map_err(|e| {
            tracing::error!(error = %e, "database error");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            )
        })
    }
}
