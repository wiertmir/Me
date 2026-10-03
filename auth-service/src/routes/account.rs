use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use common::{ApiError, ErrorBody};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    AppState,
    sessions::{self, PendingUser, SessionInfo, SessionUser},
    users::{self, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_me, patch_me))
        .routes(routes!(list_sessions))
        .routes(routes!(revoke_session))
}

/// The signed-in user. Also available while a password change is pending.
#[utoipa::path(get, path = "/api/me", responses((status = 200, body = User), (status = 401, body = ErrorBody)))]
async fn get_me(PendingUser(me): PendingUser) -> Json<User> {
    Json(me.user)
}

#[derive(Deserialize, ToSchema)]
struct PatchMe {
    display_name: String,
}

/// Updates the profile (display name, at most 100 characters).
#[utoipa::path(patch, path = "/api/me", request_body = PatchMe, responses(
    (status = 200, body = User),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 422, body = ErrorBody),
))]
async fn patch_me(
    State(s): State<AppState>,
    me: SessionUser,
    Json(req): Json<PatchMe>,
) -> Result<Json<User>, ApiError> {
    let name = req.display_name.trim();
    if name.chars().count() > 100 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation",
            "display name is too long",
        ));
    }
    users::set_display_name(&s.db, me.user.id, name)?;
    Ok(Json(users::get(&s.db, me.user.id)?))
}

/// The caller's live sessions; `current` marks the requesting one.
#[utoipa::path(get, path = "/api/me/sessions", responses(
    (status = 200, body = Vec<SessionInfo>),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
))]
async fn list_sessions(
    State(s): State<AppState>,
    me: SessionUser,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    Ok(Json(sessions::list(&s.db, me.user.id, me.session_id)?))
}

/// Revokes one of the caller's own sessions (the current one too, like sign-out).
#[utoipa::path(delete, path = "/api/me/sessions/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 404, description = "not_found (unknown or not the caller's)", body = ErrorBody),
))]
async fn revoke_session(
    State(s): State<AppState>,
    me: SessionUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !sessions::delete_owned(&s.db, me.user.id, id)? {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such session",
        ));
    }
    tracing::info!(event = "session_revoked", user_id = %me.user.id, session_id = %id);
    Ok(StatusCode::NO_CONTENT)
}
