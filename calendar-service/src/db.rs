pub use common::Db;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS calendars (
    id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, color TEXT NOT NULL,
    sync_token INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS calendars_user ON calendars(user_id);
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    uid TEXT NOT NULL, summary TEXT NOT NULL, description TEXT NOT NULL, location TEXT NOT NULL,
    all_day INTEGER NOT NULL, start TEXT NOT NULL, end TEXT NOT NULL, tz TEXT,
    rrule TEXT, exdates TEXT NOT NULL, reminders TEXT NOT NULL,
    recurring_event_id TEXT REFERENCES events(id), original_start TEXT,
    revision INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
    start_utc INTEGER NOT NULL, end_utc INTEGER,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS events_uid ON events(calendar_id, uid)
    WHERE deleted = 0 AND recurring_event_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS events_override ON events(recurring_event_id, original_start) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS events_range ON events(calendar_id, start_utc);
CREATE INDEX IF NOT EXISTS events_revision ON events(calendar_id, revision);
";
