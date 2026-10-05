use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use common::time::{When, parse_tz, parse_when};
use common::{ApiError, ApiJson, ApiResult, ErrorBody, PathId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{AppState, User, lists};

pub const MAX_TASKS_PER_LIST: i64 = 10_000;
pub(crate) const COLUMNS: &str = "id, list_id, uid, summary, description, due, tz, priority, completed_at, \
    reminders, parent_id, rrule, recurrence_id, revision, created_at, updated_at";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_task))
        .routes(routes!(get_task, replace_task, delete_task))
}

// ---- types ----

#[derive(Deserialize, ToSchema)]
pub struct TaskInput {
    /// 1 to 255 characters; defaults to the task's id.
    #[serde(default)]
    uid: Option<String>,
    /// Up to 500 characters.
    #[serde(default)]
    summary: String,
    /// Up to 10,000 characters.
    #[serde(default)]
    description: String,
    /// `YYYY-MM-DD` without `tz` (the task is due that day), `YYYY-MM-DDTHH:MM:SS` (wall-clock time in
    /// `tz`) with it; years 1900 to 2200.
    #[serde(default)]
    due: Option<String>,
    /// IANA zone name; goes with a timed `due`, forbidden without `due`.
    #[serde(default)]
    tz: Option<String>,
    /// 0 (none) to 9.
    #[serde(default)]
    priority: u8,
    /// Setting it stamps `completed_at`; clearing it clears `completed_at`.
    #[serde(default)]
    completed: bool,
    /// Up to 5 minutes-before-due values, each 0 to 40,320; needs `due`.
    #[serde(default)]
    reminders: Vec<u32>,
    /// Makes this a subtask of that task, which must be a task of the same list that is not itself a subtask;
    /// a subtask cannot have an `rrule`. On replace, absent or equal to the stored value.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// RFC 5545 rule body (no `RRULE:` prefix), up to 500 characters; DAILY or longer; needs `due`.
    #[serde(default)]
    rrule: Option<String>,
}

#[derive(Serialize, ToSchema, Clone)]
pub struct Task {
    pub(crate) id: Uuid,
    pub(crate) list_id: Uuid,
    uid: String,
    summary: String,
    description: String,
    #[schema(required)]
    pub(crate) due: Option<String>,
    #[schema(required)]
    pub(crate) tz: Option<String>,
    priority: u8,
    completed: bool,
    #[schema(required)]
    completed_at: Option<DateTime<Utc>>,
    reminders: Vec<u32>,
    #[schema(required)]
    pub(crate) parent_id: Option<Uuid>,
    #[schema(required)]
    pub(crate) rrule: Option<String>,
    #[schema(required)]
    recurrence_id: Option<Uuid>,
    /// The quoted revision, as sent in the `ETag` header; send it back in `If-Match`.
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

// ---- errors ----

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such task")
}

/// Either a database failure (500) or an answer for the client; lets a write use `?` for both.
pub(crate) enum Fail {
    Db(rusqlite::Error),
    Api(ApiError),
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Fail::Db(e)
    }
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Fail::Api(e)
    }
}

/// Runs `f` in one transaction under the database lock; an error of any kind rolls it back.
pub(crate) fn atomically<T>(
    s: &AppState,
    f: impl FnOnce(&Connection) -> Result<T, Fail>,
) -> ApiResult<T> {
    s.db.with(|c| {
        let tx = c.unchecked_transaction()?;
        match f(&tx) {
            Ok(v) => tx.commit().map(|()| Ok(v)),
            Err(Fail::Api(e)) => Ok(Err(e)),
            Err(Fail::Db(e)) => Err(e),
        }
    })?
}

// ---- validation ----

/// `due` and `tz` as stored → (When, zone used to read it, the instant at which it has passed).
pub(crate) fn due_parts(due: &str, tz: Option<&str>) -> ApiResult<(When, Tz, DateTime<Utc>)> {
    match tz {
        Some(z) => {
            let zone = parse_tz(z)?;
            let w = parse_when(due, false)?;
            Ok((w, zone, w.instant(zone)))
        }
        None => {
            let w = parse_when(due, true)?;
            let When::Date(d) = w else {
                unreachable!("parsed as a date")
            };
            let over = d.and_time(Default::default()).and_utc() + Duration::days(1);
            Ok((w, chrono_tz::UTC, over))
        }
    }
}

