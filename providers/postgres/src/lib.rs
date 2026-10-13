//! PostgreSQL for Violet, as a WebAssembly provider.
//!
//! Speaks the wire protocol itself, through `postgres-protocol`, over the
//! socket Violet hands it; TLS is negotiated with `SSLRequest` and done by
//! Violet. Authentication: SCRAM-SHA-256 (with the server's proof
//! checked), MD5 and cleartext passwords.

mod catalog;
mod session;
mod types;
mod wire;

use violet_sdk::{Capabilities, ConnectParams, DbError, Provider, ProviderInfo, Result};

pub use session::PgSession;
use wire::{BackendKey, Wire};

pub struct Postgres;

impl Provider for Postgres {
    type Session = PgSession;

    fn info() -> ProviderInfo {
        ProviderInfo {
            description: "PostgreSQL 10 and later, and compatible servers".into(),
            default_database: "postgres".into(),
            default_user: "postgres".into(),
            capabilities: Capabilities {
                databases: true,
                schemas: true,
                cancel: true,
                tls: true,
            },
            ..ProviderInfo::new("postgres", "PostgreSQL", 5432)
        }
    }

    fn connect(params: ConnectParams) -> Result<PgSession> {
        PgSession::connect(&params)
    }

    /// PostgreSQL cancels over a new connection, sending the backend key
    /// it gave the session.
    fn cancel(params: ConnectParams, ticket: Vec<u8>) -> Result<()> {
        let key = BackendKey::from_ticket(&ticket)
            .ok_or_else(|| DbError::protocol("Malformed cancel ticket"))?;
        Wire::open(&params)?.cancel(key)
    }
}

violet_sdk::export_provider!(Postgres);
