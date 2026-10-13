//! The ports a database driver implements.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::catalog::{ColumnInfo, DbObject, ServerInfo};
use crate::error::Result;
use crate::profile::{ConnectionProfile, Credentials, DriverId};
use crate::query::{ExecLimits, ResultSet};

/// What a driver can do, so the interface hides what does not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    pub id: DriverId,
    /// Shown in the connection form, such as `PostgreSQL`.
    pub name: String,
    pub default_port: u16,
    pub default_database: String,
    pub default_user: String,
    pub capabilities: Capabilities,
    /// The SQL Violet generates for it.
    pub dialect: SqlDialect,
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

    /// A value that changes whenever data or schema may have changed, so a
    /// cached result can be checked cheaply before it is shown again.
    /// `None` when the database offers no such signal; cached results then
    /// only live as long as the cache's time limit.
    async fn change_token(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn dialect(&self) -> &dyn Dialect;

    async fn close(&self);
}

/// Cancels whatever the session that made it is running.
#[async_trait]
pub trait Canceller: Send + Sync + 'static {
    async fn cancel(&self) -> Result<()>;
}

/// A column to sort a data view by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

/// A query wrapped to read one page, and how far the original text moved so
/// error positions can be mapped back to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PagedSql {
    pub sql: String,
    /// Characters added before the original statement.
    pub shift: u32,
}

/// The SQL a driver needs generated for it.
pub trait Dialect: Send + Sync {
    /// Quotes an identifier, doubling quotes inside it.
    fn quote_ident(&self, ident: &str) -> String;

    /// Reads a relation for a data view. `filter` is a condition the user
    /// typed, as DBeaver's filter box takes it; the caller checks it is a
    /// single expression.
    fn browse(
        &self,
        schema: &str,
        relation: &str,
        filter: Option<&str>,
        order: Option<&OrderBy>,
    ) -> String {
        let mut sql = format!(
            "SELECT * FROM {}.{}",
            self.quote_ident(schema),
            self.quote_ident(relation)
        );
        if let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(&format!("\nWHERE ({filter}\n)"));
        }
        if let Some(order) = order {
            let direction = if order.descending { "DESC" } else { "ASC" };
            sql.push_str(&format!(
                "\nORDER BY {} {direction}",
                self.quote_ident(&order.column)
            ));
        }
        sql
    }

    /// Wraps a single read-only statement to read `limit` rows from
    /// `offset`.
    fn paged(&self, sql: &str, limit: u64, offset: u64) -> PagedSql {
        const PREFIX: &str = "SELECT * FROM (\n";
        PagedSql {
            sql: format!("{PREFIX}{sql}\n) AS violet_page LIMIT {limit} OFFSET {offset}"),
            shift: PREFIX.chars().count() as u32,
        }
    }

    /// Counts the rows a single read-only statement returns.
    fn count(&self, sql: &str) -> String {
        format!("SELECT count(*) FROM (\n{sql}\n) AS violet_count")
    }
}

/// How a dialect cuts a query into pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Paging {
    /// `LIMIT n OFFSET m`: PostgreSQL, MySQL, SQLite...
    #[default]
    LimitOffset,
    /// `OFFSET m ROWS FETCH NEXT n ROWS ONLY`: SQL Server, Oracle.
    OffsetFetch,
}

/// A dialect described by data, as providers declare theirs: identifier
/// quotes and a paging style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SqlDialect {
    pub quote_open: char,
    /// Doubled inside a name to escape it.
    pub quote_close: char,
    pub paging: Paging,
}

impl SqlDialect {
    /// Double quotes and `LIMIT`/`OFFSET`.
    pub const ANSI: Self = Self {
        quote_open: '"',
        quote_close: '"',
        paging: Paging::LimitOffset,
    };
}

impl Default for SqlDialect {
    fn default() -> Self {
        Self::ANSI
    }
}

impl Dialect for SqlDialect {
    fn quote_ident(&self, ident: &str) -> String {
        let close = self.quote_close.to_string();
        format!(
            "{}{}{close}",
            self.quote_open,
            ident.replace(&close, &close.repeat(2))
        )
    }

    fn paged(&self, sql: &str, limit: u64, offset: u64) -> PagedSql {
        const PREFIX: &str = "SELECT * FROM (\n";
        let window = match self.paging {
            Paging::LimitOffset => format!("LIMIT {limit} OFFSET {offset}"),
            // These need an ORDER BY to page; ordering by a constant keeps
            // the query's own order.
            Paging::OffsetFetch => {
                format!("ORDER BY (SELECT NULL) OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
            }
        };
        PagedSql {
            sql: format!("{PREFIX}{sql}\n) AS violet_page {window}"),
            shift: PREFIX.chars().count() as u32,
        }
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
            AnsiDialect.browse("public", "users", None, None),
            r#"SELECT * FROM "public"."users""#
        );
    }

    #[test]
    fn declared_dialects_quote_and_page_their_way() {
        let brackets = SqlDialect {
            quote_open: '[',
            quote_close: ']',
            paging: Paging::OffsetFetch,
        };
        assert_eq!(brackets.quote_ident("a]b"), "[a]]b]");
        assert!(
            brackets
                .paged("SELECT 1", 10, 20)
                .sql
                .ends_with("OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY")
        );
        assert_eq!(SqlDialect::ANSI.quote_ident(r#"x"y"#), r#""x""y""#);
        assert_eq!(
            SqlDialect::ANSI.paged("SELECT 1", 1, 2),
            AnsiDialect.paged("SELECT 1", 1, 2)
        );
    }

    #[test]
    fn data_views_filter_sort_and_page() {
        let order = OrderBy {
            column: "Name".into(),
            descending: true,
        };
        let sql = AnsiDialect.browse("s", "t", Some(" id > 1 "), Some(&order));
        assert_eq!(
            sql,
            "SELECT * FROM \"s\".\"t\"\nWHERE (id > 1\n)\nORDER BY \"Name\" DESC"
        );
        // A blank filter is no filter, and a trailing comment stays inside
        // its own line.
        assert!(
            !AnsiDialect
                .browse("s", "t", Some("  "), None)
                .contains("WHERE")
        );
        assert!(
            AnsiDialect
                .browse("s", "t", Some("a = 1 -- x"), None)
                .contains("-- x\n)")
        );

        let paged = AnsiDialect.paged("SELECT 1 -- c", 201, 400);
        assert_eq!(
            paged.sql,
            "SELECT * FROM (\nSELECT 1 -- c\n) AS violet_page LIMIT 201 OFFSET 400"
        );
        assert_eq!(&paged.sql[paged.shift as usize..][..8], "SELECT 1");
        assert_eq!(
            AnsiDialect.count("SELECT 1"),
            "SELECT count(*) FROM (\nSELECT 1\n) AS violet_count"
        );
    }
}
