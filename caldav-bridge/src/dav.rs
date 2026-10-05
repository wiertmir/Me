use std::collections::{BTreeMap, HashMap};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{
    AppState, DavError,
    auth::Signed,
    backend::{Backend, Collection, Event, EventWrite, Task},
    ical,
    path::{self, Kind, Target},
    xml::{self, Prop, ReportReq},
};

const ALL: [Prop; 12] = [
    Prop::ResourceType,
    Prop::DisplayName,
    Prop::CurrentUserPrincipal,
    Prop::CalendarHomeSet,
    Prop::SupportedComponents,
    Prop::CTag,
    Prop::Color,
    Prop::SupportedReports,
    Prop::Privileges,
    Prop::Owner,
    Prop::ETag,
    Prop::ContentType,
];

const CALENDAR_TYPE: &str = "text/calendar; charset=utf-8";

pub async fn handle(
    State(state): State<AppState>,
    signed: Signed,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, DavError> {
    if method == Method::OPTIONS {
        return Ok((
            [
                ("dav", "1, calendar-access"),
                ("allow", "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE"),
            ],
            StatusCode::OK,
        )
            .into_response());
    }
    let b = state.backend(signed.user_id);
    let user = &signed.username;
    match (method.as_str(), path::parse(uri.path(), user)) {
        ("PROPFIND" | "REPORT" | "GET" | "PUT" | "DELETE", None) => Err(not_found()),
        ("PROPFIND", Some(target)) => propfind(&b, user, target, &headers, &body).await,
        ("REPORT", Some(Target::Collection(kind, id))) => report(&b, user, kind, id, &body).await,
        ("GET", Some(Target::Item(Kind::Todos, list, uid))) => get_todo(&b, list, &uid).await,
        ("PUT", Some(Target::Item(Kind::Todos, list, uid))) => {
            let answer = put_todo(&b, list, &uid, &headers, &body).await?;
            tracing::info!(event = "todo_written", user_id = %signed.user_id, list_id = %list);
            Ok(answer)
        }
        ("DELETE", Some(Target::Item(Kind::Todos, list, uid))) => {
            let old = b.task_by_uid(list, &uid).await?.ok_or_else(not_found)?;
            let guard = if_match(&headers, Some(&old.etag))?;
            b.delete_task(old.id, guard).await?;
            tracing::info!(event = "todo_deleted", user_id = %signed.user_id, list_id = %list);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        ("GET", Some(Target::Item(Kind::Events, cal, uid))) => get_event(&b, cal, &uid).await,
        ("PUT", Some(Target::Item(Kind::Events, cal, uid))) => {
            let answer = put_event(&b, cal, &uid, &headers, &body).await?;
            tracing::info!(event = "event_written", user_id = %signed.user_id, calendar_id = %cal);
            Ok(answer)
        }
        ("DELETE", Some(Target::Item(Kind::Events, cal, uid))) => {
            let parts = stored_event(&b, cal, &uid, &headers).await?;
            // The service deletes a series' overrides with it.
            b.delete_event(parts.first().ok_or_else(not_found)?.id)
                .await?;
            tracing::info!(event = "event_deleted", user_id = %signed.user_id, calendar_id = %cal);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        _ => Err(DavError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not supported",
        )),
    }
}

fn not_found() -> DavError {
    DavError::new(StatusCode::NOT_FOUND, "")
}

#[allow(clippy::result_large_err)] // the error type is the crate's own, as everywhere else
fn utf8(body: &[u8]) -> Result<&str, DavError> {
    std::str::from_utf8(body)
        .map_err(|_| DavError::new(StatusCode::BAD_REQUEST, "body is not UTF-8"))
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

/// 412 unless `If-Match` is absent, or names the stored item's etag (`*`: any stored item). With the header,
/// the answer is the etag to hold the service to, so that a change in between is refused there.
#[allow(clippy::result_large_err)] // as above
fn if_match<'a>(headers: &HeaderMap, stored: Option<&'a str>) -> Result<Option<&'a str>, DavError> {
    let Some(wanted) = header_text(headers, header::IF_MATCH) else {
        return Ok(None);
    };
    match stored {
        Some(etag) if wanted == "*" || wanted == etag => Ok(Some(etag)),
        _ => Err(DavError::new(StatusCode::PRECONDITION_FAILED, "")),
    }
}

/// 403 unless the item is named after the uid in its body: that is how an item is found again.
#[allow(clippy::result_large_err)] // as above
fn named_by_uid(name: &str, uid: &str) -> Result<(), DavError> {
    if name == uid {
        return Ok(());
    }
    Err(
        DavError::new(StatusCode::FORBIDDEN, "the name must be the UID and .ics")
            .precondition("valid-calendar-object-resource"),
    )
}

async fn get_todo(b: &Backend<'_>, list: Uuid, uid: &str) -> Result<Response, DavError> {
    let t = b.task_by_uid(list, uid).await?.ok_or_else(not_found)?;
    let parent = match t.parent_id {
        Some(id) => Some(b.task(id).await?.uid),
        None => None,
    };
    let headers = [
        (header::CONTENT_TYPE, CALENDAR_TYPE),
        (header::ETAG, &t.etag),
    ];
    Ok((headers, ical::todo_to_ical(&t, parent.as_deref())).into_response())
}

/// A stored to-do keeps its repeat rule and its parent, whatever the body says.
async fn put_todo(
    b: &Backend<'_>,
    list: Uuid,
    uid: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, DavError> {
    let todo = ical::ical_to_todo(utf8(body)?)?;
    named_by_uid(uid, &todo.uid)?;
    let stored = b.task_by_uid(list, uid).await?;
    let guard = if_match(headers, stored.as_ref().map(|t| t.etag.as_str()))?;
    if stored.is_some() && header_text(headers, header::IF_NONE_MATCH) == Some("*") {
        return Err(DavError::new(StatusCode::PRECONDITION_FAILED, ""));
    }
    let mut write = todo.write;
    let status = match &stored {
        Some(old) => {
            // Without a due there is nothing to repeat from: the series ends here.
            write.rrule = old.rrule.clone().filter(|_| write.due.is_some());
            b.replace_task(old.id, &write, guard).await?;
            StatusCode::NO_CONTENT
        }
        None => {
            if let Some(parent) = &todo.related_to {
                write.parent_id = b.task_by_uid(list, parent).await?.map(|p| p.id);
            }
            match b.create_task(list, &write).await {
                // The service refuses a parent that is a subtask itself, and a rule on a subtask.
                Err(e) if write.parent_id.is_some() && e.status() == StatusCode::FORBIDDEN => {
                    write.parent_id = None;
                    b.create_task(list, &write).await?
                }
                other => other?,
            };
            StatusCode::CREATED
        }
    };
    // No `ETag`: what is stored is not the body as sent, so the client reads the item back.
    Ok(status.into_response())
}

/// What is stored under a uid, the single event or the series first; empty when there is nothing. 412
/// unless that satisfies `If-Match`.
// ponytail: `If-Match` is compared here, against what was just looked up, and the service calls that follow
// are sent without it, so two writers in the same instant can both pass; pass the series' etag through to
// the service if that ever matters.
async fn stored_event(
    b: &Backend<'_>,
    cal: Uuid,
    uid: &str,
    headers: &HeaderMap,
) -> Result<Vec<Event>, DavError> {
    let parts = b.events_by_uid(cal, uid).await?;
    let etag = (!parts.is_empty()).then(|| ical::item_etag(&parts));
    if_match(headers, etag.as_deref())?;
    Ok(parts)
}

async fn get_event(b: &Backend<'_>, cal: Uuid, uid: &str) -> Result<Response, DavError> {
    let parts = b.events_by_uid(cal, uid).await?;
    if parts.is_empty() {
        return Err(not_found());
    }
    let headers = [
        (header::CONTENT_TYPE, CALENDAR_TYPE.to_owned()),
        (header::ETAG, ical::item_etag(&parts)),
    ];
    Ok((headers, ical::events_to_ical(&parts)).into_response())
}

/// Writes an item part by part: the single event or the series, then each changed occurrence, then the
/// stored ones the body no longer has. A part the body does not change is not written, so it keeps its
/// revision. Not atomic: the first call that fails ends the write with its error and what was written stays.
async fn put_event(
    b: &Backend<'_>,
    cal: Uuid,
    uid: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, DavError> {
    let item = ical::ical_to_events(utf8(body)?)?;
    named_by_uid(uid, &item.uid)?;
    let stored = stored_event(b, cal, uid, headers).await?;
    if !stored.is_empty() && header_text(headers, header::IF_NONE_MATCH) == Some("*") {
        return Err(DavError::new(StatusCode::PRECONDITION_FAILED, ""));
    }
    let (series, status) = match stored.first() {
        Some(old) => {
            if EventWrite::from(old) != item.main {
                b.replace_event(old.id, &item.main).await?;
            }
            (old.id, StatusCode::NO_CONTENT)
        }
        None => (
            b.create_event(cal, &item.main).await?.id,
            StatusCode::CREATED,
        ),
    };
    let old_overrides = stored.get(1..).unwrap_or_default();
    for new in item.overrides.iter().cloned() {
        // The service takes an override's occurrence only together with its series.
        let new = EventWrite {
            recurring_event_id: Some(series),
            ..new
        };
        let old = old_overrides
            .iter()
            .find(|old| old.original_start == new.original_start);
        match old {
            Some(old) if EventWrite::from(old) == new => {}
            Some(old) => {
                b.replace_event(old.id, &new).await?;
            }
            None => {
                b.create_event(cal, &new).await?;
            }
        }
    }
    for old in old_overrides {
        let kept = |new: &EventWrite| new.original_start == old.original_start;
        if !item.overrides.iter().any(kept) {
            b.delete_event(old.id).await?;
        }
    }
    // No `ETag`: what is stored is not the body as sent, so the client reads the item back.
    Ok(status.into_response())
}

/// The stored events of a calendar as items: under each uid the single event or the series, then its
/// overrides.
fn items(events: Vec<Event>) -> Vec<Vec<Event>> {
    let mut by_uid: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for e in events {
        by_uid.entry(e.uid.clone()).or_default().push(e);
    }
    let mut items: Vec<_> = by_uid.into_values().collect();
    for parts in &mut items {
        // Only an override has an `original_start`.
        parts.sort_by(|a, b| a.original_start.cmp(&b.original_start));
    }
    items
}

/// One `response` of a multistatus: a root, the principal, the home, a collection or an item.
struct Entry {
    target: Target,
    collection: Option<Collection>,
    item: Option<Item>,
}

struct Item {
    etag: String,
    /// The iCalendar text, in reports only.
    data: Option<String>,
}

impl Entry {
    fn todo(list: Uuid, t: &Task, data: Option<String>) -> Self {
        let etag = t.etag.clone();
        Entry {
            target: Target::Item(Kind::Todos, list, t.uid.clone()),
            collection: None,
            item: Some(Item { etag, data }),
        }
    }

    /// `parts` as `items` gives them, so not empty.
    fn event(cal: Uuid, parts: &[Event], data: Option<String>) -> Self {
        let etag = ical::item_etag(parts);
        Entry {
            target: Target::Item(Kind::Events, cal, parts[0].uid.clone()),
            collection: None,
            item: Some(Item { etag, data }),
        }
    }
}

fn multistatus(entries: &[Entry], asked: Option<&[Prop]>, username: &str) -> Response {
    let responses: Vec<_> = entries.iter().map(|e| answer(e, asked, username)).collect();
    multistatus_of(&responses)
}

fn multistatus_of(responses: &[xml::Response]) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml::multistatus(responses),
    )
        .into_response()
}

fn answer(e: &Entry, asked: Option<&[Prop]>, username: &str) -> xml::Response {
    let values = |props: &[Prop]| -> (Vec<_>, Vec<_>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for p in props {
            match value(e, username, p) {
                Some(v) => found.push((p.clone(), v)),
                None => missing.push(p.clone()),
            }
        }
        (found, missing)
    };
    let (found, missing) = match asked {
        Some(props) => values(props),
        // "All" is only what exists for this target.
        None => (values(&ALL).0, Vec::new()),
    };
    xml::Response {
        href: path::href(&e.target, username),
        found,
        missing,
        not_found: false,
    }
}

async fn propfind(
    b: &Backend<'_>,
    username: &str,
    target: Target,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, DavError> {
    let depth = match header_text(headers, header::HeaderName::from_static("depth")) {
        None | Some("0") => 0,
        Some("1") => 1,
        Some("infinity") => {
            return Err(DavError::new(
                StatusCode::FORBIDDEN,
                "depth infinity is not supported",
            ));
        }
        Some(_) => return Err(DavError::new(StatusCode::BAD_REQUEST, "bad Depth")),
    };
    let asked = xml::read_propfind(utf8(body)?)?.props;

    let plain = |target| Entry {
        target,
        collection: None,
        item: None,
    };
    let mut entries = Vec::new();
    match target {
        Target::Root | Target::Principal => entries.push(plain(target)),
        Target::Home => {
            entries.push(plain(target));
            if depth == 1 {
                for c in b.collections().await? {
                    entries.push(Entry {
                        target: Target::Collection(c.kind, c.id),
                        collection: Some(c),
                        item: None,
                    });
                }
            }
        }
        Target::Collection(kind, id) => {
            let mut c = b
                .collections()
                .await?
                .into_iter()
                .find(|c| c.kind == kind && c.id == id)
                .ok_or_else(not_found)?;
            let mut listed = Vec::new();
            // The change marker read with the items is the one they belong to.
            match kind {
                _ if depth == 0 => {}
                Kind::Todos => {
                    let (token, tasks) = b.tasks(id).await?;
                    c.sync_token = token;
                    listed.extend(tasks.iter().map(|t| Entry::todo(id, t, None)));
                }
                Kind::Events => {
                    let (token, events) = b.events(id).await?;
                    c.sync_token = token;
                    let events = items(events);
                    listed.extend(events.iter().map(|parts| Entry::event(id, parts, None)));
                }
            }
            entries.push(Entry {
                collection: Some(c),
                ..plain(target)
            });
            entries.append(&mut listed);
        }
        Target::Item(Kind::Todos, list, ref uid) => {
            let t = b.task_by_uid(list, uid).await?.ok_or_else(not_found)?;
            entries.push(Entry::todo(list, &t, None));
        }
        Target::Item(Kind::Events, cal, ref uid) => {
            let parts = b.events_by_uid(cal, uid).await?;
            if parts.is_empty() {
                return Err(not_found());
            }
            entries.push(Entry::event(cal, &parts, None));
        }
    }
    Ok(multistatus(&entries, asked.as_deref(), username))
}

/// Both reports of a collection from one listing of it; a query's filter is not read.
async fn report(
    b: &Backend<'_>,
    username: &str,
    kind: Kind,
    id: Uuid,
    body: &[u8],
) -> Result<Response, DavError> {
    let req = xml::read_report(utf8(body)?)?;
    let entries: Vec<Entry> = match kind {
        Kind::Todos => {
            let (_, tasks) = b.tasks(id).await?;
            let uids: HashMap<Uuid, &str> = tasks.iter().map(|t| (t.id, t.uid.as_str())).collect();
            let entry = |t: &Task| {
                let parent = t.parent_id.and_then(|p| uids.get(&p).copied());
                Entry::todo(id, t, Some(ical::todo_to_ical(t, parent)))
            };
            tasks.iter().map(entry).collect()
        }
        Kind::Events => {
            let (_, events) = b.events(id).await?;
            let entry =
                |parts: &Vec<Event>| Entry::event(id, parts, Some(ical::events_to_ical(parts)));
            items(events).iter().map(entry).collect()
        }
    };
    match req {
        ReportReq::Query { props } => Ok(multistatus(&entries, Some(&props), username)),
        ReportReq::Multiget { props, hrefs } => {
            let by_uid: HashMap<&str, &Entry> = entries
                .iter()
                .filter_map(|e| match &e.target {
                    Target::Item(_, _, uid) => Some((uid.as_str(), e)),
                    _ => None,
                })
                .collect();
            let responses: Vec<_> = hrefs
                .into_iter()
                .map(|href| {
                    // An href may be a whole URL.
                    let path = match href.split_once("://") {
                        Some((_, rest)) if !href.starts_with('/') => {
                            rest.find('/').map_or("", |i| &rest[i..])
                        }
                        _ => &href,
                    };
                    let entry = match path::parse(path, username) {
                        Some(Target::Item(k, c, uid)) if k == kind && c == id => {
                            by_uid.get(uid.as_str())
                        }
                        _ => None,
                    };
                    match entry {
                        Some(e) => answer(e, Some(&props), username),
                        None => xml::Response {
                            href,
                            found: Vec::new(),
                            missing: Vec::new(),
                            not_found: true,
                        },
                    }
                })
                .collect();
            Ok(multistatus_of(&responses))
        }
    }
}

/// The inner XML of `prop` for an entry, or `None` when it has no such property.
fn value(e: &Entry, username: &str, prop: &Prop) -> Option<String> {
    let href = |t: Target| {
        format!(
            "<D:href>{}</D:href>",
            xml::escape(&path::href(&t, username))
        )
    };
    let principal = href(Target::Principal);
    Some(match (prop, &e.target, &e.collection, &e.item) {
        (Prop::ResourceType, Target::Principal, _, _) => "<D:collection/><D:principal/>".into(),
        (Prop::ResourceType, Target::Collection(..), _, _) => "<D:collection/><C:calendar/>".into(),
        (Prop::ResourceType, Target::Item(..), _, _) => String::new(),
        (Prop::ResourceType, _, _, _) => "<D:collection/>".into(),
        (Prop::DisplayName, _, Some(c), _) => xml::escape(&c.name),
        (Prop::DisplayName, _, None, None) => xml::escape(username),
        (Prop::CurrentUserPrincipal, _, _, _) => principal,
        (Prop::CalendarHomeSet, Target::Principal, _, _) => href(Target::Home),
        (Prop::SupportedComponents, _, Some(c), _) => match c.kind {
            Kind::Events => "<C:comp name=\"VEVENT\"/>".into(),
            Kind::Todos => "<C:comp name=\"VTODO\"/>".into(),
        },
        (Prop::CTag, _, Some(c), _) => c.sync_token.to_string(),
        (Prop::Color, _, Some(c), _) => xml::escape(&c.color),
        (Prop::SupportedReports, _, Some(_), _) => ["calendar-multiget", "calendar-query"]
            .map(|r| {
                format!("<D:supported-report><D:report><C:{r}/></D:report></D:supported-report>")
            })
            .concat(),
        (Prop::Privileges, _, Some(_), _) => ["read", "write", "write-content", "bind", "unbind"]
            .map(|p| format!("<D:privilege><D:{p}/></D:privilege>"))
            .concat(),
        (Prop::Owner, _, Some(_), _) => principal,
        (Prop::ETag, _, _, Some(item)) => xml::escape(&item.etag),
        (Prop::ContentType, _, _, Some(_)) => CALENDAR_TYPE.into(),
        (Prop::CalendarData, _, _, Some(item)) => xml::escape(item.data.as_ref()?),
        _ => return None,
    })
}
