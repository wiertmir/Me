//! OAuth 2 / OIDC back half: token, userinfo, revoke.
use axum::{
    Form, Json,
    extract::{State, rejection::FormRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::oauth::find_client;
use crate::{
    AppState,
    oauth_store::{self, Redeem, Rotation},
    tokens::{ACCESS_TOKEN_SECS, display_name},
    users,
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(token))
        .routes(routes!(userinfo))
        .routes(routes!(revoke))
}

/// RFC 6749 error response, used by the OAuth endpoints instead of the `/api` error shape.
#[derive(Serialize, ToSchema)]
struct OAuthError {
    /// `invalid_request`, `invalid_client`, `invalid_grant`, `unsupported_grant_type` or `server_error`.
    error: &'static str,
    /// Human-readable detail.
    error_description: String,
}

fn oauth_err(error: &'static str, desc: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        no_cache(),
        Json(OAuthError {
            error,
            error_description: desc.into(),
        }),
    )
        .into_response()
}

fn internal() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        no_cache(),
        Json(OAuthError {
            error: "server_error",
            error_description: "internal error".into(),
        }),
    )
        .into_response()
}

fn no_cache() -> [(header::HeaderName, &'static str); 2] {
    [
        (header::CACHE_CONTROL, "no-store"),
        (header::PRAGMA, "no-cache"),
    ]
}

#[derive(Deserialize, ToSchema)]
struct TokenRequest {
    /// `authorization_code` or `refresh_token`.
    grant_type: Option<String>,
    client_id: Option<String>,
    /// Authorization code (grant `authorization_code`).
    code: Option<String>,
    /// Must equal the `redirect_uri` of the authorization request (grant `authorization_code`).
    redirect_uri: Option<String>,
    /// PKCE verifier, 43-128 unreserved characters (grant `authorization_code`).
    code_verifier: Option<String>,
    /// Refresh token (grant `refresh_token`).
    refresh_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct TokenResponse {
    /// ES256 JWT, valid for `expires_in` seconds.
    access_token: String,
    /// Always `Bearer`.
    token_type: &'static str,
    /// Access token lifetime in seconds.
    expires_in: i64,
    /// Single use: each refresh returns a new one.
    refresh_token: String,
    /// Granted scopes, space separated.
    scope: String,
    /// Present for the `authorization_code` grant when `openid` was granted.
    #[serde(skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
}

fn has_scope(scope: &str, s: &str) -> bool {
    scope.split(' ').any(|x| x == s)
}

fn valid_verifier(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// Exchange a code or refresh token for tokens
///
/// Public clients, form-encoded: `authorization_code` with PKCE, and `refresh_token` with rotation.
/// Errors follow RFC 6749 and are never in the `/api` shape. Public; no service secret.
#[utoipa::path(
    post, path = "/oauth/token",
    tag = "oauth",
    request_body(
        content = TokenRequest,
        content_type = "application/x-www-form-urlencoded",
        example = json!({"grant_type": "authorization_code", "client_id": "my-client", "code": "k3T9v2Hq0sZb1m8Y4pJx7WcD5nRfA6eLgUoQiVtXyBs", "redirect_uri": "http://127.0.0.1:5000/callback", "code_verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"})
    ),
    security(()),
    responses(
        (status = 200, description = "tokens issued", body = TokenResponse),
        (status = 400, description = "RFC 6749 error: `invalid_request` (also a malformed form body), `invalid_client`, `invalid_grant`, `unsupported_grant_type`", body = OAuthError),
        (status = 500, description = "`server_error`", body = OAuthError),
    )
)]
async fn token(
    State(s): State<AppState>,
    form: Result<Form<TokenRequest>, FormRejection>,
) -> Response {
    let Ok(Form(r)) = form else {
        return oauth_err("invalid_request", "malformed form body");
    };
    let Some(client_id) = r.client_id.as_deref() else {
        return oauth_err("invalid_request", "client_id is required");
    };
    if find_client(&s, client_id).is_none() {
        return oauth_err("invalid_client", "unknown client");
    }
    let (user_id, scope, nonce, refresh_token, grant) = match r.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(redirect_uri), Some(verifier)) = (
                r.code.as_deref(),
                r.redirect_uri.as_deref(),
                r.code_verifier.as_deref(),
            ) else {
                return oauth_err(
                    "invalid_request",
                    "code, redirect_uri and code_verifier are required",
                );
            };
            if !valid_verifier(verifier) {
                return oauth_err(
                    "invalid_request",
                    "code_verifier must be 43-128 unreserved characters",
                );
            }
            let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let redeemed = oauth_store::redeem_code(&s.db, code, |req| {
                // Evaluate all three so timing does not reveal which one failed.
                let pkce: bool = computed
                    .as_bytes()
                    .ct_eq(req.code_challenge.as_bytes())
                    .into();
                let client = req.client_id == client_id;
                let uri = req.redirect_uri == redirect_uri;
                pkce & client & uri
            });
            match redeemed {
                Err(_) => return internal(),
                Ok(Redeem::Invalid) => {
                    return oauth_err(
                        "invalid_grant",
                        "invalid, expired or mismatched authorization code",
                    );
                }
                Ok(Redeem::Replay { user_id, client_id }) => {
                    tracing::warn!(event = "auth_code_replay_detected", user_id = %user_id, client_id = %client_id);
                    return oauth_err(
                        "invalid_grant",
                        "invalid, expired or mismatched authorization code",
                    );
                }
                Ok(Redeem::Ok(g)) => (
                    g.user_id,
                    g.scope,
                    g.nonce,
                    g.refresh_token,
                    "authorization_code",
                ),
            }
        }
        Some("refresh_token") => {
            let Some(rt) = r.refresh_token.as_deref() else {
                return oauth_err("invalid_request", "refresh_token is required");
            };
            match oauth_store::rotate_refresh(&s.db, rt, client_id) {
                Err(_) => return internal(),
                Ok(Rotation::Invalid) => {
                    return oauth_err("invalid_grant", "invalid or expired refresh token");
                }
                Ok(Rotation::Reuse { user_id }) => {
                    tracing::warn!(event = "refresh_reuse_detected", user_id = %user_id, client_id = %client_id);
                    return oauth_err("invalid_grant", "invalid or expired refresh token");
                }
                Ok(Rotation::Ok(g)) => (g.user_id, g.scope, None, g.refresh_token, "refresh_token"),
            }
        }
        _ => {
            return oauth_err(
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            );
        }
    };
    let Ok(user) = users::get(&s.db, user_id) else {
        return internal();
    };
    let id_token = (grant == "authorization_code" && has_scope(&scope, "openid")).then(|| {
        s.signer
            .id_token(&s.cfg, &user, client_id, nonce.as_deref())
    });
    tracing::info!(event = "token_issued", user_id = %user.id, client_id = %client_id, grant = grant);
    let body = TokenResponse {
        access_token: s.signer.access_token(&s.cfg, &user, &scope, client_id),
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_SECS,
        refresh_token,
        scope,
        id_token,
    };
    (StatusCode::OK, no_cache(), Json(body)).into_response()
}

