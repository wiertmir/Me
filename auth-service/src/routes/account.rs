use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use common::{ApiError, ErrorBody};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    extract::{ApiJson, PathId},
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

/// Get the signed-in user
///
/// Also available while a password change is pending.
#[utoipa::path(
    get, path = "/api/me",
    tag = "account",
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 200, description = "the user", body = User),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
)
)]
async fn get_me(PendingUser(me): PendingUser) -> Json<User> {
    Json(me.user)
}

#[derive(Deserialize, ToSchema)]
struct PatchMe {
    /// At most 100 characters; surrounding whitespace is trimmed.
    display_name: String,
}

/// Update the profile
///
/// Changes the display name (at most 100 characters).
#[utoipa::path(
    patch, path = "/api/me",
    tag = "account",
    request_body(content = PatchMe, example = json!({"display_name": "Alice"})),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 200, description = "the updated user", body = User),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, or display name over 100 characters", body = ErrorBody),
)
)]
async fn patch_me(
    State(s): State<AppState>,
    me: SessionUser,
    ApiJson(req): ApiJson<PatchMe>,
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

/// List the user's sessions
///
/// The caller's live sessions, newest first; `current` marks the requesting one.
#[utoipa::path(
    get, path = "/api/me/sessions",
    tag = "account",
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 200, description = "the sessions", body = Vec<SessionInfo>),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
)
)]
async fn list_sessions(
    State(s): State<AppState>,
    me: SessionUser,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    Ok(Json(sessions::list(&s.db, me.user.id, me.session_id)?))
}

/// Revoke a session
///
/// Revokes one of the caller's own sessions (the current one too, like sign-out).
#[utoipa::path(
    delete, path = "/api/me/sessions/{id}",
    tag = "account",
    params(("id" = Uuid, Path)),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "session revoked"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 404, description = "`not_found`: unknown id, not a UUID, or not the caller's session", body = ErrorBody),
)
)]
async fn revoke_session(
    State(s): State<AppState>,
    me: SessionUser,
    PathId(id): PathId,
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

/// List linked social identities
///
/// The caller's linked sign-in providers.
#[utoipa::path(
    get, path = "/api/me/identities",
    tag = "account",
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 200, description = "the identities", body = Vec<Identity>),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
)
)]
async fn list_identities(
    State(s): State<AppState>,
    me: SessionUser,
) -> Result<Json<Vec<Identity>>, ApiError> {
    Ok(Json(social_store::list_identities(&s.db, me.user.id)?))
}

/// Unlink a social identity
///
/// Unlinks a provider, unless it is the caller's only way to sign in. Also signs the user out everywhere
/// else: it revokes their other sessions, refresh tokens, authorization codes and pending social tickets,
/// because someone who got in through that provider may still hold them. The current session and app
/// passwords are kept.
#[utoipa::path(
    delete, path = "/api/me/identities/{provider}",
    tag = "account",
    params(("provider" = String, Path)),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "identity unlinked; other sessions and refresh tokens revoked"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 404, description = "`not_found`: that provider is not linked", body = ErrorBody),
    (status = 409, description = "`last_sign_in_method`: it is the only way the user can sign in", body = ErrorBody),
)
)]
async fn unlink_identity(
    State(s): State<AppState>,
    me: SessionUser,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiError> {
    match social_store::unlink(&s.db, me.user.id, &provider, me.session_id)? {
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

/// Confirm a social link
///
/// Attaches the provider identity from a link callback to the signed-in user. The ticket must have been issued for this very user, so a link flow completed in someone else's browser cannot attach anything.
#[utoipa::path(
    post, path = "/api/me/identities/confirm",
    tag = "account",
    request_body(content = ConfirmIdentity, example = json!({"ticket": "k3T9v2Hq0sZb1m8Y4pJx7WcD5nRfA6eLgUoQiVtXyBs"})),
    security(("service_secret" = [], "session" = [])),
    responses(
    (status = 204, description = "identity linked"),
    (status = 400, description = "`invalid_ticket`: unknown, expired or already used ticket", body = ErrorBody),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`, or missing, invalid or expired session token", body = ErrorBody),
    (status = 403, description = "`forbidden`: the ticket was issued for another user; `password_change_required`: the user must change their temporary password first", body = ErrorBody),
    (status = 409, description = "`identity_in_use`: the provider identity is linked to another user", body = ErrorBody),
    (status = 422, description = "`validation`: the body is missing or malformed, or a field is invalid (see `message`)", body = ErrorBody),
)
)]
async fn confirm_identity(
    State(s): State<AppState>,
    me: SessionUser,
    ApiJson(req): ApiJson<ConfirmIdentity>,
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
