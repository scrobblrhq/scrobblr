use aide::OperationOutput;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("missing or invalid Authorization header")]
    Unauthorized,

    #[error("invalid credentials")]
    InvalidCredentials,

    #[error("you do not have permission to do this")]
    Forbidden,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("scrobble validation failed: {0}")]
    ScrobbleInvalid(String),

    #[error("not found")]
    NotFound,

    #[error("username already taken")]
    UsernameTaken,

    #[error("email already registered")]
    EmailTaken,

    #[error("too many requests")]
    RateLimited,

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("redis error: {0}")]
    Redis(#[from] fred::error::Error),

    #[error("internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            AppError::InvalidCredentials => (StatusCode::UNAUTHORIZED, self.to_string()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, self.to_string()),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::ScrobbleInvalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::UsernameTaken => (StatusCode::CONFLICT, self.to_string()),
            AppError::EmailTaken => (StatusCode::CONFLICT, self.to_string()),
            AppError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, self.to_string()),
            AppError::Database(e) => {
                tracing::error!("database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
            AppError::Redis(e) => {
                tracing::error!("redis error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
            AppError::Internal(e) => {
                tracing::error!("internal error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl OperationOutput for AppError {
    type Inner = Self;

    fn operation_response(
        _ctx: &mut aide::generate::GenContext,
        _operation: &mut aide::openapi::Operation,
    ) -> Option<aide::openapi::Response> {
        Some(aide::openapi::Response {
            description: Some(
                "Error response of the API (JSON with format { error: string })".to_string(),
            )?,
            ..Default::default()
        })
    }

    fn inferred_responses(
        ctx: &mut aide::generate::GenContext,
        operation: &mut aide::openapi::Operation,
    ) -> Vec<(Option<u16>, aide::openapi::Response)> {
        vec![(None, Self::operation_response(ctx, operation).unwrap())]
    }
}

pub type ApiResult<T> = Result<T, AppError>;
