use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use common::{ApiError, ApiJson, ApiResult, ErrorBody, PathId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    AppState, Caller, calendars, recur,
    time::{When, parse_tz, parse_when},
};

const COLUMNS: &str = "id, calendar_id, uid, summary, description, location, all_day, start, \"end\", tz, \
    rrule, exdates, reminders, recurring_event_id, original_start, revision, created_at, updated_at";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_event, list_occurrences))
        .routes(routes!(get_event, replace_event, delete_event))
}

// ---- types ----

#[derive(Deserialize, ToSchema)]
pub struct EventInput {
    /// 1 to 255 characters; defaults to the event's id. An override takes its series' uid.
    #[serde(default)]
    uid: Option<String>,
    /// Up to 500 characters.
    #[serde(default)]
    summary: String,
    /// Up to 10,000 characters.
    #[serde(default)]
    description: String,
    /// Up to 500 characters.
    #[serde(default)]
    location: String,
    all_day: bool,
    /// `YYYY-MM-DD` for an all-day event, `YYYY-MM-DDTHH:MM:SS` (wall-clock time in `tz`) otherwise.
    start: String,
    /// Same form as `start`; exclusive for an all-day event, and after `start` as an instant.
    end: String,
    /// IANA zone name; required for a timed event, forbidden for an all-day one.
    #[serde(default)]
    tz: Option<String>,
    /// RFC 5545 rule body (no `RRULE:` prefix), up to 500 characters; DAILY or longer.
    #[serde(default)]
    rrule: Option<String>,
    /// Up to 1,000 occurrence starts to skip, in the form of `start`.
    #[serde(default)]
    exdates: Vec<String>,
    /// Up to 5 minutes-before-start values, each 0 to 40,320.
    #[serde(default)]
    reminders: Vec<u32>,
    /// Makes this an override of one occurrence of that series; goes with `original_start`.
    #[serde(default)]
    recurring_event_id: Option<Uuid>,
    /// The occurrence start this event replaces, in the form of `start`.
    #[serde(default)]
    original_start: Option<String>,
}

#[derive(Serialize, ToSchema, Clone)]
pub struct Event {
    id: Uuid,
    calendar_id: Uuid,
    uid: String,
    summary: String,
    description: String,
    location: String,
    all_day: bool,
    start: String,
    end: String,
    tz: Option<String>,
    rrule: Option<String>,
    exdates: Vec<String>,
    reminders: Vec<u32>,
    recurring_event_id: Option<Uuid>,
    original_start: Option<String>,
    /// The quoted revision, as sent in the `ETag` header; send it back in `If-Match`.
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// One occurrence in a range: the event's fields, with `start`, `end` and `original_start` set for this
/// occurrence, and the instants it spans.
#[derive(Serialize, ToSchema)]
pub struct Occurrence {
    #[serde(flatten)]
    event: Event,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RangeQuery {
    from: Option<String>,
    to: Option<String>,
    tz: Option<String>,
}

// ---- errors ----

fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such event")
}

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "conflict", message)
}

