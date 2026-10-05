use axum::http::{Method, StatusCode};
use reqwest::{RequestBuilder, Response};
use serde::Deserialize;
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
            tracing::warn!(error = %e, "backend unreachable");
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
    /// Every calendar, then every task list, of the user.
    pub async fn collections(&self) -> Result<Vec<Collection>, DavError> {
        let mut all = Vec::new();
        for (service, path, kind) in [
            (Service::Calendar, "/calendar/v1/calendars", Kind::Events),
            (Service::Tasks, "/tasks/v1/lists", Kind::Todos),
        ] {
            let resp = self.send(self.request(service, Method::GET, path)).await?;
            let listed: Vec<Listed> = resp.json().await.map_err(|e| {
                tracing::warn!(error = %e, "backend answered with unexpected data");
                DavError::new(StatusCode::BAD_GATEWAY, "backend failed")
            })?;
            all.extend(listed.into_iter().map(|l| Collection {
                kind,
                id: l.id,
                name: l.name,
                color: l.color,
                sync_token: l.sync_token,
            }));
        }
        Ok(all)
    }
}
