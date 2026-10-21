//! The ports a database driver implements.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::catalog::{ColumnInfo, DbObject, EntryPage, ServerInfo};
use crate::error::Result;
use crate::profile::{ConnectionProfile, Credentials, DriverId};
use crate::query::{ExecLimits, ResultSet};
use crate::tools::{Download, Task, TaskSettings};

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
    /// What its statements are written in.
    #[serde(default)]
    pub language: Language,
    /// Whether the connection form asks for a user and a password.
    #[serde(default)]
    pub user: Login,
    #[serde(default = "Login::optional")]
    pub password: Login,
    /// What the tree calls schemas, such as `Databases`; empty for the
    /// default.
    #[serde(default)]
    pub schema_label: String,
}

/// The language a driver's statements are written in. Violet generates
/// SQL only for [`Language::Sql`]; other drivers page and count themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    #[default]
    Sql,
    Redis,
    Mongodb,
}

/// Whether the connection form asks for, and demands, a value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Login {
    #[default]
    Required,
    Optional,
    None,
}

impl Login {
    #[must_use]
    pub const fn optional() -> Self {
        Self::Optional
    }
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

    /// The backups and restores the driver runs through the database's
    /// client programs; none by default.
    async fn tasks(&self) -> Result<Vec<Task>> {
        Ok(Vec::new())
    }

    /// Client programs to download for the tasks, per platform; none by
    /// default.
    async fn downloads(&self) -> Result<Vec<Download>> {
        Ok(Vec::new())
    }

    /// Settings that suit restoring from a path, such as its format: from a
    /// file's first bytes, or a folder's file names.
    async fn inspect_input(
        &self,
        head: &[u8],
        entries: &[String],
        directory: bool,
    ) -> Result<TaskSettings> {
        let _ = (head, entries, directory);
        Ok(TaskSettings::new())
    }
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

    /// What its statements are written in.
    fn language(&self) -> Language {
        Language::Sql
    }

    /// Objects in `schema` matching the glob `pattern`, a page at a time.
    /// Only drivers that are not SQL list them this way.
    async fn entries(
        &self,
        schema: &str,
        pattern: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<EntryPage> {
        let _ = (schema, pattern, cursor, limit);
        Err(crate::DbError::unsupported(
            "Listing entries is not supported",
        ))
    }

    /// Rows `offset .. offset + limit` of a statement's first result, run in
    /// `schema`. Violet pages SQL itself, with [`Dialect::paged`].
    async fn page(
        &self,
        schema: &str,
        statement: &str,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<ResultSet>> {
        let _ = (schema, statement, offset, limit);
        Err(crate::DbError::unsupported("Paging is not supported"))
    }

    /// The rows a statement returns, `None` when it cannot tell. Violet
    /// counts SQL itself, with [`Dialect::count`].
    async fn count(&self, schema: &str, statement: &str) -> Result<Option<u64>> {
        let _ = (schema, statement);
        Err(crate::DbError::unsupported("Counting is not supported"))
    }

    /// Runs statements in `schema` as [`Session::execute`] does, leaving
    /// the state the editor set alone. Only drivers that are not SQL do.
    async fn run_in(
        &self,
        schema: &str,
        statements: &str,
        limits: ExecLimits,
    ) -> Result<Vec<ResultSet>> {
        let _ = (schema, statements, limits);
        Err(crate::DbError::unsupported(
            "Running in a schema is not supported",
        ))
    }

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

    /// Shows how the server would run a statement; with `analyze`, runs it
    /// and shows what happened. The statement follows the prefix unchanged,
    /// so error positions can be mapped back to it.
    fn explain(&self, sql: &str, analyze: bool) -> String {
        if analyze {
            format!("EXPLAIN ANALYZE {sql}")
        } else {
            format!("EXPLAIN {sql}")
        }
    }

    /// Lists the foreign keys from or to `schema`'s tables, one row per
    /// column pair, in key order: constraint schema and name, table schema,
    /// table, column, then the referenced schema, table and column. By
    /// default the standard `information_schema` query; `None` when the
    /// database has no way, or `schema` cannot be written safely as a
    /// literal.
    fn foreign_keys(&self, schema: &str) -> Option<String> {
        let schema = string_literal(schema)?;
        Some(format!(
            "SELECT kcu.constraint_schema, kcu.constraint_name, \
             kcu.table_schema, kcu.table_name, kcu.column_name, \
             ref.table_schema, ref.table_name, ref.column_name \
             FROM information_schema.referential_constraints rc \
             JOIN information_schema.key_column_usage kcu \
             ON kcu.constraint_schema = rc.constraint_schema \
             AND kcu.constraint_name = rc.constraint_name \
             JOIN information_schema.key_column_usage ref \
             ON ref.constraint_schema = rc.unique_constraint_schema \
             AND ref.constraint_name = rc.unique_constraint_name \
             AND ref.ordinal_position = kcu.position_in_unique_constraint \
             WHERE kcu.table_schema = {schema} OR ref.table_schema = {schema} \
             ORDER BY 1, 2, 3, 4, kcu.ordinal_position"
        ))
    }
}

/// `text` as a standard SQL string literal, or `None` when it holds a
/// character some databases treat specially inside one (a backslash, a NUL
/// or another control character), so it is never sent at all.
#[must_use]
pub fn string_literal(text: &str) -> Option<String> {
    if text.chars().any(|c| c == '\\' || c.is_control()) {
        return None;
    }
    Some(format!("'{}'", text.replace('\'', "''")))
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
    fn catalog_literals_cannot_break_out() {
        assert_eq!(string_literal("it's").as_deref(), Some("'it''s'"));
        // MySQL reads a backslash as an escape: such names are never sent.
        assert_eq!(string_literal(r"x\' OR 1=1 --"), None);
        assert_eq!(string_literal("a\0b"), None);
        let sql = AnsiDialect.foreign_keys("o'k").expect("query");
        assert!(sql.contains("kcu.table_schema = 'o''k' OR ref.table_schema = 'o''k'"));
        assert_eq!(AnsiDialect.explain("SELECT 1", false), "EXPLAIN SELECT 1");
        assert_eq!(
            AnsiDialect.explain("SELECT 1", true),
            "EXPLAIN ANALYZE SELECT 1"
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
