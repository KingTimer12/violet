//! Building errors in Violet's shape.

use crate::{DbError, ErrorKind};

impl DbError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            detail: None,
            hint: None,
            position: None,
        }
    }

    /// The server could not be reached, or the connection dropped.
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Connection, message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Authentication, message)
    }

    pub fn tls(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Tls, message)
    }

    /// The server rejected a statement.
    pub fn query(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Query, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    /// The server said something the provider does not understand.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

impl From<std::io::Error> for DbError {
    fn from(error: std::io::Error) -> Self {
        // `net::Connection` wraps host errors in `io::Error`; unwrap them.
        if error.get_ref().is_none() {
            return Self::connection(error.to_string());
        }
        match error.into_inner().map(|inner| inner.downcast::<DbError>()) {
            Some(Ok(error)) => *error,
            Some(Err(other)) => Self::connection(other.to_string()),
            None => Self::connection("Connection error"),
        }
    }
}
