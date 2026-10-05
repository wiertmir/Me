use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use common::{ApiError, ApiJson, ErrorBody, PathId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{AppState, User};

const DEFAULT_COLOR: &str = "#3b82f6";
const MAX_LISTS: i64 = 100;
const COLUMNS: &str = "id, name, color, sync_token, created_at, updated_at";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_lists, create_list))
        .routes(routes!(get_list, patch_list, delete_list))
}

#[derive(Serialize, ToSchema)]
pub struct List {
    id: Uuid,
    name: String,
    /// `#rrggbb`
    color: String,
    /// Bumped by every change to the list's tasks; clients use it to detect changes.
    pub(crate) sync_token: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct ListInput {
    /// 1 to 100 characters.
    name: String,
    /// `#rrggbb`; defaults to `#3b82f6`.
    color: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ListPatch {
    /// 1 to 100 characters.
    name: Option<String>,
    /// `#rrggbb`
    color: Option<String>,
}

fn from_row(r: &Row) -> rusqlite::Result<List> {
    let ts = |i| -> rusqlite::Result<DateTime<Utc>> {
        Ok(DateTime::from_timestamp(r.get(i)?, 0).unwrap_or_default())
    };
    Ok(List {
        id: r.get::<_, String>(0)?.parse().unwrap_or_default(),
        name: r.get(1)?,
        color: r.get(2)?,
        sync_token: r.get(3)?,
        created_at: ts(4)?,
        updated_at: ts(5)?,
    })
}

/// `None` when the list does not exist or is not `user`'s; callers answer that with 404 `not_found`.
pub fn owned(c: &Connection, user: Uuid, list: Uuid) -> rusqlite::Result<Option<List>> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM lists WHERE id = ?1 AND user_id = ?2"),
        params![list.to_string(), user.to_string()],
        from_row,
    )
    .optional()
}

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such list")
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

/// Inserts a list for `user` and returns it.
fn insert(c: &Connection, user: Uuid, name: &str, color: &str, now: i64) -> rusqlite::Result<List> {
    let id = Uuid::new_v4();
    c.execute(
        "INSERT INTO lists (id, user_id, name, color, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id.to_string(), user.to_string(), name, color, now],
    )?;
    Ok(owned(c, user, id)?.expect("row just inserted"))
}

/// List the user's lists
///
/// Oldest first. A user with no list gets a default one, "Tasks", on the first call.
#[utoipa::path(
    get, path = "/tasks/v1/lists",
    tag = "lists",
    params(("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`")),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the lists", body = Vec<List>),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
)
)]
async fn list_lists(
    State(s): State<AppState>,
    User(user): User,
) -> Result<Json<Vec<List>>, ApiError> {
    let now = s.now().timestamp();
    let list = s.db.with(|c| {
        let uid = user.to_string();
        let mut list = c
            .prepare(&format!(
                "SELECT {COLUMNS} FROM lists WHERE user_id = ?1 ORDER BY created_at, rowid"
            ))?
            .query_map([&uid], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if list.is_empty() {
            list.push(insert(c, user, "Tasks", DEFAULT_COLOR, now)?);
        }
        Ok(list)
    })?;
    Ok(Json(list))
}

/// Create a list
///
/// A user can have up to 100 lists.
#[utoipa::path(
    post, path = "/tasks/v1/lists",
    tag = "lists",
    params(("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`")),
    request_body(content = ListInput, example = json!({"name": "Work", "color": "#ff0000"})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 201, description = "created", body = List),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 409, description = "`conflict`: 100 lists already exist", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, name not 1 to 100 characters, or color not `#rrggbb`", body = ErrorBody),
)
)]
async fn create_list(
    State(s): State<AppState>,
    User(user): User,
    ApiJson(req): ApiJson<ListInput>,
) -> Result<(StatusCode, Json<List>), ApiError> {
    check_name(&req.name)?;
    let color = req.color.as_deref().unwrap_or(DEFAULT_COLOR);
    check_color(color)?;
    let now = s.now().timestamp();
    // Count and insert under one lock, so concurrent creates cannot overshoot the limit.
    let created = s.db.with(|c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM lists WHERE user_id = ?1",
            [user.to_string()],
            |r| r.get(0),
        )?;
        if n >= MAX_LISTS {
            return Ok(None);
        }
        insert(c, user, &req.name, color, now).map(Some)
    })?;
    let list = created.ok_or_else(|| {
        ApiError::new(StatusCode::CONFLICT, "conflict", "100 lists already exist")
    })?;
    tracing::info!(event = "list_created", user_id = %user, list_id = %list.id);
    Ok((StatusCode::CREATED, Json(list)))
}

/// Get a list
#[utoipa::path(
    get, path = "/tasks/v1/lists/{id}",
    tag = "lists",
    params(
        ("id" = String, Path, description = "UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the list", body = List),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
)
)]
async fn get_list(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
) -> Result<Json<List>, ApiError> {
    s.db.with(|c| owned(c, user, id))?
        .map(Json)
        .ok_or_else(not_found)
}

/// Update a list
///
/// Only the fields present are changed.
#[utoipa::path(
    patch, path = "/tasks/v1/lists/{id}",
    tag = "lists",
    params(
        ("id" = String, Path, description = "UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body(content = ListPatch, example = json!({"name": "Job"})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the updated list", body = List),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 503, description = "`unavailable`: the keys to verify the access token cannot be fetched", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, name not 1 to 100 characters, or color not `#rrggbb`", body = ErrorBody),
)
)]
async fn patch_list(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
    ApiJson(req): ApiJson<ListPatch>,
) -> Result<Json<List>, ApiError> {
    if let Some(n) = &req.name {
        check_name(n)?;
    }
    if let Some(col) = &req.color {
        check_color(col)?;
    }
    let now = s.now().timestamp();
    let cal = s.db.with(|c| {
        c.execute(
            "UPDATE lists SET name = COALESCE(?3, name), color = COALESCE(?4, color), updated_at = ?5
             WHERE id = ?1 AND user_id = ?2",
            params![id.to_string(), user.to_string(), req.name, req.color, now],
        )?;
        owned(c, user, id)
    })?;
    let list = cal.ok_or_else(not_found)?;
    tracing::info!(event = "list_updated", user_id = %user, list_id = %id);
    Ok(Json(list))
}

/// Delete a list
///
/// Deletes the list with its tasks.
#[utoipa::path(
    delete, path = "/tasks/v1/lists/{id}",
    tag = "lists",
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
async fn delete_list(
    State(s): State<AppState>,
    User(user): User,
    PathId(id): PathId,
) -> Result<StatusCode, ApiError> {
    let n = s.db.with(|c| {
        c.execute(
            "DELETE FROM lists WHERE id = ?1 AND user_id = ?2",
            params![id.to_string(), user.to_string()],
        )
    })?;
    if n == 0 {
        return Err(not_found());
    }
    tracing::info!(event = "list_deleted", user_id = %user, list_id = %id);
    Ok(StatusCode::NO_CONTENT)
}
