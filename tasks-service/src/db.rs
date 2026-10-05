pub use common::Db;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lists (
    id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, color TEXT NOT NULL,
    sync_token INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS lists_user ON lists(user_id);
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    list_id TEXT NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    uid TEXT NOT NULL, summary TEXT NOT NULL, description TEXT NOT NULL,
    due TEXT, tz TEXT, priority INTEGER NOT NULL DEFAULT 0, completed_at INTEGER,
    reminders TEXT NOT NULL, parent_id TEXT REFERENCES tasks(id),
    rrule TEXT, recurrence_id TEXT,
    revision INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0, due_utc INTEGER,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_uid ON tasks(list_id, uid) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS tasks_revision ON tasks(list_id, revision);
CREATE INDEX IF NOT EXISTS tasks_parent ON tasks(parent_id) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS tasks_heads ON tasks(due_utc) WHERE rrule IS NOT NULL AND deleted = 0;
";
