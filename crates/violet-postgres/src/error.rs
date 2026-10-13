//! `tokio-postgres` errors in Violet's shape.

use std::error::Error as _;

use tokio_postgres::error::{ErrorPosition, SqlState};
use violet_core::{DbError, ErrorKind};

pub(crate) fn map(error: &tokio_postgres::Error) -> DbError {
    if let Some(db) = error.as_db_error() {
        let code = db.code();
        let kind = if *code == SqlState::QUERY_CANCELED {
            ErrorKind::Cancelled
        } else if *code == SqlState::INVALID_PASSWORD
            || *code == SqlState::INVALID_AUTHORIZATION_SPECIFICATION
        {
            ErrorKind::Authentication
        } else {
            ErrorKind::Query
        };
        return DbError {
            kind,
            message: db.message().to_owned(),
            code: Some(code.code().to_owned()),
            detail: db.detail().map(str::to_owned),
            hint: db.hint().map(str::to_owned),
            position: match db.position() {
                Some(ErrorPosition::Original(position)) => Some(*position),
                _ => None,
            },
        };
    }

    let mut message = error.to_string();
    // The source holds the useful part ("Connection refused") on some
    // errors whose own text is only the category.
    if let Some(source) = error.source() {
        let source = source.to_string();
        if !message.contains(&source) {
            message = format!("{message}: {source}");
        }
    }
    let lower = message.to_lowercase();
    let kind = if lower.contains("tls") || lower.contains("certificate") {
        ErrorKind::Tls
    } else if lower.contains("password") {
        ErrorKind::Authentication
    } else {
        ErrorKind::Connection
    };
    DbError::new(kind, message)
}
