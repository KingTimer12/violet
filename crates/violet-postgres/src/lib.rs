//! PostgreSQL for Violet, on `tokio-postgres` with TLS through rustls.
//!
//! Results come back through the simple query protocol, in the server's
//! text form, so every type renders (including extensions' and arrays),
//! several statements run in one go, and values are never re-encoded. A
//! single statement is also prepared first, without running it, to learn
//! its column types.

mod catalog;
mod error;
mod session;
mod tls;

use async_trait::async_trait;
use violet_core::{
    Capabilities, ConnectionProfile, Credentials, Driver, DriverId, DriverInfo, Result, Session,
};

pub use session::PgSession;

/// The id profiles name this driver by.
pub const DRIVER_ID: &str = "postgres";

/// Opens PostgreSQL sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: DriverId::new(DRIVER_ID),
            name: "PostgreSQL",
            default_port: 5432,
            default_database: "postgres",
            default_user: "postgres",
            capabilities: Capabilities {
                databases: true,
                schemas: true,
                cancel: true,
                tls: true,
            },
        }
    }

    async fn connect(
        &self,
        profile: &ConnectionProfile,
        credentials: &Credentials,
    ) -> Result<Box<dyn Session>> {
        Ok(Box::new(PgSession::connect(profile, credentials).await?))
    }
}