/// Checks that need no stored state; returns `due_utc`.
fn check(inp: &TaskInput) -> ApiResult<Option<i64>> {
    let text = |s: &str, max: usize| s.chars().count() <= max;
    if let Some(u) = &inp.uid
        && !(1..=255).contains(&u.chars().count())
    {
        return Err(invalid("uid must be 1 to 255 characters"));
    }
    if !text(&inp.summary, 500) {
        return Err(invalid("summary is at most 500 characters"));
    }
    if !text(&inp.description, 10_000) {
        return Err(invalid("description is at most 10,000 characters"));
    }
    if inp.priority > 9 {
        return Err(invalid("priority is 0 to 9"));
    }
    if inp.reminders.len() > 5 || inp.reminders.iter().any(|&m| m > 40_320) {
        return Err(invalid("reminders: at most 5, each 0 to 40320 minutes"));
    }
    if inp.parent_id.is_some() && inp.rrule.is_some() {
        return Err(invalid("a subtask cannot have an rrule"));
    }
    let Some(due) = &inp.due else {
        if inp.tz.is_some() {
            return Err(invalid("tz needs due"));
        }
        if !inp.reminders.is_empty() || inp.rrule.is_some() {
            return Err(invalid("reminders and rrule need due"));
        }
        return Ok(None);
    };
    let (start, zone, over) = due_parts(due, inp.tz.as_deref())?;
    if let Some(r) = &inp.rrule {
        if r.chars().count() > 500 {
            return Err(invalid("rrule is at most 500 characters"));
        }
        common::recur::validate(r, start, zone)?;
    }
    Ok(Some(over.timestamp()))
}

// ---- storage ----

pub(crate) fn from_row(r: &Row) -> rusqlite::Result<Task> {
    let id = |i| -> rusqlite::Result<Option<Uuid>> {
        Ok(r.get::<_, Option<String>>(i)?.and_then(|s| s.parse().ok()))
    };
    let ts = |i| -> rusqlite::Result<DateTime<Utc>> {
        Ok(DateTime::from_timestamp(r.get(i)?, 0).unwrap_or_default())
    };
    let completed_at = r
        .get::<_, Option<i64>>(8)?
        .map(|t| DateTime::from_timestamp(t, 0).unwrap_or_default());
    Ok(Task {
        id: id(0)?.unwrap_or_default(),
        list_id: id(1)?.unwrap_or_default(),
        uid: r.get(2)?,
        summary: r.get(3)?,
        description: r.get(4)?,
        due: r.get(5)?,
        tz: r.get(6)?,
        priority: r.get(7)?,
        completed: completed_at.is_some(),
        completed_at,
        reminders: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        parent_id: id(10)?,
        rrule: r.get(11)?,
        recurrence_id: id(12)?,
        etag: format!("\"{}\"", r.get::<_, i64>(13)?),
        created_at: ts(14)?,
        updated_at: ts(15)?,
    })
}

/// The live task `id`, if it is in one of `user`'s lists.
fn load(c: &Connection, user: Uuid, id: Uuid) -> rusqlite::Result<Option<Task>> {
    c.query_row(
        &format!(
            "SELECT {COLUMNS} FROM tasks WHERE id = ?1 AND deleted = 0
             AND list_id IN (SELECT id FROM lists WHERE user_id = ?2)"
        ),
        params![id.to_string(), user.to_string()],
        from_row,
    )
    .optional()
}

/// Live subtasks of `parent`, oldest first.
pub(crate) fn subtasks(c: &Connection, parent: Uuid) -> rusqlite::Result<Vec<Task>> {
    c.prepare(&format!(
        "SELECT {COLUMNS} FROM tasks WHERE parent_id = ?1 AND deleted = 0 ORDER BY created_at, rowid"
    ))?
    .query_map([parent.to_string()], from_row)?
    .collect()
}

/// Bumps the list's sync token and returns the new value, the revision of the write that follows.
/// The caller has checked that the list is the user's.
pub(crate) fn bump(c: &Connection, list: Uuid) -> rusqlite::Result<i64> {
    c.query_row(
        "UPDATE lists SET sync_token = sync_token + 1 WHERE id = ?1 RETURNING sync_token",
        [list.to_string()],
        |r| r.get(0),
    )
}

