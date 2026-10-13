//! The ports a database driver implements.

use async_trait::async_trait;

use crate::catalog::{ColumnInfo, DbObject, ServerInfo};
use crate::error::Result;
use crate::profile::{ConnectionProfile, Credentials, DriverId};
use crate::query::{ExecLimits, ResultSet};

/// What a driver can do, so the interface hides what does not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    /// The server hosts several databases (listed but opened separately).
    pub databases: bool,
    /// Objects are grouped in schemas.
    pub schemas: bool,
    /// Running statements can be cancelled.
    pub cancel: bool,
    /// The connection form shows TLS settings.
    pub tls: bool,
}

/// A driver's description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverInfo {
    pub id: DriverId,
    /// Shown in the connection form, such as `PostgreSQL`.
    pub name: &'static str,
    pub default_port: u16,
    pub default_database: &'static str,
    pub default_user: &'static str,
    pub capabilities: Capabilities,
}

/// Opens sessions to one kind of database.
#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn info(&self) -> DriverInfo;

    /// Opens a session. Runs inside the application's async runtime.
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        credentials: &Credentials,
    ) -> Result<Box<dyn Session>>;
}

/// An open connection.
///
/// Methods take `&self`: a driver serialises access to its connection
/// itself, so the catalog can load while a query runs when the database
/// allows it.
#[async_trait]
pub trait Session: Send + Sync + 'static {
    async fn server_info(&self) -> Result<ServerInfo>;

    /// Other databases on the same server, when the driver lists them.
    async fn databases(&self) -> Result<Vec<String>>;

    async fn schemas(&self) -> Result<Vec<String>>;

    async fn objects(&self, schema: &str) -> Result<Vec<DbObject>>;

    async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>>;

    /// Runs one or more statements and returns one set per statement.
    async fn execute(&self, sql: &str, limits: ExecLimits) -> Result<Vec<ResultSet>>;

    /// A handle that cancels the statement running now, from another task.
    fn canceller(&self) -> Option<Box<dyn Canceller>>;

    fn dialect(&self) -> &dyn Dialect;

    async fn close(&self);
}

/// Cancels whatever the session that made it is running.
#[async_trait]
pub trait Canceller: Send + Sync + 'static {
    async fn cancel(&self) -> Result<()>;
}

/// The SQL a driver needs generated for it.
pub trait Dialect: Send + Sync {
    /// Quotes an identifier, doubling quotes inside it.
    fn quote_ident(&self, ident: &str) -> String;

    /// A statement that reads the first `limit` rows of a relation.
    fn select_rows(&self, schema: &str, relation: &str, limit: usize) -> String {
        format!(
            "SELECT * FROM {}.{} LIMIT {limit}",
            self.quote_ident(schema),
            self.quote_ident(relation)
        )
    }
}

/// Standard SQL quoting with double quotes.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnsiDialect;

impl Dialect for AnsiDialect {
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_cannot_break_out_of_their_quotes() {
        assert_eq!(
            AnsiDialect.quote_ident(r#"a"; drop table x; --"#),
            r#""a""; drop table x; --""#
        );
        assert_eq!(
            AnsiDialect.select_rows("public", "users", 200),
            r#"SELECT * FROM "public"."users" LIMIT 200"#
        );
    }
}
