//! Social sign-in: this service is the OAuth client of Google, GitHub and Microsoft.
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::client_info;
use crate::{
    AppState,
    config::SignupMode,
    crypto, providers,
    sessions::{self, SessionUser},
    social_store,
    users::{self, NewUser, User},
};

const COOKIE: &str = "me_social_state";
const MAX_CHALLENGE: usize = 512;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_providers))
        .routes(routes!(link_intent))
        .routes(routes!(exchange))
        .routes(routes!(start))
        .routes(routes!(callback))
}

fn trimmed(u: &str) -> &str {
    u.trim_end_matches('/')
}

/// A 302 to a path on the web app, optionally setting a cookie. Never built from request input.
fn go(s: &AppState, path: &str, cookie: Option<HeaderValue>) -> Response {
    let to = format!("{}{path}", trimmed(&s.cfg.web_url));
    redirect(&to, cookie)
}

fn redirect(to: &str, cookie: Option<HeaderValue>) -> Response {
    let Ok(to) = HeaderValue::from_str(to) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut r = (
        StatusCode::FOUND,
        [
            (header::LOCATION, to),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response();
    if let Some(c) = cookie {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r
}

fn state_cookie(s: &AppState, value: &str, max_age: u32) -> HeaderValue {
    let secure = if s.cfg.issuer.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    // The value is base64url, so it is always a valid header value.
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; HttpOnly{secure}; SameSite=Lax; Path=/social; Max-Age={max_age}"
    ))
    .expect("cookie is ASCII")
}

fn cookie_value(h: &HeaderMap) -> Option<&str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|p| p.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

fn configured<'a>(s: &'a AppState, provider: &str) -> Option<&'a crate::config::ProviderConfig> {
    providers::PROVIDERS
        .contains(&provider)
        .then(|| s.cfg.providers.get(provider))?
}

fn redirect_uri(s: &AppState, provider: &str) -> String {
    format!("{}/social/{provider}/callback", trimmed(&s.cfg.issuer))
}

/// Providers with configured credentials, in a fixed order.
#[utoipa::path(get, path = "/api/providers", responses((status = 200, body = Vec<String>), (status = 401, body = ErrorBody)))]
async fn list_providers(State(s): State<AppState>) -> Json<Vec<&'static str>> {
    Json(
        providers::PROVIDERS
            .into_iter()
            .filter(|p| s.cfg.providers.contains_key(*p))
            .collect(),
    )
}

#[derive(Deserialize, ToSchema)]
struct LinkIntentRequest {
    provider: String,
}

#[derive(Serialize, ToSchema)]
struct LinkIntentResponse {
    start_url: String,
}

/// Creates a one-time (10 minute) intent so the signed-in user can link a provider from the account page.
#[utoipa::path(post, path = "/api/social/link-intent", request_body = LinkIntentRequest, responses(
    (status = 200, body = LinkIntentResponse),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
    (status = 404, description = "not_found (provider not configured)", body = ErrorBody),
))]
async fn link_intent(
    State(s): State<AppState>,
    me: SessionUser,
    Json(req): Json<LinkIntentRequest>,
) -> Result<Json<LinkIntentResponse>, ApiError> {
    if configured(&s, &req.provider).is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "provider not available",
        ));
    }
    let token = social_store::create_link_intent(&s.db, me.user.id, &req.provider)?;
    Ok(Json(LinkIntentResponse {
        start_url: format!(
            "{}/social/{}/start?link={token}",
            trimmed(&s.cfg.issuer),
            req.provider
        ),
    }))
}

#[derive(Deserialize, IntoParams)]
struct StartParams {
    /// OAuth auth-request challenge to carry through to the ticket exchange.
    challenge: Option<String>,
    /// One-time link intent from `POST /api/social/link-intent`.
    link: Option<String>,
}

