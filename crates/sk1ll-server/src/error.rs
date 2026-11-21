use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sk1ll_proto::ErrorBody;

/// An API failure, rendered as `{"error": "..."}` with a matching status.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("missing or invalid credentials")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    BadRequest(String),
    /// The server can't answer right now, e.g. it can't read the baseline.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T, E = ApiError> = std::result::Result<T, E>;

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Db(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The message a caller sees; internal errors are only logged.
    pub fn message(&self) -> String {
        if matches!(self, Self::Db(_) | Self::Internal(_)) {
            tracing::error!("{self:#}");
            "internal error".to_string()
        } else {
            self.to_string()
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(ErrorBody { error: self.message() })).into_response()
    }
}
