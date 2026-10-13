//! Errors every driver reports in the same shape.

use std::fmt;

pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Broad class of failure, for how the interface reacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The server could not be reached, or the connection dropped.
    Connection,
    /// Wrong user or password.
    Authentication,
    /// TLS could not be set up or the certificate was rejected.
    Tls,
    /// The server rejected a statement.
    Query,
    /// The user cancelled the statement.
    Cancelled,
    /// No connection is open for the request.
    NotConnected,
    /// The driver does not support what was asked.
    Unsupported,
    /// The keychain or the profiles file failed.
    Storage,
    Other,
}

/// A failure, with whatever detail the database gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    pub kind: ErrorKind,
    pub message: String,
    /// The SQLSTATE or the driver's own code.
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character offset into the statement.
    pub position: Option<u32>,
}

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

    pub fn not_connected() -> Self {
        Self::new(ErrorKind::NotConnected, "Not connected")
    }

    pub fn unsupported(what: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, what)
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(code) = &self.code {
            write!(f, " ({code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for DbError {}
