use std::collections::HashMap;

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
    backend::{Backend, Collection, Task},
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
        ("REPORT", Some(Target::Collection(Kind::Todos, list))) => {
            report(&b, user, list, &body).await
        }
        ("GET", Some(Target::Item(Kind::Todos, list, uid))) => get_todo(&b, list, &uid).await,
        ("PUT", Some(Target::Item(Kind::Todos, list, uid))) => {
            let answer = put_todo(&b, list, &uid, &headers, &body).await?;
            tracing::info!(event = "todo_written", user_id = %signed.user_id, list_id = %list);
            Ok(answer)
        }
        ("DELETE", Some(Target::Item(Kind::Todos, list, uid))) => {
            let old = b.task_by_uid(list, &uid).await?.ok_or_else(not_found)?;
            let guard = if_match(&headers, Some(&old))?;
            b.delete_task(old.id, guard).await?;
            tracing::info!(event = "todo_deleted", user_id = %signed.user_id, list_id = %list);
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
fn if_match<'a>(
    headers: &HeaderMap,
    stored: Option<&'a Task>,
) -> Result<Option<&'a str>, DavError> {
    let Some(wanted) = header_text(headers, header::IF_MATCH) else {
        return Ok(None);
    };
    match stored {
        Some(t) if wanted == "*" || wanted == t.etag => Ok(Some(&t.etag)),
        _ => Err(DavError::new(StatusCode::PRECONDITION_FAILED, "")),
    }
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
    if todo.uid != uid {
        return Err(
            DavError::new(StatusCode::FORBIDDEN, "the name must be the UID and .ics")
                .precondition("valid-calendar-object-resource"),
        );
    }
    let stored = b.task_by_uid(list, uid).await?;
    let guard = if_match(headers, stored.as_ref())?;
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
            let mut items = Vec::new();
            if depth == 1 && kind == Kind::Todos {
                // The change marker read with the items is the one they belong to.
                let (token, tasks) = b.tasks(id).await?;
                c.sync_token = token;
                items.extend(tasks.iter().map(|t| Entry::todo(id, t, None)));
            }
            entries.push(Entry {
                collection: Some(c),
                ..plain(target)
            });
            entries.append(&mut items);
        }
        Target::Item(Kind::Todos, list, ref uid) => {
            let t = b.task_by_uid(list, uid).await?.ok_or_else(not_found)?;
            entries.push(Entry::todo(list, &t, None));
        }
        Target::Item(Kind::Events, ..) => return Err(not_found()),
    }
    Ok(multistatus(&entries, asked.as_deref(), username))
}

/// Both reports of a task list from one listing of it; a query's filter is not read.
async fn report(
    b: &Backend<'_>,
    username: &str,
    list: Uuid,
    body: &[u8],
) -> Result<Response, DavError> {
    let req = xml::read_report(utf8(body)?)?;
    let (_, tasks) = b.tasks(list).await?;
    let uids: HashMap<Uuid, &str> = tasks.iter().map(|t| (t.id, t.uid.as_str())).collect();
    let entry = |t: &Task| {
        let parent = t.parent_id.and_then(|p| uids.get(&p).copied());
        Entry::todo(list, t, Some(ical::todo_to_ical(t, parent)))
    };
    match req {
        ReportReq::Query { props } => {
            let entries: Vec<_> = tasks.iter().map(entry).collect();
            Ok(multistatus(&entries, Some(&props), username))
        }
        ReportReq::Multiget { props, hrefs } => {
            let by_uid: HashMap<&str, &Task> = tasks.iter().map(|t| (t.uid.as_str(), t)).collect();
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
                    let task = match path::parse(path, username) {
                        Some(Target::Item(Kind::Todos, l, uid)) if l == list => {
                            by_uid.get(uid.as_str())
                        }
                        _ => None,
                    };
                    match task {
                        Some(t) => answer(&entry(t), Some(&props), username),
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
