//! OAuth 2 / OIDC front half: authorize, the internal auth-request endpoints, discovery and JWKS.
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use common::{ApiError, ErrorBody};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    config::ClientConfig,
    oauth_store::{self, AuthRequest},
    sessions::SessionUser,
};

pub const SUPPORTED_SCOPES: [&str; 4] = ["openid", "profile", "email", "offline_access"];
const MAX_PARAM: usize = 512;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(authorize))
        .routes(routes!(get_auth_request))
        .routes(routes!(accept_auth_request))
        .routes(routes!(discovery))
        .routes(routes!(jwks))
}

pub(crate) fn find_client<'a>(s: &'a AppState, id: &str) -> Option<&'a ClientConfig> {
    s.cfg.clients.iter().find(|c| c.id == id)
}

fn is_loopback(u: &Url) -> bool {
    u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

/// Exact match, except that a registered http loopback URI matches any port (RFC 8252). Never with a fragment.
fn redirect_allowed(registered: &[String], req: &str) -> bool {
    if req.contains('#') {
        return false;
    }
    if registered.iter().any(|r| r == req) {
        return true;
    }
    let Ok(req) = Url::parse(req) else {
        return false;
    };
    if !req.username().is_empty() || req.password().is_some() || !is_loopback(&req) {
        return false;
    }
    registered
        .iter()
        .filter_map(|r| Url::parse(r).ok())
        .any(|r| is_loopback(&r) && r.host_str() == req.host_str() && r.path() == req.path())
}

/// Appends query parameters to a URI that may or may not already have a query string.
fn with_query(uri: &str, pairs: &[(&str, &str)]) -> Option<String> {
    let mut u = Url::parse(uri).ok()?;
    u.query_pairs_mut().extend_pairs(pairs);
    Some(u.into())
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn error_page() -> Response {
    // Static text only: nothing from the request is reflected.
    no_store((StatusCode::BAD_REQUEST, Html("<!doctype html><title>Sign-in error</title><h1>Sign-in error</h1><p>This sign-in link is invalid. Return to the application and try again.</p>")).into_response())
}

fn redirect(to: &str) -> Response {
    match HeaderValue::from_str(to) {
        Ok(v) => no_store((StatusCode::FOUND, [(header::LOCATION, v)]).into_response()),
        Err(_) => error_page(),
    }
}

#[derive(Deserialize, IntoParams)]
struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    nonce: Option<String>,
}

fn valid_challenge(c: &str) -> bool {
    (43..=128).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Starts the authorization code flow (PKCE required). Bad client or redirect_uri: 400 page, never a
/// redirect. Other errors redirect to the client. Success redirects to the web sign-in page.
#[utoipa::path(get, path = "/oauth/authorize", params(AuthorizeParams), responses(
    (status = 302, description = "redirect to the sign-in page, or to the client with an error"),
    (status = 400, description = "unknown client or redirect_uri (HTML)"),
))]
async fn authorize(
    State(s): State<AppState>,
    q: Result<Query<AuthorizeParams>, QueryRejection>,
) -> Response {
    let Ok(Query(p)) = q else { return error_page() };
    let (Some(client_id), Some(redirect_uri)) = (p.client_id.as_deref(), p.redirect_uri.as_deref())
    else {
        return error_page();
    };
    let Some(client) = find_client(&s, client_id) else {
        return error_page();
    };
    if !redirect_allowed(&client.redirect_uris, redirect_uri) {
        return error_page();
    }
    let fail = |error: &str, description: &str| {
        let mut pairs = vec![("error", error), ("error_description", description)];
        if let Some(st) = p.state.as_deref() {
            pairs.push(("state", st));
        }
        with_query(redirect_uri, &pairs).map_or_else(error_page, |to| redirect(&to))
    };
    if p.response_type.as_deref() != Some("code") {
        return fail("unsupported_response_type", "response_type must be code");
    }
    let Some(code_challenge) = p.code_challenge.as_deref().filter(|c| valid_challenge(c)) else {
        return fail("invalid_request", "a valid code_challenge is required");
    };
    if p.code_challenge_method.as_deref() != Some("S256") {
        return fail("invalid_request", "code_challenge_method must be S256");
    }
    if [&p.state, &p.nonce, &p.scope]
        .iter()
        .any(|v| v.as_deref().is_some_and(|v| v.len() > MAX_PARAM))
    {
        return fail("invalid_request", "parameter too long");
    }
    let mut scopes: Vec<&str> = Vec::new();
    for sc in p.scope.as_deref().unwrap_or_default().split_whitespace() {
        if SUPPORTED_SCOPES.contains(&sc) && !scopes.contains(&sc) {
            scopes.push(sc);
        }
    }
    let req = AuthRequest {
        client_id: client.id.clone(),
        redirect_uri: redirect_uri.to_string(),
        scope: scopes.join(" "),
        state: p.state.clone(),
        code_challenge: code_challenge.to_string(),
        nonce: p.nonce.clone(),
    };
    match oauth_store::create_request(&s.db, &req) {
        Ok(challenge) => match with_query(
            &format!("{}/signin", s.cfg.web_url.trim_end_matches('/')),
            &[("challenge", &challenge)],
        ) {
            Some(to) => redirect(&to),
            None => error_page(),
        },
        Err(_) => fail("server_error", "internal error"),
    }
}

