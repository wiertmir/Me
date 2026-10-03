use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{auth::SigninError, client_info};
use crate::{
    AppState,
    config::SignupMode,
    crypto,
    email_tokens::{self, Purpose},
    extract::ApiJson,
    mail::Email,
    users::{self, NewUser, User},
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(signup))
        .routes(routes!(verify_email))
        .routes(routes!(resend_verification))
        .routes(routes!(forgot_password))
        .routes(routes!(reset_password))
}

pub(crate) fn validation(msg: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", msg)
}

fn invalid_token() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_token",
        "invalid or expired token",
    )
}

async fn send_token_mail(s: &AppState, user: &User, purpose: Purpose) -> Result<(), ApiError> {
    // A passwordless account was created socially, possibly with an address its creator does not own;
    // verifying it by mail would bless that. It becomes verified only through a password reset.
    if matches!(purpose, Purpose::Verify) && !user.has_password {
        return Ok(());
    }
    let token = email_tokens::create(&s.db, user.id, purpose)?;
    let (path, subject, what) = match purpose {
        Purpose::Verify => (
            "verify",
            "Verify your email address",
            "To verify your email address",
        ),
        Purpose::Reset => ("reset", "Reset your password", "To reset your password"),
    };
    let body = format!(
        "{what}, open this link (valid for {}):\n\n{}/{path}?token={token}\n\nIf you did not ask for this, ignore this email.\n",
        purpose.valid_for(),
        s.cfg.web_url.trim_end_matches('/'),
    );
    s.mail
        .dispatch(Email {
            to: user.email.clone(),
            subject: subject.into(),
            body,
        })
        .await;
    Ok(())
}

