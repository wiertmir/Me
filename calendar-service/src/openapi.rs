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
        title = "Me calendar-service",
        description = "Calendars and events for the Me apps, with recurrence.\n\nEvery `/calendar/v1` operation acts for one user, identified either by that user's access token (`Authorization: Bearer`), or, for trusted backends, by the `X-Service-Secret` header together with `X-User-Id`. Errors are `{\"code\", \"message\"}`."
    ),
    tags(
        (name = "calendars", description = "The user's calendars"),
        (name = "events", description = "Events, recurring series and their occurrences, and the changes feed"),
    ),
    modifiers(&SecuritySchemes),
)]
pub struct ApiDoc;

struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, api: &mut OpenApi) {
        let c = api.components.get_or_insert_default();
        c.add_security_scheme(
            "service_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-Service-Secret",
                "Shared secret between this service and its trusted callers; send it together with `X-User-Id`.",
            ))),
        );
        c.add_security_scheme(
            "access_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "OAuth access token (ES256 JWT) issued by auth-service.",
                    ))
                    .build(),
            ),
        );
    }
}

const DOCS_HTML: &str = r#"<!doctype html>
<html>
<head>
  <title>Me calendar-service API</title>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script id="api-reference" data-url="/api/openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#;

/// `/api/openapi.json` plus the Scalar page at `/api/docs`.
pub fn routes(api: OpenApi) -> Router {
    Router::new()
        .route("/api/openapi.json", get(move || async move { Json(api) }))
        .route("/api/docs", get(|| async { Html(DOCS_HTML) }))
}