/// How many live tasks `list` holds.
// ponytail: counted on every create, over all the list's rows (tombstones too); keep the count on the
// list row if writes get slow
pub(crate) fn live_count(c: &Connection, list: Uuid) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM tasks WHERE list_id = ?1 AND deleted = 0",
        [list.to_string()],
        |r| r.get(0),
    )
}

fn with_etag(
    status: StatusCode,
    t: Task,
) -> (StatusCode, [(header::HeaderName, String); 1], Json<Task>) {
    (status, [(header::ETAG, t.etag.clone())], Json(t))
}

/// 412 unless `If-Match` is absent or equals `etag` (compared after trimming whitespace).
fn check_if_match(h: &HeaderMap, etag: &str) -> Result<(), ApiError> {
    match h.get(header::IF_MATCH) {
        Some(v) if v.to_str().ok().map(str::trim) != Some(etag) => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "etag_mismatch",
            "the task has changed; read it again",
        )),
        _ => Ok(()),
    }
}

// ---- handlers ----

const ID_PARAMS: &str = "UUID";

/// Create a task
///
/// A date `due` makes an all-day deadline; a timed one is a wall-clock time in `tz`.
#[utoipa::path(
    post, path = "/tasks/v1/lists/{id}/tasks",
    tag = "tasks",
    params(
        ("id" = String, Path, description = "the list's UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body(content = TaskInput, example = json!({"summary": "Buy milk", "due": "2026-10-07T09:00:00",
        "tz": "Europe/Warsaw", "reminders": [10]})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 201, description = "created; `ETag` header carries the etag", body = Task,
        headers(("ETag" = String, description = "The task's etag, as in the body"))),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
    (status = 409, description = "`conflict`: the uid is in use, or the list already holds 10,000 tasks", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, a limit exceeded, bad due or zone, bad rrule, or a parent_id that is not a task of this list or is itself a subtask, or an rrule on a subtask", body = ErrorBody),
)
)]
async fn create_task(
    State(s): State<AppState>,
    User(user): User,
    PathId(list): PathId,
    ApiJson(inp): ApiJson<TaskInput>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let due_utc = check(&inp)?;
    let now = s.now().timestamp();
    let task = atomically(&s, |c| {
        lists::owned(c, user, list)?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such list"))?;
        if live_count(c, list)? >= MAX_TASKS_PER_LIST {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "conflict",
                "this list already holds 10,000 tasks",
            )
            .into());
        }
        if let Some(p) = inp.parent_id {
            let usable: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND list_id = ?2 AND deleted = 0
                    AND parent_id IS NULL)",
                params![p.to_string(), list.to_string()],
                |r| r.get(0),
            )?;
            if !usable {
                return Err(invalid(
                    "parent_id must name a task of this list that is not a subtask",
                )
                .into());
            }
        }
        let id = Uuid::new_v4();
        let uid = inp.uid.clone().unwrap_or_else(|| id.to_string());
        let taken: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE list_id = ?1 AND uid = ?2 AND deleted = 0)",
            params![list.to_string(), uid],
            |r| r.get(0),
        )?;
        if taken {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "conflict",
                "uid is already in use in this list",
            )
            .into());
        }
        let revision = bump(c, list)?;
        c.execute(
            "INSERT INTO tasks (id, list_id, uid, summary, description, due, tz, priority, completed_at,
                reminders, parent_id, rrule, recurrence_id, revision, due_utc, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
            params![
                id.to_string(), list.to_string(), uid, inp.summary, inp.description, inp.due, inp.tz,
                inp.priority, inp.completed.then_some(now),
                serde_json::to_string(&inp.reminders).unwrap_or_default(),
                inp.parent_id.map(|p| p.to_string()), inp.rrule,
                inp.rrule.as_ref().map(|_| id.to_string()), revision, due_utc, now,
            ],
        )?;
        Ok(load(c, user, id)?.expect("row just inserted"))
    })?;
    tracing::info!(event = "task_created", user_id = %user, list_id = %list, task_id = %task.id);
    Ok(with_etag(StatusCode::CREATED, task))
}

/// Get a task
///
/// The `ETag` header carries the etag.
#[utoipa::path(
    get, path = "/tasks/v1/tasks/{id}",
    tag = "tasks",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the task", body = Task,
        headers(("ETag" = String, description = "The task's etag, as in the body"))),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, deleted, or not the caller's", body = ErrorBody),
)
)]
async fn get_task(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let t = s.db.with(|c| load(c, user, id))?.ok_or_else(not_found)?;
    Ok(with_etag(StatusCode::OK, t))
}