/// Either a database failure (500) or an answer for the client; lets a write use `?` for both.
enum Fail {
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
fn atomically<T>(s: &AppState, f: impl FnOnce(&Connection) -> Result<T, Fail>) -> ApiResult<T> {
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

/// An input that passed every check that needs no stored state.
struct Checked {
    start_utc: i64,
    /// `None` for a series: see `// ponytail:` below.
    end_utc: Option<i64>,
}

fn check(inp: &EventInput) -> ApiResult<Checked> {
    let text = |s: &str, max: usize| s.chars().count() <= max;
    if let Some(u) = &inp.uid
        && !(1..=255).contains(&u.chars().count())
    {
        return Err(invalid("uid must be 1 to 255 characters"));
    }
    if !text(&inp.summary, 500) || !text(&inp.location, 500) {
        return Err(invalid("summary and location are at most 500 characters"));
    }
    if !text(&inp.description, 10_000) {
        return Err(invalid("description is at most 10,000 characters"));
    }
    if inp.reminders.len() > 5 || inp.reminders.iter().any(|&m| m > 40_320) {
        return Err(invalid("reminders: at most 5, each 0 to 40320 minutes"));
    }
    if inp.exdates.len() > 1000 {
        return Err(invalid("exdates: at most 1000 entries"));
    }
    let zone = match (&inp.tz, inp.all_day) {
        (None, false) => return Err(invalid("a timed event needs tz")),
        (Some(_), true) => return Err(invalid("an all-day event has no tz")),
        (Some(tz), false) => parse_tz(tz)?,
        (None, true) => chrono_tz::UTC,
    };
    let start = parse_when(&inp.start, inp.all_day)?;
    let end = parse_when(&inp.end, inp.all_day)?;
    // On instants, so a start and end that both land in a DST gap are judged by where they map.
    if end.instant(zone) <= start.instant(zone) {
        return Err(invalid("end must be after start"));
    }
    for x in &inp.exdates {
        parse_when(x, inp.all_day)?;
    }
    if let Some(r) = &inp.rrule {
        if r.chars().count() > 500 {
            return Err(invalid("rrule is at most 500 characters"));
        }
        recur::validate(r, start, zone)?;
    }
    match (&inp.recurring_event_id, &inp.original_start) {
        (None, None) => {}
        (Some(_), Some(o)) => {
            parse_when(o, inp.all_day)?;
            if inp.rrule.is_some() || !inp.exdates.is_empty() {
                return Err(invalid("an override has no rrule and no exdates"));
            }
        }
        _ => return Err(invalid("recurring_event_id and original_start go together")),
    }
    Ok(Checked {
        start_utc: start.instant(zone).timestamp(),
        // ponytail: a series has no end instant, so every series of the calendar is expanded per range
        // query; store the last occurrence's end here and filter on it if calendars grow many series.
        end_utc: inp.rrule.is_none().then(|| end.instant(zone).timestamp()),
    })
}

// ---- storage ----

fn from_row(r: &Row) -> rusqlite::Result<Event> {
    let id =
        |i| -> rusqlite::Result<Uuid> { Ok(r.get::<_, String>(i)?.parse().unwrap_or_default()) };
    let ts = |i| -> rusqlite::Result<DateTime<Utc>> {
        Ok(DateTime::from_timestamp(r.get(i)?, 0).unwrap_or_default())
    };
    let list = |i| -> rusqlite::Result<String> { r.get(i) };
    Ok(Event {
        id: id(0)?,
        calendar_id: id(1)?,
        uid: r.get(2)?,
        summary: r.get(3)?,
        description: r.get(4)?,
        location: r.get(5)?,
        all_day: r.get(6)?,
        start: r.get(7)?,
        end: r.get(8)?,
        tz: r.get(9)?,
        rrule: r.get(10)?,
        exdates: serde_json::from_str(&list(11)?).unwrap_or_default(),
        reminders: serde_json::from_str(&list(12)?).unwrap_or_default(),
        recurring_event_id: r.get::<_, Option<String>>(13)?.and_then(|s| s.parse().ok()),
        original_start: r.get(14)?,
        etag: format!("\"{}\"", r.get::<_, i64>(15)?),
        created_at: ts(16)?,
        updated_at: ts(17)?,
    })
}

/// The live event `id`, if it is in one of `user`'s calendars.
fn load(c: &Connection, user: Uuid, id: Uuid) -> rusqlite::Result<Option<Event>> {
    c.query_row(
        &format!(
            "SELECT {COLUMNS} FROM events WHERE id = ?1 AND deleted = 0
             AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?2)"
        ),
        params![id.to_string(), user.to_string()],
        from_row,
    )
    .optional()
}

/// Bumps the calendar's sync token and returns the new value, the revision of the write that follows.
fn bump(c: &Connection, user: Uuid, calendar: Uuid) -> rusqlite::Result<i64> {
    c.query_row(
        "UPDATE calendars SET sync_token = sync_token + 1 WHERE id = ?1 AND user_id = ?2 RETURNING sync_token",
        params![calendar.to_string(), user.to_string()],
        |r| r.get(0),
    )
}

fn exists(c: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<bool> {
    c.query_row(&format!("SELECT EXISTS({sql})"), p, |r| r.get(0))
}

fn with_etag(
    status: StatusCode,
    ev: Event,
) -> (StatusCode, [(header::HeaderName, String); 1], Json<Event>) {
    (status, [(header::ETAG, ev.etag.clone())], Json(ev))
}

/// 412 unless `If-Match` is absent or equals `etag` (compared after trimming whitespace).
fn check_if_match(h: &HeaderMap, etag: &str) -> Result<(), ApiError> {
    match h.get(header::IF_MATCH) {
        Some(v) if v.to_str().ok().map(str::trim) != Some(etag) => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "etag_mismatch",
            "the event has changed; read it again",
        )),
        _ => Ok(()),
    }
}

