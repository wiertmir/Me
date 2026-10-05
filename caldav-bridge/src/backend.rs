use axum::http::{Method, StatusCode, header};
use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{RequestBuilder, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{AppState, DavError, path::Kind};

#[derive(Clone, Copy)]
pub enum Service {
    Calendar,
    Tasks,
}

/// calendar-service and tasks-service, spoken to as one user with the service secret.
pub struct Backend<'a> {
    state: &'a AppState,
    user_id: Uuid,
}

impl AppState {
    pub fn backend(&self, user: Uuid) -> Backend<'_> {
        Backend {
            state: self,
            user_id: user,
        }
    }
}

impl Backend<'_> {
    /// `path` starts with `/calendar/v1` or `/tasks/v1`.
    pub fn request(&self, service: Service, method: Method, path: &str) -> RequestBuilder {
        let cfg = &self.state.cfg;
        let (url, secret) = match service {
            Service::Calendar => (&cfg.calendar_url, &cfg.calendar_secret),
            Service::Tasks => (&cfg.tasks_url, &cfg.tasks_secret),
        };
        self.state
            .http
            .request(method, format!("{url}{path}"))
            .header("x-service-secret", secret)
            .header("x-user-id", self.user_id.to_string())
    }

    /// Sends the request; a success is returned as it is, anything else becomes the error the client gets.
    pub async fn send(&self, req: RequestBuilder) -> Result<Response, DavError> {
        let resp = req.send().await.map_err(|e| {
            // The address can carry an item's uid, which is never logged.
            tracing::warn!(error = %e.without_url(), "backend unreachable");
            DavError::new(StatusCode::BAD_GATEWAY, "backend unreachable")
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        Err(match status {
            StatusCode::NOT_FOUND | StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                DavError::new(status, "")
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let message = body["message"].as_str().unwrap_or("invalid calendar data");
                DavError::new(StatusCode::FORBIDDEN, message).precondition("valid-calendar-data")
            }
            _ => {
                tracing::warn!(%status, "backend answered unexpectedly");
                DavError::new(StatusCode::BAD_GATEWAY, "backend failed")
            }
        })
    }

    /// Sends the request and reads the JSON of its answer.
    async fn json<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, DavError> {
        self.send(req).await?.json().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), "backend answered with unexpected data");
            DavError::new(StatusCode::BAD_GATEWAY, "backend failed")
        })
    }
}

/// A calendar or a task list.
pub struct Collection {
    pub kind: Kind,
    pub id: Uuid,
    pub name: String,
    pub color: String,
    pub sync_token: i64,
}

#[derive(Deserialize)]
struct Listed {
    id: Uuid,
    name: String,
    color: String,
    sync_token: i64,
}

impl Backend<'_> {
    /// The user's calendars, or their task lists: only that kind's service is asked.
    pub async fn collections(&self, kind: Kind) -> Result<Vec<Collection>, DavError> {
        let (service, path) = match kind {
            Kind::Events => (Service::Calendar, "/calendar/v1/calendars"),
            Kind::Todos => (Service::Tasks, "/tasks/v1/lists"),
        };
        let listed: Vec<Listed> = self.json(self.request(service, Method::GET, path)).await?;
        let collection = |l: Listed| Collection {
            kind,
            id: l.id,
            name: l.name,
            color: l.color,
            sync_token: l.sync_token,
        };
        Ok(listed.into_iter().map(collection).collect())
    }
}

/// A task as tasks-service answers it.
#[derive(Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub due: Option<String>,
    pub tz: Option<String>,
    pub priority: u8,
    pub completed: bool,
    pub completed_at: Option<DateTime<Utc>>,
    pub reminders: Vec<u32>,
    pub parent_id: Option<Uuid>,
    pub rrule: Option<String>,
    pub etag: String,
    pub updated_at: DateTime<Utc>,
}

/// What tasks-service takes for a task. On a replace, `uid` and `parent_id` left out stay as stored.
#[derive(Serialize, Default)]
pub struct TaskWrite {
    pub uid: Option<String>,
    pub summary: String,
    pub description: String,
    pub due: Option<String>,
    pub tz: Option<String>,
    pub priority: u8,
    pub completed: bool,
    pub reminders: Vec<u32>,
    pub parent_id: Option<Uuid>,
    pub rrule: Option<String>,
}

#[derive(Deserialize)]
struct Changes {
    sync_token: i64,
    tasks: Vec<Task>,
}

