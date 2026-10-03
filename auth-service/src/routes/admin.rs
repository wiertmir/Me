use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::signup::{valid_email, valid_username, validation};
use crate::{
    AppState, crypto,
    sessions::AdminUser,
    users::{self, FlagsOutcome, NewUser, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users, create_user))
        .routes(routes!(patch_user))
        .routes(routes!(reset_user_password))
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such user")
}

/// All users ordered by username.
#[utoipa::path(get, path = "/api/admin/users", responses(
    (status = 200, body = Vec<User>),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
))]
async fn list_users(
    State(s): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<User>>, ApiError> {
    Ok(Json(users::list_all(&s.db)?))
}

#[derive(Deserialize, ToSchema)]
struct CreateUserRequest {
    username: String,
    email: String,
    display_name: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct CreateUserResponse {
    user: User,
    /// Shown once; the user must change it at first sign-in.
    temporary_password: String,
}

/// Creates a verified, non-admin user with a one-time temporary password.
#[utoipa::path(post, path = "/api/admin/users", request_body = CreateUserRequest, responses(
    (status = 201, body = CreateUserResponse),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 409, description = "username or email taken", body = ErrorBody),
    (status = 422, body = ErrorBody),
))]
async fn create_user(
    State(s): State<AppState>,
    AdminUser(admin): AdminUser,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreateUserResponse>), ApiError> {
    let (username, email) = (
        users::normalize(&req.username),
        users::normalize(&req.email),
    );
    if !valid_username(&username) {
        return Err(validation(
            "username must be 3-32 characters of a-z, 0-9, . _ -",
        ));
    }
    if !valid_email(&email) {
        return Err(validation("email address is not valid"));
    }
    let display_name = req
        .display_name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    if display_name.chars().count() > 100 {
        return Err(validation("display name is too long"));
    }
    let temporary_password = crypto::temporary_password();
    let password_hash = super::hash(&s, temporary_password.clone()).await?;
    let mut user = users::create(
        &s.db,
        NewUser {
            username,
            email,
            email_verified: true,
            is_admin: false,
            must_change_password: true,
            password_hash: Some(password_hash),
        },
    )?;
    if !display_name.is_empty() {
        users::set_display_name(&s.db, user.id, &display_name)?;
        user.display_name = display_name;
    }
    tracing::info!(event = "admin_user_created", admin_id = %admin.user.id, target_id = %user.id);
    Ok((
        StatusCode::CREATED,
        Json(CreateUserResponse {
            user,
            temporary_password,
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
struct PatchUserRequest {
    disabled: Option<bool>,
    is_admin: Option<bool>,
}

/// Disables/enables or promotes/demotes a user. Disabling revokes their sessions and refresh tokens.
#[utoipa::path(patch, path = "/api/admin/users/{id}", params(("id" = Uuid, Path)), request_body = PatchUserRequest, responses(
    (status = 200, body = User),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 404, body = ErrorBody),
    (status = 409, description = "last_admin", body = ErrorBody),
    (status = 422, body = ErrorBody),
))]
async fn patch_user(
    State(s): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchUserRequest>,
) -> Result<Json<User>, ApiError> {
    if req.disabled.is_none() && req.is_admin.is_none() {
        return Err(validation("provide disabled and/or is_admin"));
    }
    match users::update_flags(&s.db, id, req.disabled, req.is_admin)? {
        FlagsOutcome::NotFound => return Err(not_found()),
        FlagsOutcome::LastAdmin => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "last_admin",
                "there must be at least one enabled admin",
            ));
        }
        FlagsOutcome::Updated => {}
    }
    tracing::info!(event = "admin_user_updated", admin_id = %admin.user.id, target_id = %id, disabled = ?req.disabled, is_admin = ?req.is_admin);
    Ok(Json(users::get(&s.db, id)?))
}

#[derive(Serialize, ToSchema)]
struct TemporaryPassword {
    /// Shown once; the user must change it at next sign-in.
    temporary_password: String,
}

/// Gives the user a new temporary password and revokes their sessions, refresh tokens and reset tokens.
#[utoipa::path(post, path = "/api/admin/users/{id}/reset-password", params(("id" = Uuid, Path)), responses(
    (status = 200, body = TemporaryPassword),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 404, body = ErrorBody),
))]
async fn reset_user_password(
    State(s): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> Result<Json<TemporaryPassword>, ApiError> {
    let temporary_password = crypto::temporary_password();
    let password_hash = super::hash(&s, temporary_password.clone()).await?;
    if !users::admin_reset(&s.db, id, &password_hash)? {
        return Err(not_found());
    }
    tracing::info!(event = "admin_password_reset", admin_id = %admin.user.id, target_id = %id);
    Ok(Json(TemporaryPassword { temporary_password }))
}