// ---- handlers ----

const ID_PARAMS: &str = "UUID";

/// Create an event
///
/// Timed events are wall-clock times in `tz`; all-day events are dates. With `recurring_event_id` and
/// `original_start` the event overrides one occurrence of a series in the same calendar.
#[utoipa::path(
    post, path = "/calendar/v1/calendars/{id}/events",
    tag = "events",
    params(
        ("id" = String, Path, description = "the calendar's UUID"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body(content = EventInput, example = json!({"summary": "Dentist", "all_day": false,
        "start": "2026-10-05T09:00:00", "end": "2026-10-05T10:00:00", "tz": "Europe/Warsaw", "reminders": [10]})),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 201, description = "created; `ETag` header carries the etag", body = Event),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
    (status = 409, description = "`conflict`: the uid is in use, or the occurrence already has an override", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, a limit exceeded, bad times or zone, bad rrule, or bad override", body = ErrorBody),
)
)]
async fn create_event(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(calendar): PathId,
    ApiJson(inp): ApiJson<EventInput>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let ck = check(&inp)?;
    let ev = atomically(&s, |c| {
        calendars::owned(c, user, calendar)?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such calendar"))?;
        let id = Uuid::new_v4();
        let cal = calendar.to_string();
        let uid = match inp.recurring_event_id {
            Some(series) => {
                let row = c
                    .query_row(
                        "SELECT uid, all_day FROM events WHERE id = ?1 AND calendar_id = ?2 AND deleted = 0
                         AND recurring_event_id IS NULL AND rrule IS NOT NULL AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?3)",
                        params![series.to_string(), cal, user.to_string()],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
                    )
                    .optional()?;
                let Some((uid, all_day)) = row else {
                    return Err(
                        invalid("recurring_event_id is not a series of this calendar").into(),
                    );
                };
                if all_day != inp.all_day || inp.uid.as_ref().is_some_and(|u| *u != uid) {
                    return Err(invalid("an override has its series' uid and all_day").into());
                }
                let taken = exists(
                    c,
                    "SELECT 1 FROM events WHERE recurring_event_id = ?1 AND original_start = ?2 AND deleted = 0 AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?3)",
                    params![series.to_string(), inp.original_start, user.to_string()],
                )?;
                if taken {
                    return Err(conflict("that occurrence already has an override").into());
                }
                uid
            }
            None => {
                let uid = inp.uid.clone().unwrap_or_else(|| id.to_string());
                let taken = exists(
                    c,
                    "SELECT 1 FROM events WHERE calendar_id = ?1 AND uid = ?2 AND deleted = 0 AND recurring_event_id IS NULL AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?3)",
                    params![cal, uid, user.to_string()],
                )?;
                if taken {
                    return Err(conflict("uid is already in use in this calendar").into());
                }
                uid
            }
        };
        let revision = bump(c, user, calendar)?;
        let now = Utc::now().timestamp();
        c.execute(
            "INSERT INTO events (id, calendar_id, uid, summary, description, location, all_day, start, \"end\", tz,
                rrule, exdates, reminders, recurring_event_id, original_start, revision, start_utc, end_utc,
                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?19)",
            params![
                id.to_string(), cal, uid, inp.summary, inp.description, inp.location, inp.all_day,
                inp.start, inp.end, inp.tz, inp.rrule,
                serde_json::to_string(&inp.exdates).unwrap_or_default(),
                serde_json::to_string(&inp.reminders).unwrap_or_default(),
                inp.recurring_event_id.map(|u| u.to_string()), inp.original_start,
                revision, ck.start_utc, ck.end_utc, now,
            ],
        )?;
        Ok(load(c, user, id)?.expect("row just inserted"))
    })?;
    tracing::info!(event = "event_created", user_id = %user, calendar_id = %calendar, event_id = %ev.id);
    Ok(with_etag(StatusCode::CREATED, ev))
}

/// Get an event
///
/// The `ETag` header carries the etag.
#[utoipa::path(
    get, path = "/calendar/v1/events/{id}",
    tag = "events",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the event", body = Event),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, deleted, or not the caller's", body = ErrorBody),
)
)]
async fn get_event(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let ev = s.db.with(|c| load(c, user, id))?.ok_or_else(not_found)?;
    Ok(with_etag(StatusCode::OK, ev))
}

