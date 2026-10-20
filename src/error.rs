//! s1-specific errors. Each ingress renders them in its own wire format
//! (see [`crate::protocol::Protocol::error_body`]); the codes are the same in
//! every format.

use axum::http::StatusCode;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum S1Error {
    /// The backend queue is full (429).
    #[error("{message}")]
    QueueFull {
        message: String,
        retry_after_secs: u64,
    },
    /// Input exceeds the model's context (413).
    #[error("{0}")]
    TooLong(String),
    /// The request needs a feature the backend lacks (422).
    #[error("{0}")]
    Unsupported(String),
    /// The model belongs to another family than the endpoint (400).
    #[error("{0}")]
    WrongFamily(String),
    /// No backend can take the request now (503).
    #[error("{0}")]
    BackendUnavailable(String),
}

impl S1Error {
    /// Stable error code, the same in every wire format.
    pub fn code(&self) -> &'static str {
        match self {
            S1Error::QueueFull { .. } => "queue_full",
            S1Error::TooLong(_) => "too_long",
            S1Error::Unsupported(_) => "unsupported",
            S1Error::WrongFamily(_) => "wrong_family",
            S1Error::BackendUnavailable(_) => "backend_unavailable",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            S1Error::QueueFull { .. } => StatusCode::TOO_MANY_REQUESTS,
            S1Error::TooLong(_) => StatusCode::PAYLOAD_TOO_LARGE,
            S1Error::Unsupported(_) => StatusCode::UNPROCESSABLE_ENTITY,
            S1Error::WrongFamily(_) => StatusCode::BAD_REQUEST,
            S1Error::BackendUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// `Retry-After` value in seconds, if any.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            S1Error::QueueFull {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses_match_spec() {
        let cases = [
            (
                S1Error::QueueFull {
                    message: String::new(),
                    retry_after_secs: 1,
                },
                "queue_full",
                429,
            ),
            (S1Error::TooLong(String::new()), "too_long", 413),
            (S1Error::Unsupported(String::new()), "unsupported", 422),
            (S1Error::WrongFamily(String::new()), "wrong_family", 400),
            (
                S1Error::BackendUnavailable(String::new()),
                "backend_unavailable",
                503,
            ),
        ];
        for (err, code, status) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.status().as_u16(), status);
        }
    }
}
