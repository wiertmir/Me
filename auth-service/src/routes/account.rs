use axum::{Json, extract::State, http::StatusCode};
use common::{ApiError, ErrorBody};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    sessions::{PendingUser, SessionUser},
    users::{self, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_me, patch_me))
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
async fn patch_me(State(s): State<AppState>, me: SessionUser, Json(req): Json<PatchMe>) -> Result<Json<User>, ApiError> {
    let name = req.display_name.trim();
    if name.chars().count() > 100 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", "display name is too long"));
    }
    users::set_display_name(&s.db, me.user.id, name)?;
    Ok(Json(users::get(&s.db, me.user.id)?))
}