/// Replace an event
///
/// The body is the whole event. `uid`, `recurring_event_id` and `original_start` may be left out;
/// when given they must equal the stored values. With `If-Match`, the write happens only if it equals the current etag.
#[utoipa::path(
    put, path = "/calendar/v1/events/{id}",
    tag = "events",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("If-Match" = Option<String>, Header, description = "The etag the client last saw"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    request_body = EventInput,
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 200, description = "the replaced event; `ETag` header carries the new etag", body = Event),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, deleted, or not the caller's", body = ErrorBody),
    (status = 412, description = "`etag_mismatch`: `If-Match` is not the current etag", body = ErrorBody),
    (status = 422, description = "`validation`: as for create, or an immutable field differs", body = ErrorBody),
)
)]
async fn replace_event(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
    headers: HeaderMap,
    ApiJson(inp): ApiJson<EventInput>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let ck = check(&inp)?;
    let ev = atomically(&s, |c| {
        let old = load(c, user, id)?.ok_or_else(not_found)?;
        check_if_match(&headers, &old.etag)?;
        if inp.uid.as_ref().is_some_and(|u| *u != old.uid)
            || inp
                .recurring_event_id
                .is_some_and(|r| Some(r) != old.recurring_event_id)
            || inp
                .original_start
                .as_ref()
                .is_some_and(|o| Some(o) != old.original_start.as_ref())
        {
            return Err(invalid("uid, recurring_event_id and original_start cannot change").into());
        }
        let is_override = old.recurring_event_id.is_some();
        if is_override && (inp.rrule.is_some() || !inp.exdates.is_empty()) {
            return Err(invalid("an override has no rrule and no exdates").into());
        }
        if is_override && inp.all_day != old.all_day {
            return Err(invalid("an override keeps its series' all_day").into());
        }
        let revision = bump(c, user, old.calendar_id)?;
        c.execute(
            "UPDATE events SET summary = ?2, description = ?3, location = ?4, all_day = ?5, start = ?6, \"end\" = ?7,
                tz = ?8, rrule = ?9, exdates = ?10, reminders = ?11, revision = ?12, start_utc = ?13,
                end_utc = ?14, updated_at = ?15
             WHERE id = ?1 AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?16)",
            params![
                id.to_string(), inp.summary, inp.description, inp.location, inp.all_day, inp.start, inp.end,
                inp.tz, inp.rrule,
                serde_json::to_string(&inp.exdates).unwrap_or_default(),
                serde_json::to_string(&inp.reminders).unwrap_or_default(),
                revision, ck.start_utc, ck.end_utc, Utc::now().timestamp(), user.to_string(),
            ],
        )?;
        Ok(load(c, user, id)?.expect("row just updated"))
    })?;
    tracing::info!(event = "event_replaced", user_id = %user, calendar_id = %ev.calendar_id, event_id = %id);
    Ok(with_etag(StatusCode::OK, ev))
}

/// Delete an event
///
/// Deleting a series deletes its overrides. With `If-Match`, the delete happens only if it equals the current etag.
#[utoipa::path(
    delete, path = "/calendar/v1/events/{id}",
    tag = "events",
    params(
        ("id" = String, Path, description = ID_PARAMS),
        ("If-Match" = Option<String>, Header, description = "The etag the client last saw"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
    (status = 204, description = "deleted"),
    (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, already deleted, or not the caller's", body = ErrorBody),
    (status = 412, description = "`etag_mismatch`: `If-Match` is not the current etag", body = ErrorBody),
)
)]
async fn delete_event(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(id): PathId,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let calendar = atomically(&s, |c| {
        let old = load(c, user, id)?.ok_or_else(not_found)?;
        check_if_match(&headers, &old.etag)?;
        let revision = bump(c, user, old.calendar_id)?;
        c.execute(
            "UPDATE events SET deleted = 1, revision = ?3, updated_at = ?4
             WHERE calendar_id = ?2 AND (id = ?1 OR recurring_event_id = ?1) AND deleted = 0 AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?5)",
            params![
                id.to_string(),
                old.calendar_id.to_string(),
                revision,
                Utc::now().timestamp(),
                user.to_string()
            ],
        )?;
        Ok(old.calendar_id)
    })?;
    tracing::info!(event = "event_deleted", user_id = %user, calendar_id = %calendar, event_id = %id);
    Ok(StatusCode::NO_CONTENT)
}

// ---- range query ----

const MAX_RANGE_DAYS: i64 = 366;
const MAX_OCCURRENCES: usize = 5000;