#[derive(Deserialize, ToSchema)]
struct SignupRequest {
    username: String,
    email: String,
    password: String,
    display_name: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct SignupResponse {
    user: User,
    verification_required: bool,
}

pub(crate) fn valid_username(u: &str) -> bool {
    (3..=32).contains(&u.len())
        && u.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

pub(crate) fn valid_email(e: &str) -> bool {
    let mut parts = e.split('@');
    matches!((parts.next(), parts.next(), parts.next()), (Some(l), Some(d), None) if !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'))
        && e.len() <= 254
        && !e.contains(|c: char| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
}

/// Sign up
///
/// Self sign-up. The email address always starts unverified. With mail enabled the account must verify it
/// before it can sign in; without mail it can sign in at once, but the address stays unverified (so a
/// social identity with the same address is never linked to it automatically).
#[utoipa::path(
    post, path = "/api/signup",
    tag = "auth",
    request_body(content = SignupRequest, example = json!({"username": "alice", "email": "alice@example.com", "password": "correct horse battery staple", "display_name": "Alice"})),
    security(("service_secret" = [])),
    responses(
    (status = 201, description = "account created; `verification_required` says whether a verification mail was sent", body = SignupResponse),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 403, description = "`signup_disabled`: self sign-up is switched off", body = ErrorBody),
    (status = 409, description = "`conflict`: username or email already in use", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, invalid username (3-32 of a-z 0-9 . _ -), invalid email, password shorter than 12 characters or longer than 1024 bytes, or display name over 100 characters", body = ErrorBody),
)
)]
async fn signup(
    State(s): State<AppState>,
    ApiJson(req): ApiJson<SignupRequest>,
) -> Result<(StatusCode, Json<SignupResponse>), ApiError> {
    if s.cfg.signup == SignupMode::Disabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "signup_disabled",
            "sign-up is disabled",
        ));
    }
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
    crypto::validate_password(&req.password)?;
    let display_name = req
        .display_name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    if display_name.chars().count() > 100 {
        return Err(validation("display name is too long"));
    }
    let verification_required = s.mail.enabled();
    let password_hash = super::hash(&s, req.password).await?;
    let mut user = users::create(
        &s.db,
        NewUser {
            username,
            email,
            // Never verified here: nobody has proven the address. Without mail the sign-in gate does not
            // apply, and an unverified address is never auto-linked to a provider identity.
            email_verified: false,
            is_admin: false,
            must_change_password: false,
            password_hash: Some(password_hash),
        },
    )?;
    if !display_name.is_empty() {
        users::set_display_name(&s.db, user.id, &display_name)?;
        user.display_name = display_name;
    }
    tracing::info!(event = "signup", user_id = %user.id, verification_required);
    if verification_required {
        // The account exists either way; a lost mail is recovered through /api/email/resend.
        if let Err(e) = send_token_mail(&s, &user, Purpose::Verify).await {
            tracing::error!(event = "signup", user_id = %user.id, error = %e.message, "creating verification token failed");
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(SignupResponse {
            user,
            verification_required,
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
struct TokenRequest {
    token: String,
}

/// Verify an email address
///
/// Confirms an email address with the single-use token from the verification mail.
#[utoipa::path(
    post, path = "/api/email/verify",
    tag = "auth",
    request_body(content = TokenRequest, example = json!({"token": "k3T9v2Hq0sZb1m8Y4pJx7WcD5nRfA6eLgUoQiVtXyBs"})),
    security(("service_secret" = [])),
    responses(
    (status = 204, description = "email address verified"),
    (status = 400, description = "`invalid_token`: unknown, expired or already used token", body = ErrorBody),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 422, description = "`validation`: the body is missing or malformed, or a field is invalid (see `message`)", body = ErrorBody),
)
)]
async fn verify_email(
    State(s): State<AppState>,
    ApiJson(req): ApiJson<TokenRequest>,
) -> Result<StatusCode, ApiError> {
    let id =
        email_tokens::consume(&s.db, &req.token, Purpose::Verify)?.ok_or_else(invalid_token)?;
    if !users::get(&s.db, id)?.has_password {
        return Err(invalid_token());
    }
    users::mark_verified(&s.db, id)?;
    tracing::info!(event = "email_verified", user_id = %id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct EmailRequest {
    email: String,
}

/// Counts a mail request against per-IP and per-email limits (never cleared), so the endpoint
/// cannot be used to flood a mailbox. The email key locks whether or not the account exists.
fn limit_mail(
    s: &AppState,
    kind: &str,
    headers: &HeaderMap,
    email: &str,
) -> Result<String, SigninError> {
    let (ip, _) = client_info(headers);
    s.limiter
        .begin(&format!("ip:{kind}:{ip}"))
        .map_err(SigninError::Limited)?;
    s.limiter
        .begin(&format!(
            "{kind}:{}",
            email.chars().take(254).collect::<String>()
        ))
        .map_err(SigninError::Limited)?;
    Ok(ip)
}

async fn mail_target(s: &AppState, email: &str) -> Result<Option<User>, ApiError> {
    Ok(users::find_by_login(&s.db, email)?
        .map(|(u, _)| u)
        .filter(|u| u.email == email && !u.disabled))
}

/// Resend the verification mail
///
/// Sends a fresh verification mail if the account exists and is unverified. Always 204, so it does not reveal which addresses exist.
#[utoipa::path(
    post, path = "/api/email/resend",
    tag = "auth",
    request_body(content = EmailRequest, example = json!({"email": "alice@example.com"})),
    security(("service_secret" = [])),
    responses(
    (status = 204, description = "accepted"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 422, description = "`validation`: the body is missing or malformed, or a field is invalid (see `message`)", body = ErrorBody),
    (status = 429, description = "`rate_limited`: too many requests for this address or client; see `Retry-After`", body = ErrorBody),
)
)]
async fn resend_verification(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<EmailRequest>,
) -> Result<StatusCode, SigninError> {
    let email = users::normalize(&req.email);
    limit_mail(&s, "resend", &headers, &email)?;
    if s.mail.enabled()
        && let Some(user) = mail_target(&s, &email).await?.filter(|u| !u.email_verified)
    {
        send_token_mail(&s, &user, Purpose::Verify).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Request a password reset mail
///
/// Mails a reset link if the account exists and mail is enabled. Always 204, so it does not reveal which addresses exist.
#[utoipa::path(
    post, path = "/api/password/forgot",
    tag = "auth",
    request_body(content = EmailRequest, example = json!({"email": "alice@example.com"})),
    security(("service_secret" = [])),
    responses(
    (status = 204, description = "accepted"),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 422, description = "`validation`: the body is missing or malformed, or a field is invalid (see `message`)", body = ErrorBody),
    (status = 429, description = "`rate_limited`: too many requests for this address or client; see `Retry-After`", body = ErrorBody),
)
)]
async fn forgot_password(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<EmailRequest>,
) -> Result<StatusCode, SigninError> {
    if !s.mail.enabled() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let email = users::normalize(&req.email);
    let ip = limit_mail(&s, "forgot", &headers, &email)?;
    let user = mail_target(&s, &email).await?;
    // The log is server-side only; the response is identical either way.
    tracing::info!(event = "password_reset_requested", ip = %ip, user_id = user.as_ref().map(|u| u.id.to_string()));
    if let Some(user) = user {
        send_token_mail(&s, &user, Purpose::Reset).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct ResetRequest {
    token: String,
    new_password: String,
}

/// Reset a password
///
/// Sets a new password from a single-use reset token. Revokes all sign-in state: sessions, refresh tokens and tickets.
#[utoipa::path(
    post, path = "/api/password/reset",
    tag = "auth",
    request_body(content = ResetRequest, example = json!({"token": "k3T9v2Hq0sZb1m8Y4pJx7WcD5nRfA6eLgUoQiVtXyBs", "new_password": "another long passphrase"})),
    security(("service_secret" = [])),
    responses(
    (status = 204, description = "password changed"),
    (status = 400, description = "`invalid_token`: unknown, expired or already used token", body = ErrorBody),
    (status = 401, description = "`unauthorized`: missing or wrong `X-Service-Secret`", body = ErrorBody),
    (status = 422, description = "`validation`: malformed body, new password shorter than 12 characters or longer than 1024 bytes", body = ErrorBody),
)
)]
async fn reset_password(
    State(s): State<AppState>,
    ApiJson(req): ApiJson<ResetRequest>,
) -> Result<StatusCode, ApiError> {
    // Validated first so a typo does not burn the token.
    crypto::validate_password(&req.new_password)?;
    let id = email_tokens::consume(&s.db, &req.token, Purpose::Reset)?.ok_or_else(invalid_token)?;
    let password_hash = super::hash(&s, req.new_password).await?;
    users::complete_reset(&s.db, id, &password_hash)?;
    tracing::info!(event = "password_reset_completed", user_id = %id);
    Ok(StatusCode::NO_CONTENT)
}
