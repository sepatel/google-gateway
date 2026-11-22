use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0} must be linked again: its Google authorization was revoked or expired")]
    Relink(String),
    #[error("Gmail returned {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("{0:#}")]
    Internal(#[from] anyhow::Error),
}

impl From<reqwest::Error> for GatewayError {
    fn from(e: reqwest::Error) -> Self {
        Self::Internal(anyhow::anyhow!("request to Google failed: {e}"))
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Relink(_) => StatusCode::CONFLICT,
            Self::Upstream { .. } => StatusCode::BAD_GATEWAY,
            Self::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.to_string()).into_response()
    }
}

pub type GatewayResult<T> = Result<T, GatewayError>;