/// Redirects the browser to the provider (state + PKCE) and sets the `me_social_state` cookie.
#[utoipa::path(get, path = "/social/{provider}/start", params(StartParams, ("provider" = String, Path)), responses(
    (status = 302, description = "to the provider, or to the web app with an error"),
))]
async fn start(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Query(p): Query<StartParams>,
) -> Response {
    let Some(pc) = configured(&s, &provider) else {
        return go(&s, "/signin?error=provider_unavailable", None);
    };
    let failed = |link: bool| {
        go(
            &s,
            if link {
                "/account/security?error=social_failed"
            } else {
                "/signin?error=social_failed"
            },
            None,
        )
    };
    if p.challenge.is_some() && p.link.is_some() {
        return failed(false);
    }
    if p.challenge
        .as_ref()
        .is_some_and(|c| c.len() > MAX_CHALLENGE)
    {
        return failed(false);
    }
    let link_user = match p.link.as_deref() {
        Some(token) => match social_store::take_link_intent(&s.db, token, &provider) {
            Ok(Some(u)) => Some(u),
            _ => return failed(true),
        },
        None => None,
    };
    let verifier = crypto::random_token();
    let code_challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let Ok(state) = social_store::create_state(
        &s.db,
        &provider,
        &verifier,
        p.challenge.as_deref(),
        link_user,
    ) else {
        return failed(link_user.is_some());
    };
    match providers::authorize_url(
        &provider,
        pc,
        &redirect_uri(&s, &provider),
        &state,
        &code_challenge,
    ) {
        Some(to) => redirect(&to, Some(state_cookie(&s, &state, 600))),
        None => failed(link_user.is_some()),
    }
}

#[derive(Deserialize, IntoParams)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Why a callback ended in an error redirect.
struct Fail {
    link: bool,
    code: &'static str,
    reason: String,
}

impl Fail {
    fn new(link: bool, code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            link,
            code,
            reason: reason.into(),
        }
    }
}

impl From<ApiError> for Fail {
    fn from(_: ApiError) -> Self {
        Self::new(false, "social_failed", "database error")
    }
}

/// Provider redirect target. Applies the sign-in / link rules and ends at the web app.
#[utoipa::path(get, path = "/social/{provider}/callback", params(CallbackParams, ("provider" = String, Path)), responses(
    (status = 302, description = "to the web app: a ticket on success, an error code otherwise"),
))]
async fn callback(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let clear = Some(state_cookie(&s, "", 0));
    match run(&s, &provider, q, &headers).await {
        Ok(path) => go(&s, &path, clear),
        Err(f) => {
            tracing::warn!(event = "social_failed", provider = %provider, reason = %f.reason, error = f.code);
            let base = if f.link {
                "/account/security"
            } else {
                "/signin"
            };
            go(&s, &format!("{base}?error={}", f.code), clear)
        }
    }
}

async fn run(
    s: &AppState,
    provider: &str,
    q: CallbackParams,
    headers: &HeaderMap,
) -> Result<String, Fail> {
    let raw = q.state.as_deref().unwrap_or_default();
    // Consumed first, so a failed attempt cannot be retried with the same state.
    let Some(st) = social_store::take_state(&s.db, raw)? else {
        return Err(Fail::new(
            false,
            "social_failed",
            "state unknown, expired or used",
        ));
    };
    let link = st.link_user.is_some();
    let fail = |code, reason: &str| Fail::new(link, code, reason);
    let bound =
        cookie_value(headers).is_some_and(|c| bool::from(c.as_bytes().ct_eq(raw.as_bytes())));
    if !bound || st.provider != provider {
        return Err(fail(
            "social_failed",
            "state not bound to this browser or provider",
        ));
    }
    let (Some(pc), Some(code), None) = (
        configured(s, provider),
        q.code.as_deref(),
        q.error.as_deref(),
    ) else {
        return Err(fail("social_failed", "provider error or no code"));
    };
    let profile = providers::fetch_profile(
        &s.http,
        provider,
        pc,
        &redirect_uri(s, provider),
        code,
        &st.verifier,
    )
    .await
    .map_err(|e| fail("social_failed", &e))?;

    if let Some(uid) = st.link_user {
        match social_store::find_identity(&s.db, provider, &profile.subject)? {
            Some(owner) if owner != uid => {
                return Err(fail("identity_in_use", "linked to another user"));
            }
            _ => {}
        }
        // The identity is attached only when the signed-in user's own session confirms this ticket.
        let ticket = social_store::create_link_ticket(
            &s.db,
            uid,
            provider,
            &profile.subject,
            profile.email.as_deref(),
        )?;
        return Ok(format!("/account/security?link_ticket={ticket}"));
    }

    let user_id = match social_store::find_identity(&s.db, provider, &profile.subject)? {
        Some(uid) => {
            if users::get(&s.db, uid)?.disabled {
                return Err(fail("account_disabled", "user disabled"));
            }
            uid
        }
        None => match profile
            .email
            .as_deref()
            .filter(|e| e.contains('@') && e.len() <= 254)
        {
            Some(email) => match users::find_by_login(&s.db, email)? {
                Some((u, _)) => {
                    // Both sides must vouch for the address; anything else needs a password sign-in first.
                    if !(profile.email_verified
                        && u.email_verified
                        && u.email == users::normalize(email))
                    {
                        return Err(fail(
                            "account_exists",
                            "email belongs to an existing account",
                        ));
                    }
                    if u.disabled {
                        return Err(fail("account_disabled", "user disabled"));
                    }
                    if !social_store::insert_identity(
                        &s.db,
                        u.id,
                        provider,
                        &profile.subject,
                        Some(email),
                    )? {
                        return Err(fail("social_failed", "identity already linked"));
                    }
                    tracing::info!(event = "identity_linked", user_id = %u.id, provider = %provider);
                    u.id
                }
                None => sign_up(s, provider, &profile, Some(email))?,
            },
            None => sign_up(s, provider, &profile, None)?,
        },
    };
    let ticket = social_store::create_ticket(&s.db, user_id, st.challenge.as_deref())?;
    tracing::info!(event = "social_signin", user_id = %user_id, provider = %provider);
    Ok(format!("/social/complete?ticket={ticket}"))
}

