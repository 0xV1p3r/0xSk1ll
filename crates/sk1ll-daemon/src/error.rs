use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sk1ll_proto::ErrorBody;

/// An operation's failure, as the local API reports it.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request can't be done as asked (unknown item, nothing to commit...).
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// The machine's state forbids it (e.g. it was removed from the fleet).
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    ReviewedPlacementChanged(String),
    /// The server is unreachable.
    #[error("{0}")]
    Unavailable(String),
    #[error("{1}")]
    Upstream(StatusCode, String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<sk1ll_core::Error> for Error {
    fn from(e: sk1ll_core::Error) -> Self {
        match e {
            sk1ll_core::Error::Displaced { dest, kept, id } => Self::Conflict(format!(
                "{} was swapped; concurrent edits recovered at {} (recovery id {id})",
                dest.display(),
                kept.display()
            )),
            sk1ll_core::Error::Invalid(m) => Self::BadRequest(m),
            sk1ll_core::Error::Changed(m) => Self::Conflict(m),
            e => Self::Internal(e.into()),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) | Self::ReviewedPlacementChanged(_) => StatusCode::CONFLICT,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream(status, _) => *status,
            Self::Internal(e) => {
                tracing::error!("{e:#}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let error = match &self {
            Self::Internal(e) => format!("{e:#}"),
            e => e.to_string(),
        };
        (status, Json(ErrorBody { error })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displaced_is_a_conflict_with_recovery_path() {
        let error = Error::from(sk1ll_core::Error::Displaced {
            dest: "/harness/x".into(),
            kept: "/recovery/entry/content".into(),
            id: "entry".into(),
        });
        assert!(error.to_string().contains("/recovery/entry/content"));
        assert_eq!(error.into_response().status(), StatusCode::CONFLICT);
    }
}
