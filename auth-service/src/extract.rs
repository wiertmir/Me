//! Extractors whose rejections use the JSON error shape instead of axum's plain text.
use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Path, Request},
    http::{StatusCode, request::Parts},
};
use common::ApiError;
use serde::de::DeserializeOwned;
use uuid::Uuid;

/// `Json<T>` for `/api/*` request bodies. Malformed JSON, wrong field types, missing fields and a wrong
/// content type are all 422 `validation`; the message never echoes the body.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| Self(v))
            .map_err(|_| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation",
                    "request body is missing, not JSON, or does not match the expected fields",
                )
            })
    }
}

/// The single UUID path segment of a route. Anything that is not a UUID cannot name a resource: 404 `not_found`.
pub struct PathId(pub Uuid);

impl<S: Send + Sync> FromRequestParts<S> for PathId {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let not_found = || ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such resource");
        let Path(raw) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| not_found())?;
        Uuid::parse_str(&raw).map(Self).map_err(|_| not_found())
    }
}
