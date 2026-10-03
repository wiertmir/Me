use axum::{Json, Router, routing::get};
use utoipa::openapi::OpenApi;
use utoipa_scalar::{Scalar, Servable};

/// `/api/openapi.json` plus the Scalar page at `/api/docs`. Neither needs the service secret.
pub fn routes(api: OpenApi) -> Router {
    let json = api.clone();
    Router::new()
        .route("/api/openapi.json", get(move || async move { Json(json) }))
        .merge(Scalar::with_url("/api/docs", api))
}
