use axum::{extract::FromRequestParts, http::request::Parts};
use common::{ApiError, Caller};
use uuid::Uuid;

use crate::{AppState, recurrence};

/// The user a request acts for. Extracting it first creates whatever is due in the user's recurring chains.
pub struct User(pub Uuid);

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, ApiError> {
        let Caller(id) = Caller::from_request_parts(parts, s).await?;
        s.db.with(|c| {
            let tx = c.unchecked_transaction()?;
            recurrence::catch_up(&tx, id, s.now())?;
            tx.commit()
        })?;
        Ok(User(id))
    }
}