/// A stored event read back into typed times; `None` when a stored value no longer parses.
struct Parts {
    all_day: bool,
    start: When,
    end: When,
    /// The event's zone; UTC for an all-day event.
    tz: Tz,
}

impl Parts {
    fn of(ev: &Event) -> Option<Parts> {
        let tz = match &ev.tz {
            Some(z) => parse_tz(z).ok()?,
            None => chrono_tz::UTC,
        };
        Some(Parts {
            all_day: ev.all_day,
            start: parse_when(&ev.start, ev.all_day).ok()?,
            end: parse_when(&ev.end, ev.all_day).ok()?,
            tz,
        })
    }

    /// The zone its instants are read in: all-day events follow the query.
    fn zone(&self, query: Tz) -> Tz {
        if self.all_day { query } else { self.tz }
    }
}

fn exdates_of(ev: &Event, all_day: bool) -> Vec<When> {
    ev.exdates
        .iter()
        .filter_map(|x| parse_when(x, all_day).ok())
        .collect()
}

/// Every row the query could need: the calendar's overrides (one moved out of the range still cancels
/// its occurrence), series that began before the range ends, and singles overlapping it. The 14 hours
/// cover all-day rows, whose stored instants assume UTC.
fn candidates(
    c: &Connection,
    user: Uuid,
    calendar: Uuid,
    from: i64,
    to: i64,
) -> rusqlite::Result<Vec<Event>> {
    let pad = 14 * 3600;
    c.prepare(&format!(
        "SELECT {COLUMNS} FROM events WHERE calendar_id = ?1 AND deleted = 0
         AND calendar_id IN (SELECT id FROM calendars WHERE user_id = ?2)
         AND (recurring_event_id IS NOT NULL
              OR (rrule IS NOT NULL AND start_utc < ?3)
              OR (start_utc < ?3 AND end_utc > ?4))"
    ))?
    .query_map(
        params![calendar.to_string(), user.to_string(), to + pad, from - pad],
        from_row,
    )?
    .collect()
}

fn occurrence(
    ev: &Event,
    start: &str,
    end: &str,
    original: Option<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Occurrence {
    let mut event = ev.clone();
    event.start = start.into();
    event.end = end.into();
    event.original_start = original;
    Occurrence {
        event,
        start_utc: from,
        end_utc: to,
    }
}

/// The occurrences of `series` in [from, to) whose start no effective override replaces.
fn expand(
    series: &Event,
    replaced: &HashSet<(Uuid, String)>,
    qtz: Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: usize,
) -> ApiResult<Vec<Occurrence>> {
    let (Some(rrule), Some(p)) = (&series.rrule, Parts::of(series)) else {
        return Ok(vec![]);
    };
    let zone = p.zone(qtz);
    // A timed occurrence is as long as the series' own first one; an all-day one spans whole days.
    let duration = match (p.start, p.end) {
        (When::Date(a), When::Date(b)) => Duration::days((b - a).num_days()),
        _ => p.end.instant(zone) - p.start.instant(zone),
    };
    let starts = recur::starts(
        rrule,
        p.start,
        duration,
        &exdates_of(series, p.all_day),
        zone,
        from,
        to,
        limit,
    )?;
    Ok(starts
        .into_iter()
        .filter(|s| !replaced.contains(&(series.id, s.to_string())))
        .map(|s| {
            let start_utc = s.instant(zone);
            let (end, end_utc) = match s {
                When::Date(d) => {
                    let e = When::Date(d + duration);
                    (e, e.instant(zone))
                }
                When::Timed(_) => {
                    let e = start_utc + duration;
                    (When::Timed(e.with_timezone(&zone).naive_local()), e)
                }
            };
            occurrence(
                series,
                &s.to_string(),
                &end.to_string(),
                Some(s.to_string()),
                start_utc,
                end_utc,
            )
        })
        .collect())
}

/// Turns the candidate rows into the sorted occurrences overlapping [from, to).
fn occurrences_in(
    rows: Vec<Event>,
    qtz: Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> ApiResult<Vec<Occurrence>> {
    let series: HashMap<Uuid, &Event> = rows
        .iter()
        .filter(|e| e.recurring_event_id.is_none() && e.rrule.is_some())
        .map(|e| (e.id, e))
        .collect();
    let mut out = Vec::new();
    let mut replaced = HashSet::new();
    // Singles and overrides; an override that is not effective is dropped and replaces nothing.
    for ev in rows.iter().filter(|e| e.rrule.is_none()) {
        let Some(p) = Parts::of(ev) else { continue };
        if let Some(sid) = ev.recurring_event_id {
            let effective = (|| {
                let s = series.get(&sid)?;
                let sp = Parts::of(s)?;
                let at = parse_when(ev.original_start.as_deref()?, ev.all_day).ok()?;
                (sp.all_day == p.all_day
                    && recur::is_occurrence(
                        s.rrule.as_deref()?,
                        sp.start,
                        &exdates_of(s, sp.all_day),
                        sp.zone(qtz),
                        at,
                    ))
                .then_some(())
            })();
            if effective.is_none() {
                continue;
            }
            replaced.insert((sid, ev.original_start.clone().unwrap_or_default()));
        }
        let zone = p.zone(qtz);
        let (start_utc, end_utc) = (p.start.instant(zone), p.end.instant(zone));
        if start_utc < to && end_utc > from {
            out.push(occurrence(
                ev,
                &ev.start,
                &ev.end,
                ev.original_start.clone(),
                start_utc,
                end_utc,
            ));
        }
    }
    for s in series.values() {
        let more = expand(
            s,
            &replaced,
            qtz,
            from,
            to,
            MAX_OCCURRENCES.saturating_sub(out.len()),
        )?;
        out.extend(more);
    }
    if out.len() > MAX_OCCURRENCES {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_many_occurrences",
            format!("more than {MAX_OCCURRENCES} occurrences in the requested range; narrow it"),
        ));
    }
    out.sort_by_key(|o| (o.start_utc, o.event.id));
    Ok(out)
}

