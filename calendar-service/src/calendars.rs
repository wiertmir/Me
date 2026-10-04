use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use common::{ApiError, ApiJson, ErrorBody, PathId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{AppState, Caller};

const DEFAULT_COLOR: &str = "#3b82f6";
const MAX_CALENDARS: i64 = 100;
const COLUMNS: &str = "id, name, color, sync_token, created_at, updated_at";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_calendars, create_calendar))
        .routes(routes!(get_calendar, patch_calendar, delete_calendar))
}

#[derive(Serialize, ToSchema)]
pub struct Calendar {
    id: Uuid,
    name: String,
    /// `#rrggbb`
    color: String,
    /// Bumped by every change to the calendar's events; clients use it to detect changes.
    pub(crate) sync_token: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct CalendarInput {
    /// 1 to 100 characters.
    name: String,
    /// `#rrggbb`; defaults to `#3b82f6`.
    color: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct CalendarPatch {
    /// 1 to 100 characters.
    name: Option<String>,
    /// `#rrggbb`
    color: Option<String>,
}

fn from_row(r: &Row) -> rusqlite::Result<Calendar> {
    let ts = |i| -> rusqlite::Result<DateTime<Utc>> {
        Ok(DateTime::from_timestamp(r.get(i)?, 0).unwrap_or_default())
    };
    Ok(Calendar {
        id: r.get::<_, String>(0)?.parse().unwrap_or_default(),
        name: r.get(1)?,
        color: r.get(2)?,
        sync_token: r.get(3)?,
        created_at: ts(4)?,
        updated_at: ts(5)?,
    })
}

/// `None` when the calendar does not exist or is not `user`'s; callers answer that with 404 `not_found`.
pub fn owned(c: &Connection, user: Uuid, calendar: Uuid) -> rusqlite::Result<Option<Calendar>> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM calendars WHERE id = ?1 AND user_id = ?2"),
        params![calendar.to_string(), user.to_string()],
        from_row,
    )
    .optional()
}

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such calendar")
}

fn check_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() || name.chars().count() > 100 {
        return Err(invalid("name must be 1 to 100 characters"));
    }
    Ok(())
}

fn check_color(color: &str) -> Result<(), ApiError> {
    let b = color.as_bytes();
    if b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit) {
        return Ok(());
    }
    Err(invalid("color must be #rrggbb"))
}

/// Inserts a calendar for `user` and returns it.
fn insert(c: &Connection, user: Uuid, name: &str, color: &str) -> rusqlite::Result<Calendar> {
    let id = Uuid::new_v4();
    let now = Utc::now().timestamp();
    c.execute(
        "INSERT INTO calendars (id, user_id, name, color, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id.to_string(), user.to_string(), name, color, now],
    )?;
    Ok(owned(c, user, id)?.expect("row just inserted"))
}

/// List the user's calendars
///
/// Oldest first. A user with no calendar gets a default one, "Personal", on the first call.
#[utoipa::path(
    get, path = "/calendar/v1/calendars",
    tag = "calendars",
    params(("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`")),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the calendars", body = Vec<Calendar>),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
)
)]
async fn list_calendars(
    State(s): State<AppState>,
    Caller(user): Caller,
) -> Result<Json<Vec<Calendar>>, ApiError> {
    let list = s.db.with(|c| {
        let uid = user.to_string();
        let mut list = c
            .prepare(&format!(
                "SELECT {COLUMNS} FROM calendars WHERE user_id = ?1 ORDER BY created_at, rowid"
            ))?
            .query_map([&uid], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if list.is_empty() {
            list.push(insert(c, user, "Personal", DEFAULT_COLOR)?);
        }
        Ok(list)
    })?;
    Ok(Json(list))
}

/// Create a calendar
///
/// A user can have up to 100 calendars.
#[utoipa::path(
    post, path = "/calendar/v1/calendars",
    tag = "calendars",
    params(("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`")),
    request_body(content = CalendarInput, example = json!({"name": "Work", "color": "#ff0000"})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 201, description = "created", body = Calendar),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 409, description = "`conflict`: 100 calendars already exist", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, name not 1 to 100 characters, or color not `#rrggbb`", body = ErrorBody),
)
)]
async fn create_calendar(
    State(s): State<AppState>,
    Caller(user): Caller,
    ApiJson(req): ApiJson<CalendarInput>,
) -> Result<(StatusCode, Json<Calendar>), ApiError> {
    check_name(&req.name)?;
    let color = req.color.as_deref().unwrap_or(DEFAULT_COLOR);
    check_color(color)?;
    // Count and insert under one lock, so concurrent creates cannot overshoot the limit.
    let created = s.db.with(|c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM calendars WHERE user_id = ?1",
            [user.to_string()],
            |r| r.get(0),
        )?;
        if n >= MAX_CALENDARS {
            return Ok(None);
        }
        insert(c, user, &req.name, color).map(Some)
    })?;
    let cal = created.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "100 calendars already exist",
        )
    })?;
    tracing::info!(event = "calendar_created", user_id = %user, calendar_id = %cal.id);
    Ok((StatusCode::CREATED, Json(cal)))
}

/// Get a calendar
#[utoipa::path(
    get, path = "/calendar/v1/calendars/{id}",
    tag = "calendars",
    params(
        ("id" = String, Path, description = "UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the calendar", body = Calendar),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
)
)]
async fn get_calendar(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
) -> Result<Json<Calendar>, ApiError> {
    s.db.with(|c| owned(c, user, id))?
        .map(Json)
        .ok_or_else(not_found)
}

/// Update a calendar
///
/// Only the fields present are changed.
#[utoipa::path(
    patch, path = "/calendar/v1/calendars/{id}",
    tag = "calendars",
    params(
        ("id" = String, Path, description = "UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body(content = CalendarPatch, example = json!({"name": "Job"})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the updated calendar", body = Calendar),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, name not 1 to 100 characters, or color not `#rrggbb`", body = ErrorBody),
)
)]
async fn patch_calendar(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
    ApiJson(req): ApiJson<CalendarPatch>,
) -> Result<Json<Calendar>, ApiError> {
    if let Some(n) = &req.name {
        check_name(n)?;
    }
    if let Some(col) = &req.color {
        check_color(col)?;
    }
    let cal = s.db.with(|c| {
        c.execute(
            "UPDATE calendars SET name = COALESCE(?3, name), color = COALESCE(?4, color), updated_at = ?5
             WHERE id = ?1 AND user_id = ?2",
            params![id.to_string(), user.to_string(), req.name, req.color, Utc::now().timestamp()],
        )?;
        owned(c, user, id)
    })?;
    let cal = cal.ok_or_else(not_found)?;
    tracing::info!(event = "calendar_updated", user_id = %user, calendar_id = %id);
    Ok(Json(cal))
}

/// Delete a calendar
///
/// Deletes the calendar with its events.
#[utoipa::path(
    delete, path = "/calendar/v1/calendars/{id}",
    tag = "calendars",
    params(
        ("id" = String, Path, description = "UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 204, description = "deleted"),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
)
)]
async fn delete_calendar(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
) -> Result<StatusCode, ApiError> {
    let n = s.db.with(|c| {
        c.execute(
            "DELETE FROM calendars WHERE id = ?1 AND user_id = ?2",
            params![id.to_string(), user.to_string()],
        )
    })?;
    if n == 0 {
        return Err(not_found());
    }
    tracing::info!(event = "calendar_deleted", user_id = %user, calendar_id = %id);
    Ok(StatusCode::NO_CONTENT)
}