fn sign_up(
    s: &AppState,
    provider: &str,
    p: &providers::Profile,
    email: Option<&str>,
) -> Result<uuid::Uuid, Fail> {
    if s.cfg.signup == SignupMode::Disabled {
        return Err(Fail::new(false, "signup_disabled", "sign-up is closed"));
    }
    let Some(email) = email else {
        return Err(Fail::new(false, "email_required", "provider gave no email"));
    };
    let base = username_base(email);
    let mut user = None;
    // ponytail: suffix probing is linear; fine until thousands of identical local parts
    for n in 0..100u32 {
        let new = NewUser {
            username: username_candidate(&base, n),
            email: email.into(),
            email_verified: p.email_verified,
            is_admin: false,
            must_change_password: false,
            password_hash: None,
        };
        match users::create(&s.db, new) {
            Ok(u) => {
                user = Some(u);
                break;
            }
            Err(e) if e.code == "conflict" => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let user = user.ok_or_else(|| Fail::new(false, "social_failed", "no free username"))?;
    if let Some(name) = &p.name {
        users::set_display_name(&s.db, user.id, &name.chars().take(100).collect::<String>())?;
    }
    if !social_store::insert_identity(&s.db, user.id, provider, &p.subject, Some(email))? {
        return Err(Fail::new(false, "social_failed", "identity already linked"));
    }
    tracing::info!(event = "social_signup", user_id = %user.id, provider = %provider);
    Ok(user.id)
}

/// Email local part, lower-cased, reduced to `a-z 0-9 . _ -`, at least 3 characters.
fn username_base(email: &str) -> String {
    let mut b: String = email
        .split('@')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        .take(32)
        .collect();
    while b.len() < 3 {
        b.push('0');
    }
    b
}

/// `base`, then `base1`, `base2`… truncated so the result stays within 32 characters.
fn username_candidate(base: &str, n: u32) -> String {
    if n == 0 {
        return base.to_string();
    }
    let suffix = n.to_string();
    format!("{}{suffix}", &base[..base.len().min(32 - suffix.len())])
}

#[derive(Deserialize, ToSchema)]
struct ExchangeRequest {
    ticket: String,
}

#[derive(Serialize, ToSchema)]
struct ExchangeResponse {
    session_token: String,
    user: User,
    /// The OAuth auth-request challenge given at start, if any.
    challenge: Option<String>,
}

/// Trades the one-time ticket from the callback redirect for a session.
// Social accounts are not blocked by email verification: a ticket only exists for a user with a linked identity.
#[utoipa::path(post, path = "/api/social/exchange", request_body = ExchangeRequest, responses(
    (status = 200, body = ExchangeResponse),
    (status = 400, description = "invalid_ticket (unknown, expired or used)", body = ErrorBody),
    (status = 401, body = ErrorBody),
))]
async fn exchange(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExchangeRequest>,
) -> Result<Json<ExchangeResponse>, ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_ticket",
            "unknown, expired or already used ticket",
        )
    };
    let (uid, challenge) = social_store::take_ticket(&s.db, &req.ticket)?.ok_or_else(invalid)?;
    let user = users::get(&s.db, uid)?;
    if user.disabled {
        return Err(invalid());
    }
    let (ip, ua) = client_info(&headers);
    let session_token = sessions::create(&s.db, uid, &ua, &ip)?;
    tracing::info!(event = "signin", user_id = %uid, ip = %ip, outcome = "success", method = "social");
    Ok(Json(ExchangeResponse {
        session_token,
        user,
        challenge,
    }))
}
