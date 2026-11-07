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
    /// Well-formed but not applicable, e.g. a toggle for an unknown item.
    #[error("{0}")]
    Unprocessable(String),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T, E = ApiError> = std::result::Result<T, E>;

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Db(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let error = if status.is_server_error() {
            tracing::error!("{self:#}");
            "internal error".to_string()
        } else {
            self.to_string()
        };
        (status, Json(ErrorBody { error })).into_response()
    }
}
