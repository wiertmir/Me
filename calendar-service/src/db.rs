pub use common::Db;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS calendars (
    id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, color TEXT NOT NULL,
    sync_token INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS calendars_user ON calendars(user_id);
";