/// Replace a task
///
/// The body is the whole task. `uid` and `parent_id` may be left out; when given they must equal the
/// stored values. `completed` changing stamps or clears `completed_at`. With `If-Match`, the write happens
/// only if it equals the current etag.
#[utoipa::path(
    put, path = "/tasks/v1/tasks/{id}",
    tag = "tasks",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("If-Match" = Option<String>, Header, description = "The etag the client last saw"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body = TaskInput,
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the replaced task; `ETag` header carries the new etag", body = Task,
        headers(("ETag" = String, description = "The task's etag, as in the body"))),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, deleted, or not the caller's", body = ErrorBody),
    (status = 412, description = "`etag_mismatch`: `If-Match` is not the current etag", body = ErrorBody),
    (status = 422, description = "`validation`: as for create, or an immutable field differs", body = ErrorBody),
)
)]
async fn replace_task(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
    headers: HeaderMap,
    ApiJson(inp): ApiJson<TaskInput>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let due_utc = check(&inp)?;
    let now = s.now().timestamp();
    let task = atomically(&s, |c| {
        let old = load(c, user, id)?.ok_or_else(not_found)?;
        check_if_match(&headers, &old.etag)?;
        if inp.uid.as_ref().is_some_and(|u| *u != old.uid)
            || inp.parent_id.is_some_and(|p| Some(p) != old.parent_id)
        {
            return Err(invalid("uid and parent_id cannot change").into());
        }
        if old.parent_id.is_some() && inp.rrule.is_some() {
            return Err(invalid("a subtask cannot have an rrule").into());
        }
        let completed_at = match (old.completed_at, inp.completed) {
            (Some(t), true) => Some(t.timestamp()),
            (None, true) => Some(now),
            (_, false) => None,
        };
        let recurrence_id = match (&old.rrule, &inp.rrule) {
            (None, Some(_)) => Some(id),
            _ => old.recurrence_id,
        };
        let revision = bump(c, old.list_id)?;
        c.execute(
            "UPDATE tasks SET summary = ?2, description = ?3, due = ?4, tz = ?5, priority = ?6,
                completed_at = ?7, reminders = ?8, rrule = ?9, recurrence_id = ?10, revision = ?11,
                due_utc = ?12, updated_at = ?13
             WHERE id = ?1",
            params![
                id.to_string(),
                inp.summary,
                inp.description,
                inp.due,
                inp.tz,
                inp.priority,
                completed_at,
                serde_json::to_string(&inp.reminders).unwrap_or_default(),
                inp.rrule,
                recurrence_id.map(|u| u.to_string()),
                revision,
                due_utc,
                now,
            ],
        )?;
        Ok(load(c, user, id)?.expect("row just updated"))
    })?;
    tracing::info!(event = "task_updated", user_id = %user, list_id = %task.list_id, task_id = %id);
    Ok(with_etag(StatusCode::OK, task))
}

/// Delete a task
///
/// Deletes its subtasks too. With `If-Match`, the delete happens only if it equals the current etag.
#[utoipa::path(
    delete, path = "/tasks/v1/tasks/{id}",
    tag = "tasks",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("If-Match" = Option<String>, Header, description = "The etag the client last saw"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 204, description = "deleted"),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, already deleted, or not the caller's", body = ErrorBody),
    (status = 412, description = "`etag_mismatch`: `If-Match` is not the current etag", body = ErrorBody),
)
)]
async fn delete_task(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let now = s.now().timestamp();
    let list = atomically(&s, |c| {
        let old = load(c, user, id)?.ok_or_else(not_found)?;
        check_if_match(&headers, &old.etag)?;
        let doomed = subtasks(c, id)?.into_iter().map(|t| t.id);
        for t in std::iter::once(id).chain(doomed) {
            let revision = bump(c, old.list_id)?;
            c.execute(
                "UPDATE tasks SET deleted = 1, revision = ?2, updated_at = ?3 WHERE id = ?1",
                params![t.to_string(), revision, now],
            )?;
        }
        Ok(old.list_id)
    })?;
    tracing::info!(event = "task_deleted", user_id = %user, list_id = %list, task_id = %id);
    Ok(StatusCode::NO_CONTENT)
}
