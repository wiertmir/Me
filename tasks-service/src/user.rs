use axum::{extract::FromRequestParts, http::request::Parts};
use common::{ApiError, Caller};
use uuid::Uuid;

use crate::AppState;

/// The user a request acts for.
pub struct User(pub Uuid);

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, ApiError> {
        let Caller(id) = Caller::from_request_parts(parts, s).await?;
        Ok(User(id))
    }
}
