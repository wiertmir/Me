use axum::{Json, Router, response::Html, routing::get};
use utoipa::{
    Modify,
    openapi::{
        OpenApi,
        security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
    },
};

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Me auth-service",
        description = "Authentication for the Me apps: password and social sign-in, accounts, OAuth 2 / OpenID Connect and app passwords.\n\nEvery `/api/*` operation needs the `X-Service-Secret` header and is meant to be called by trusted backends only, never by browsers. Operations that act for a signed-in user also need that user's session token as a bearer token. Errors on `/api/*` are `{\"code\", \"message\"}`; the OAuth endpoints use the RFC 6749 shape `{\"error\", \"error_description\"}`."
    ),
    tags(
        (name = "auth", description = "Sign-in, sign-out, sign-up, email verification and password reset"),
        (name = "account", description = "The signed-in user's profile, sessions and linked identities"),
        (name = "admin", description = "User administration (admins only)"),
        (name = "oauth", description = "OAuth 2 authorization code flow with PKCE, OpenID Connect discovery and keys"),
        (name = "social", description = "Sign-in and linking through Google, GitHub and Microsoft"),
        (name = "app-passwords", description = "Per-device passwords for clients that cannot do OAuth, such as CalDAV"),
    ),
    modifiers(&SecuritySchemes),
)]
pub struct ApiDoc;

struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, api: &mut OpenApi) {
        let bearer = |format: Option<&str>, description: &str| {
            let mut b = HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .description(Some(description));
            if let Some(f) = format {
                b = b.bearer_format(f);
            }
            SecurityScheme::Http(b.build())
        };
        let c = api.components.get_or_insert_default();
        c.add_security_scheme(
            "service_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-Service-Secret",
                "Shared secret between this service and its trusted callers.",
            ))),
        );
        c.add_security_scheme(
            "session",
            bearer(
                None,
                "Opaque session token from `POST /api/signin` or `POST /api/social/exchange`.",
            ),
        );
        c.add_security_scheme(
            "access_token",
            bearer(
                Some("JWT"),
                "OAuth access token (ES256 JWT) from `POST /oauth/token`.",
            ),
        );
    }
}

const DOCS_HTML: &str = r#"<!doctype html>
<html>
<head>
  <title>Me auth-service API</title>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script id="api-reference" data-url="/api/openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#;

/// `/api/openapi.json` plus the Scalar page at `/api/docs`. Neither needs the service secret.
pub fn routes(api: OpenApi) -> Router {
    Router::new()
        .route("/api/openapi.json", get(move || async move { Json(api) }))
        .route("/api/docs", get(|| async { Html(DOCS_HTML) }))
}
