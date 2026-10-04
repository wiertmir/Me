use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

/// JSON error shape shared by all services: `{"code": "...", "message": "..."}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

/// Wire form of [`ApiError`], for OpenAPI documents.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // What the caller is told, so that a refusal can be explained from the log alone. Messages are
        // written by the services and never repeat secrets or event texts.
        tracing::warn!(status = self.status.as_u16(), code = self.code, message = %self.message, "request refused");
        let body = serde_json::json!({ "code": self.code, "message": self.message });
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn renders_status_and_json_body() {
        let r = ApiError::new(StatusCode::CONFLICT, "taken", "name taken").into_response();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"code": "taken", "message": "name taken"})
        );
    }
}