/// `from`, `to` and `tz` of a range query, checked.
fn parse_range(
    q: Result<Query<RangeQuery>, QueryRejection>,
) -> ApiResult<(DateTime<Utc>, DateTime<Utc>, Tz)> {
    let Query(q) = q.map_err(|_| invalid("the query string is malformed"))?;
    let instant = |v: Option<String>, name: &'static str| {
        v.and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
            .map(|t| t.to_utc())
            .ok_or_else(|| invalid(name))
    };
    let (from, to) = (
        instant(
            q.from,
            "from is required, RFC 3339 with an offset (send + as %2B)",
        )?,
        instant(
            q.to,
            "to is required, RFC 3339 with an offset (send + as %2B)",
        )?,
    );
    if to <= from || to - from > Duration::days(MAX_RANGE_DAYS) {
        return Err(invalid("to must be after from, at most 366 days apart"));
    }
    let tz = q.tz.as_deref().map_or(Ok(chrono_tz::UTC), parse_tz)?;
    Ok((from, to, tz))
}

/// List occurrences in a range
///
/// Recurring series are expanded; a cancelled occurrence is left out and a changed one appears as its
/// override. All-day events are read in `tz`. At most 366 days and 5,000 occurrences.
#[utoipa::path(
    get, path = "/calendar/v1/calendars/{id}/events",
    tag = "events",
    params(
        ("id" = String, Path, description = "the calendar's UUID"),
        ("from" = String, Query, description = "RFC 3339 with an offset; `+` must be sent as `%2B`"),
        ("to" = String, Query, description = "RFC 3339 with an offset, after `from`, at most 366 days later"),
        ("tz" = Option<String>, Query, description = "IANA zone for all-day events; default `UTC`"),
        ("X-User-Id" = Option<Uuid>, Header, description = "The user to act for; required with `X-Service-Secret`"),
    ),
    security(("service_secret" = []), ("access_token" = [])),
    responses(
        (status = 200, description = "occurrences overlapping [from, to), by start then id", body = Vec<Occurrence>),
        (status = 401, description = "`unauthorized`: no valid credentials", body = ErrorBody),
        (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's", body = ErrorBody),
        (status = 422, description = "`validation`: bad or missing `from`, `to` or `tz`, or a range over 366 days; `too_many_occurrences`: more than 5,000", body = ErrorBody),
    )
)]
async fn list_occurrences(
    State(s): State<AppState>,
    Caller(user): Caller,
    PathId(calendar): PathId,
    query: Result<Query<RangeQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<Occurrence>>> {
    let (from, to, tz) = parse_range(query)?;
    let rows =
        s.db.with(|c| {
            let Some(_) = calendars::owned(c, user, calendar)? else {
                return Ok(None);
            };
            candidates(c, user, calendar, from.timestamp(), to.timestamp()).map(Some)
        })?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such calendar"))?;
    Ok(Json(occurrences_in(rows, tz, from, to)?))
}