fn invalid_token(has_token: bool) -> Response {
    let challenge = if has_token {
        r#"Bearer error="invalid_token""#
    } else {
        "Bearer"
    };
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(challenge),
        )],
    )
        .into_response()
}

/// OpenID Connect UserInfo
///
/// Claims about the user behind a bearer access token: `sub`, `preferred_username`, `name`, and with the
/// `email` scope `email` and `email_verified`. No service secret; the access token is the credential.
#[utoipa::path(
    get, path = "/oauth/userinfo",
    tag = "oauth",
    security(("access_token" = [])),
    responses(
        (status = 200, description = "the claims", body = serde_json::Value),
        (status = 401, description = "missing, invalid or expired access token; see the `WWW-Authenticate` header (`invalid_token`)"),
    )
)]
async fn userinfo(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return invalid_token(false);
    };
    let Some(claims) = s.signer.verify_access(&s.cfg, token) else {
        return invalid_token(true);
    };
    let Some(user) = Uuid::parse_str(&claims.sub)
        .ok()
        .and_then(|id| users::get(&s.db, id).ok())
        .filter(|u| !u.disabled)
    else {
        return invalid_token(true);
    };
    let mut body =
        json!({"sub": user.id, "preferred_username": user.username, "name": display_name(&user)});
    if has_scope(&claims.scope, "email") {
        body["email"] = user.email.clone().into();
        body["email_verified"] = user.email_verified.into();
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}

#[derive(Deserialize, ToSchema)]
struct RevokeRequest {
    /// A refresh token; its whole family is revoked.
    token: Option<String>,
    client_id: Option<String>,
}

/// Revoke a refresh token
///
/// RFC 7009 revocation of a refresh token's whole family. Always 200, so it does not reveal whether the
/// token existed. Public; no service secret.
#[utoipa::path(
    post, path = "/oauth/revoke",
    tag = "oauth",
    request_body(
        content = RevokeRequest,
        content_type = "application/x-www-form-urlencoded",
        example = json!({"token": "k3T9v2Hq0sZb1m8Y4pJx7WcD5nRfA6eLgUoQiVtXyBs", "client_id": "my-client"})
    ),
    security(()),
    responses((status = 200, description = "accepted (or the token was unknown)"))
)]
async fn revoke(
    State(s): State<AppState>,
    form: Result<Form<RevokeRequest>, FormRejection>,
) -> Response {
    // RFC 7009: the answer never reveals whether the token existed; failures are already logged by Db.
    if let Ok(Form(RevokeRequest {
        token: Some(t),
        client_id,
    })) = form
    {
        let _ = oauth_store::revoke_family(&s.db, &t, client_id.as_deref());
    }
    (StatusCode::OK, no_cache()).into_response()
}