fn invalid_challenge() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_challenge",
        "unknown, expired or already used challenge",
    )
}

#[derive(Serialize, ToSchema)]
struct AuthRequestInfo {
    client_id: String,
    client_name: String,
    scope: String,
}

/// What the sign-in page shows about a pending authorization request.
#[utoipa::path(get, path = "/api/auth-requests/{challenge}", params(("challenge" = String, Path)), responses(
    (status = 200, body = AuthRequestInfo),
    (status = 400, description = "invalid_challenge", body = ErrorBody),
    (status = 401, body = ErrorBody),
))]
async fn get_auth_request(
    State(s): State<AppState>,
    Path(challenge): Path<String>,
) -> Result<Json<AuthRequestInfo>, ApiError> {
    let req = oauth_store::get_request(&s.db, &challenge)?.ok_or_else(invalid_challenge)?;
    let client = find_client(&s, &req.client_id).ok_or_else(invalid_challenge)?;
    Ok(Json(AuthRequestInfo {
        client_id: client.id.clone(),
        client_name: client.name.clone(),
        scope: req.scope,
    }))
}

#[derive(Serialize, ToSchema)]
struct AcceptResponse {
    /// The client's redirect_uri with `code` and `state` appended.
    redirect_to: String,
}

/// The signed-in user approves the request; consumes the challenge and returns where to send the browser.
#[utoipa::path(post, path = "/api/auth-requests/{challenge}/accept", params(("challenge" = String, Path)), responses(
    (status = 200, body = AcceptResponse),
    (status = 400, description = "invalid_challenge", body = ErrorBody),
    (status = 401, body = ErrorBody),
    (status = 403, body = ErrorBody),
))]
async fn accept_auth_request(
    State(s): State<AppState>,
    me: SessionUser,
    Path(challenge): Path<String>,
) -> Result<Json<AcceptResponse>, ApiError> {
    let (code, req) =
        oauth_store::accept(&s.db, &challenge, me.user.id)?.ok_or_else(invalid_challenge)?;
    // The client may have been removed from config since the request was made.
    let client = find_client(&s, &req.client_id)
        .filter(|c| redirect_allowed(&c.redirect_uris, &req.redirect_uri))
        .ok_or_else(invalid_challenge)?;
    let mut pairs = vec![("code", code.as_str())];
    if let Some(st) = req.state.as_deref() {
        pairs.push(("state", st));
    }
    let redirect_to = with_query(&req.redirect_uri, &pairs).ok_or_else(invalid_challenge)?;
    tracing::info!(event = "auth_request_accepted", user_id = %me.user.id, client_id = %client.id);
    Ok(Json(AcceptResponse { redirect_to }))
}

/// OpenID Connect discovery document.
#[utoipa::path(get, path = "/.well-known/openid-configuration", responses((status = 200, body = Value)))]
async fn discovery(State(s): State<AppState>) -> Json<Value> {
    let iss = s.cfg.issuer.trim_end_matches('/');
    Json(json!({
        "issuer": s.cfg.issuer,
        "authorization_endpoint": format!("{iss}/oauth/authorize"),
        "token_endpoint": format!("{iss}/oauth/token"),
        "userinfo_endpoint": format!("{iss}/oauth/userinfo"),
        "jwks_uri": format!("{iss}/.well-known/jwks.json"),
        "revocation_endpoint": format!("{iss}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": SUPPORTED_SCOPES,
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

/// Public signing key set.
#[utoipa::path(get, path = "/.well-known/jwks.json", responses((status = 200, body = Value)))]
async fn jwks(State(s): State<AppState>) -> Json<Value> {
    Json(s.signer.jwks())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_matching() {
        let reg = vec![
            "http://127.0.0.1/callback".to_string(),
            "https://app.example/cb".to_string(),
        ];
        assert!(redirect_allowed(&reg, "http://127.0.0.1:5000/callback"));
        assert!(!redirect_allowed(&reg, "http://127.0.0.1:5000/other"));
        assert!(!redirect_allowed(&reg, "http://localhost:5000/callback"));
        assert!(!redirect_allowed(&reg, "https://app.example:8443/cb"));
        assert!(!redirect_allowed(&reg, "https://app.example/cb#x"));
        assert!(!redirect_allowed(&reg, "http://user@127.0.0.1/callback"));
        assert_eq!(
            with_query("https://a/cb?x=1", &[("code", "c")]).unwrap(),
            "https://a/cb?x=1&code=c"
        );
    }
}