impl Backend<'_> {
    fn tasks_request(&self, method: Method, path: &str) -> RequestBuilder {
        self.request(Service::Tasks, method, &format!("/tasks/v1{path}"))
    }

    /// The list's sync token and every task in it.
    pub async fn tasks(&self, list: Uuid) -> Result<(i64, Vec<Task>), DavError> {
        let path = format!("/lists/{list}/changes");
        let c: Changes = self.json(self.tasks_request(Method::GET, &path)).await?;
        Ok((c.sync_token, c.tasks))
    }

    pub async fn task_by_uid(&self, list: Uuid, uid: &str) -> Result<Option<Task>, DavError> {
        let uid = utf8_percent_encode(uid, NON_ALPHANUMERIC);
        let path = format!("/lists/{list}/by-uid?uid={uid}");
        let found: Vec<Task> = self.json(self.tasks_request(Method::GET, &path)).await?;
        Ok(found.into_iter().next())
    }

    pub async fn task(&self, id: Uuid) -> Result<Task, DavError> {
        let path = format!("/tasks/{id}");
        self.json(self.tasks_request(Method::GET, &path)).await
    }

    pub async fn create_task(&self, list: Uuid, t: &TaskWrite) -> Result<Task, DavError> {
        let path = format!("/lists/{list}/tasks");
        self.json(self.tasks_request(Method::POST, &path).json(t))
            .await
    }

    /// With `if_match`, the service refuses (412) when the task's etag is no longer that one.
    pub async fn replace_task(
        &self,
        id: Uuid,
        t: &TaskWrite,
        if_match: Option<&str>,
    ) -> Result<Task, DavError> {
        let req = self.tasks_request(Method::PUT, &format!("/tasks/{id}"));
        self.json(guarded(req, if_match).json(t)).await
    }

    pub async fn delete_task(&self, id: Uuid, if_match: Option<&str>) -> Result<(), DavError> {
        let req = self.tasks_request(Method::DELETE, &format!("/tasks/{id}"));
        self.send(guarded(req, if_match)).await.map(|_| ())
    }
}

fn guarded(req: RequestBuilder, if_match: Option<&str>) -> RequestBuilder {
    match if_match {
        Some(etag) => req.header(header::IF_MATCH, etag),
        None => req,
    }
}

/// A stored event as calendar-service answers it: a single event, a series, or an override of one
/// occurrence of a series.
#[derive(Deserialize, Clone)]
pub struct Event {
    pub id: Uuid,
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub all_day: bool,
    pub start: String,
    pub end: String,
    pub tz: Option<String>,
    pub rrule: Option<String>,
    pub exdates: Vec<String>,
    pub reminders: Vec<u32>,
    pub recurring_event_id: Option<Uuid>,
    pub original_start: Option<String>,
    pub etag: String,
    pub updated_at: DateTime<Utc>,
}

/// What calendar-service takes for an event. On a replace, `uid` left out stays as stored.
#[derive(Serialize, Default, Clone, PartialEq)]
pub struct EventWrite {
    pub uid: Option<String>,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub all_day: bool,
    pub start: String,
    pub end: String,
    pub tz: Option<String>,
    pub rrule: Option<String>,
    pub exdates: Vec<String>,
    pub reminders: Vec<u32>,
    pub recurring_event_id: Option<Uuid>,
    pub original_start: Option<String>,
}

/// A stored event as it would be sent to write it again, to tell whether a body changes it. An override
/// is without its uid, which is its series'.
impl From<&Event> for EventWrite {
    fn from(e: &Event) -> Self {
        let e = e.clone();
        EventWrite {
            uid: e.recurring_event_id.is_none().then_some(e.uid),
            summary: e.summary,
            description: e.description,
            location: e.location,
            all_day: e.all_day,
            start: e.start,
            end: e.end,
            tz: e.tz,
            rrule: e.rrule,
            exdates: e.exdates,
            reminders: e.reminders,
            recurring_event_id: e.recurring_event_id,
            original_start: e.original_start,
        }
    }
}

#[derive(Deserialize)]
struct EventChanges {
    sync_token: i64,
    events: Vec<Event>,
}

impl Backend<'_> {
    fn calendar_request(&self, method: Method, path: &str) -> RequestBuilder {
        self.request(Service::Calendar, method, &format!("/calendar/v1{path}"))
    }

    /// The calendar's sync token and every stored event in it, series and overrides as stored.
    pub async fn events(&self, cal: Uuid) -> Result<(i64, Vec<Event>), DavError> {
        let path = format!("/calendars/{cal}/changes");
        let c: EventChanges = self.json(self.calendar_request(Method::GET, &path)).await?;
        Ok((c.sync_token, c.events))
    }

    /// What is stored under a uid: the single event or the series first, then its overrides.
    pub async fn events_by_uid(&self, cal: Uuid, uid: &str) -> Result<Vec<Event>, DavError> {
        let uid = utf8_percent_encode(uid, NON_ALPHANUMERIC);
        let path = format!("/calendars/{cal}/by-uid?uid={uid}");
        self.json(self.calendar_request(Method::GET, &path)).await
    }

    pub async fn create_event(&self, cal: Uuid, e: &EventWrite) -> Result<Event, DavError> {
        let path = format!("/calendars/{cal}/events");
        self.json(self.calendar_request(Method::POST, &path).json(e))
            .await
    }

    pub async fn replace_event(&self, id: Uuid, e: &EventWrite) -> Result<Event, DavError> {
        let path = format!("/events/{id}");
        self.json(self.calendar_request(Method::PUT, &path).json(e))
            .await
    }

    /// Deleting a series deletes its overrides.
    pub async fn delete_event(&self, id: Uuid) -> Result<(), DavError> {
        let path = format!("/events/{id}");
        let req = self.calendar_request(Method::DELETE, &path);
        self.send(req).await.map(|_| ())
    }
}
