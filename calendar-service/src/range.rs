use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use common::{ApiError, ApiResult, ErrorBody, PathId};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    AppState, Caller, calendars,
    events::{COLUMNS, Event, from_row, invalid},
    recur,
    time::{When, parse_tz, parse_when},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list_occurrences))
}

// ---- types ----

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

/// Every exdate, or `None` when any no longer parses (the row is then skipped, not half-read).
fn exdates_of(ev: &Event, all_day: bool) -> Option<Vec<When>> {
    ev.exdates
        .iter()
        .map(|x| parse_when(x, all_day).ok())
        .collect()
}

/// Rows the query could need: every series, every override (one moved out of the range still cancels its
/// occurrence, and one moved before its series' start must still find the series), and singles overlapping
/// the range. The 14 hours cover all-day rows, whose stored instants assume UTC.
// ponytail: all series and override rows of the calendar are loaded and parsed per query; filter series by a
// stored last-occurrence instant and overrides by their original_start if one calendar gets thousands.
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
         AND (recurring_event_id IS NOT NULL OR rrule IS NOT NULL
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
    (start, end): (&str, &str),
    original: Option<String>,
    (start_utc, end_utc): (DateTime<Utc>, DateTime<Utc>),
) -> Occurrence {
    let mut event = ev.clone();
    event.start = start.into();
    event.end = end.into();
    event.original_start = original;
    Occurrence {
        event,
        start_utc,
        end_utc,
    }
}

fn too_many() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "too_many_occurrences",
        format!("more than {MAX_OCCURRENCES} occurrences in the requested range; narrow it"),
    )
}

/// A stored row that no longer parses is left out of every answer; only its id is logged.
fn unreadable(ev: &Event) {
    tracing::warn!(event_id = %ev.id, "stored event does not parse; skipped from range queries");
}

/// A series with its times parsed once, and its starts in the range.
struct Series<'a> {
    ev: &'a Event,
    rrule: &'a str,
    p: Parts,
    exdates: Vec<When>,
    zone: Tz,
    /// Starts overlapping the range, exdates removed, before any override is applied.
    starts: Vec<When>,
}

impl<'a> Series<'a> {
    /// `None` when the row, its exdates or its rule do not parse.
    fn expand(
        ev: &'a Event,
        qtz: Tz,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> ApiResult<Option<Self>> {
        let parsed = Parts::of(ev).and_then(|p| Some((exdates_of(ev, p.all_day)?, p)));
        let (Some(rrule), Some((exdates, p))) = (ev.rrule.as_deref(), parsed) else {
            unreadable(ev);
            return Ok(None);
        };
        let zone = p.zone(qtz);
        let mut s = Series {
            ev,
            rrule,
            p,
            exdates,
            zone,
            starts: vec![],
        };
        // A series that begins after the range has nothing in it; its rule is not even built.
        if s.p.start.instant(zone) < to {
            match recur::starts(
                rrule,
                s.p.start,
                s.duration(),
                &s.exdates,
                zone,
                from,
                to,
                MAX_OCCURRENCES,
            ) {
                Ok(v) => s.starts = v,
                Err(e) if e.code == "too_many_occurrences" => return Err(e),
                Err(_) => {
                    unreadable(ev);
                    return Ok(None);
                }
            }
        }
        Ok(Some(s))
    }

    /// A timed occurrence is as long as the series' own first one; an all-day one spans whole days.
    fn duration(&self) -> Duration {
        match (self.p.start, self.p.end) {
            (When::Date(a), When::Date(b)) => Duration::days((b - a).num_days()),
            _ => self.p.end.instant(self.zone) - self.p.start.instant(self.zone),
        }
    }

    fn occurrence(&self, s: When) -> Occurrence {
        let duration = self.duration();
        let start_utc = s.instant(self.zone);
        let (end, end_utc) = match s {
            When::Date(d) => {
                let e = When::Date(d + duration);
                (e, e.instant(self.zone))
            }
            When::Timed(_) => {
                let e = start_utc + duration;
                (When::Timed(e.with_timezone(&self.zone).naive_local()), e)
            }
        };
        occurrence(
            self.ev,
            (&s.to_string(), &end.to_string()),
            Some(s.to_string()),
            (start_utc, end_utc),
        )
    }
}

/// Whether the override `ev` (read as `p`) replaces an occurrence of `series`, and so is shown or hides one.
/// Membership in the expanded starts settles it for an occurrence in the range; any other needs the rule.
fn is_effective(ev: &Event, p: &Parts, series: &Series, overlaps: bool) -> Option<When> {
    let at = parse_when(ev.original_start.as_deref()?, ev.all_day).ok()?;
    if series.p.all_day != p.all_day {
        return None;
    }
    let found = series.starts.contains(&at)
        || (overlaps
            && recur::is_occurrence(
                series.rrule,
                series.p.start,
                &series.exdates,
                series.zone,
                at,
            ));
    found.then_some(at)
}

/// Turns the candidate rows into the sorted occurrences overlapping [from, to).
fn occurrences_in(
    rows: Vec<Event>,
    qtz: Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> ApiResult<Vec<Occurrence>> {
    let mut series = HashMap::new();
    for ev in rows
        .iter()
        .filter(|e| e.recurring_event_id.is_none() && e.rrule.is_some())
    {
        if let Some(s) = Series::expand(ev, qtz, from, to)? {
            series.insert(ev.id, s);
        }
    }
    let mut out = Vec::new();
    let mut replaced = HashSet::new();
    // Singles and overrides; an override that is not effective is dropped and replaces nothing.
    for ev in rows.iter().filter(|e| e.rrule.is_none()) {
        let Some(p) = Parts::of(ev) else {
            unreadable(ev);
            continue;
        };
        let zone = p.zone(qtz);
        let (start_utc, end_utc) = (p.start.instant(zone), p.end.instant(zone));
        let overlaps = start_utc < to && end_utc > from;
        if let Some(sid) = ev.recurring_event_id {
            let Some(at) = series
                .get(&sid)
                .and_then(|s| is_effective(ev, &p, s, overlaps))
            else {
                continue;
            };
            replaced.insert((sid, at));
        }
        if overlaps {
            out.push(occurrence(
                ev,
                (&ev.start, &ev.end),
                ev.original_start.clone(),
                (start_utc, end_utc),
            ));
        }
    }
    for s in series.values() {
        for &st in s
            .starts
            .iter()
            .filter(|st| !replaced.contains(&(s.ev.id, **st)))
        {
            out.push(s.occurrence(st));
        }
        if out.len() > MAX_OCCURRENCES {
            return Err(too_many());
        }
    }
    if out.len() > MAX_OCCURRENCES {
        return Err(too_many());
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
