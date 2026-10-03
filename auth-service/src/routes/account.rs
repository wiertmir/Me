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
    social_store::{self, Identity, Unlink},
    users::{self, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_me, patch_me))
        .routes(routes!(list_sessions))
        .routes(routes!(revoke_session))
        .routes(routes!(list_identities))
        .routes(routes!(unlink_identity))
        .routes(routes!(confirm_identity))
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

/// The caller's linked social identities.
#[utoipa::path(get, path = "/api/me/identities", responses(
    (status = 200, body = Vec<Identity>),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
))]
async fn list_identities(
    State(s): State<AppState>,
    me: SessionUser,
) -> Result<Json<Vec<Identity>>, ApiError> {
    Ok(Json(social_store::list_identities(&s.db, me.user.id)?))
}

/// Unlinks a provider, unless it is the caller's only way to sign in.
#[utoipa::path(delete, path = "/api/me/identities/{provider}", params(("provider" = String, Path)), responses(
    (status = 204),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 404, description = "not_found (not linked)", body = ErrorBody),
    (status = 409, description = "last_sign_in_method", body = ErrorBody),
))]
async fn unlink_identity(
    State(s): State<AppState>,
    me: SessionUser,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiError> {
    match social_store::unlink(&s.db, me.user.id, &provider)? {
        Unlink::NotLinked => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "identity not linked",
        )),
        Unlink::LastMethod => Err(social_store::conflict()),
        Unlink::Done => {
            tracing::info!(event = "identity_unlinked", user_id = %me.user.id, provider = %provider);
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct ConfirmIdentity {
    ticket: String,
}

/// Attaches the provider identity from a link callback to the signed-in user. The ticket must have been
/// issued for this very user, so a link flow completed in someone else's browser cannot attach anything.
#[utoipa::path(post, path = "/api/me/identities/confirm", request_body = ConfirmIdentity, responses(
    (status = 204),
    (status = 400, description = "invalid_ticket (unknown, expired or used)", body = ErrorBody),
    (status = 401, body = ErrorBody),
    (status = 403, description = "forbidden (ticket belongs to another user)", body = ErrorBody),
    (status = 409, description = "identity_in_use", body = ErrorBody),
))]
async fn confirm_identity(
    State(s): State<AppState>,
    me: SessionUser,
    Json(req): Json<ConfirmIdentity>,
) -> Result<StatusCode, ApiError> {
    let t = social_store::take_link_ticket(&s.db, &req.ticket)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_ticket",
            "unknown, expired or already used ticket",
        )
    })?;
    if t.user != me.user.id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "ticket was issued for another user",
        ));
    }
    let in_use = || {
        ApiError::new(
            StatusCode::CONFLICT,
            "identity_in_use",
            "identity is linked to another user",
        )
    };
    match social_store::find_identity(&s.db, &t.provider, &t.subject)? {
        Some(owner) if owner != me.user.id => return Err(in_use()),
        Some(_) => {}
        None => {
            if !social_store::insert_identity(
                &s.db,
                me.user.id,
                &t.provider,
                &t.subject,
                t.email.as_deref(),
            )? {
                return Err(in_use());
            }
            tracing::info!(event = "identity_linked", user_id = %me.user.id, provider = %t.provider);
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
