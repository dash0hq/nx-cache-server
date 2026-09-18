use crate::domain::storage::StorageError;
use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("Bad request")]
    BadRequest,

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Forbidden")]
    Forbidden,

    #[error("Artifact too large")]
    TooLarge,

    #[error("Upload spool I/O failed: {0}")]
    UploadIo(#[from] std::io::Error),

    #[error("Internal server error")]
    InternalError,

    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            // Map domain errors to HTTP responses
            ServerError::Storage(StorageError::NotFound) => {
                (StatusCode::NOT_FOUND, "The record was not found")
            }
            ServerError::Storage(StorageError::AlreadyExists) => {
                (StatusCode::CONFLICT, "Cannot override an existing record")
            }

            // HTTP-specific errors
            ServerError::BadRequest => (StatusCode::BAD_REQUEST, "Bad request"),
            ServerError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized"),
            ServerError::Forbidden => (StatusCode::FORBIDDEN, "Forbidden"),
            ServerError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Artifact too large"),

            // Generic fallback - log details but return safe message
            _ => {
                tracing::error!("Server error: {}", self);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
            }
        };

        let mut response = (status, [("Content-Type", "text/plain")], message).into_response();
        apply_401_challenge(&mut response);

        response
    }
}

/// RFC 9110 requires a challenge on every 401. Keyed off the response status
/// rather than the `ServerError` variant, so a 401 added later carries the
/// challenge without touching this function.
fn apply_401_challenge(response: &mut Response) {
    if response.status() == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These two tests call the helper directly rather than going through a
    /// `ServerError`. That is the point: `Unauthorized` is the only variant that
    /// maps to 401 today, so a variant-keyed rewrite would pass every
    /// response-level test. Driving arbitrary statuses through the helper is
    /// what actually pins the status keying.
    #[test]
    fn any_401_carries_the_challenge() {
        let mut response = StatusCode::UNAUTHORIZED.into_response();

        apply_401_challenge(&mut response);

        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
    }

    #[test]
    fn no_other_status_carries_the_challenge() {
        for status in [
            StatusCode::OK,
            StatusCode::BAD_REQUEST,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::IM_A_TEAPOT,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let mut response = status.into_response();

            apply_401_challenge(&mut response);

            assert!(
                !response.headers().contains_key(header::WWW_AUTHENTICATE),
                "{status} must not carry a challenge"
            );
        }
    }
}
