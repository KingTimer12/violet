//! A connection to the profile's database file.

use violet_sdk::files;
use violet_sdk::prelude::*;

use crate::db::{Db, Value};

/// The profile option that opens the database read-only.
pub const READ_ONLY: &str = "read_only";

pub struct LiteSession {
    db: Db,
    /// The file's name, as the user knows the database.
    name: String,
}

impl LiteSession {
    pub fn connect(params: &ConnectParams) -> Result<Self> {
        let read_only = params.option(READ_ONLY) == Some("true");
        let wal = in_wal_mode()?;
        let db = Db::open(read_only)?;
        if wal {
            // No shared memory in the sandbox: SQLite keeps the WAL index
            // itself, and holds the database while connected.
            log::info!("the database is in WAL mode: opening it exclusively");
            db.exec("PRAGMA locking_mode = EXCLUSIVE")?;
        }
        // Reads the header: fails here, not on the first query, for a file
        // that is not a database.
        db.exec("SELECT count(*) FROM sqlite_schema")
            .map_err(|error| {
                if error.code.as_deref() == Some("26") {
                    DbError::connection(format!("{} is not a SQLite database", params.host))
                } else {
                    error
                }
            })?;
        // The host's path, which may be a Windows one: WASI's `Path` only
        // knows `/`.
        let name = params
            .host
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&params.host)
            .to_owned();
        Ok(Self { db, name })
    }

    /// The names of the attached databases: `main`, then any `ATTACH`ed,
    /// and `temp` while it holds anything.
    fn database_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for row in self
            .db
            .rows("SELECT name FROM pragma_database_list ORDER BY seq", &[])?
        {
            let Some(Some(name)) = row.into_iter().next() else {
                continue;
            };
            if name == "temp" {
                let used = self
                    .db
                    .rows("SELECT count(*) FROM temp.sqlite_schema", &[])?
                    .first()
                    .and_then(|row| row.first().cloned().flatten())
                    .is_some_and(|count| count != "0");
                if !used {
                    continue;
                }
            }
            names.push(name);
        }
        Ok(names)
    }

    /// `schema` quoted, once checked to be attached.
    fn schema(&self, schema: &str) -> Result<String> {
        if !self.database_names()?.iter().any(|name| name == schema) {
            return Err(DbError::query(format!("No database named {schema}")));
        }
        Ok(quote(schema))
    }
}

/// Whether the file's header says WAL (read and write versions 2).
fn in_wal_mode() -> Result<bool> {
    if !files::exists("")? {
        return Ok(false);
    }
    let header = files::open("", false, true)?.read(0, 20)?;
    Ok(header.len() == 20 && (header[18] == 2 || header[19] == 2))
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Session for LiteSession {
    fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo {
            version: format!("SQLite {}", Db::version()),
            database: self.name.clone(),
            user: String::new(),
        })
    }

    fn schemas(&self) -> Result<Vec<String>> {
        self.database_names()
    }

    fn objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_schema \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY name",
            self.schema(schema)?
        );
        Ok(self
            .db
            .rows(&sql, &[])?
            .into_iter()
            .filter_map(|row| {
                let mut row = row.into_iter();
                let name = row.next().flatten()?;
                let kind = match row.next().flatten().as_deref() {
                    Some("view") => ObjectKind::View,
                    _ => ObjectKind::Table,
                };
                Some(DbObject {
                    name,
                    kind,
                    comment: None,
                    estimated_rows: None,
                })
            })
            .collect())
    }

    fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        self.schema(schema)?;
        // Hidden columns of virtual tables (1) are left out; generated ones
        // (2, 3) are shown.
        let rows = self.db.rows(
            "SELECT name, type, \"notnull\", dflt_value, pk \
             FROM pragma_table_xinfo(?1, ?2) WHERE hidden <> 1 ORDER BY cid",
            &[relation, schema],
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let mut row = row.into_iter();
                let name = row.next().flatten()?;
                let data_type = row.next().flatten().unwrap_or_default();
                let not_null = row.next().flatten().as_deref() == Some("1");
                let default = row.next().flatten();
                let primary_key = row.next().flatten().is_some_and(|pk| pk != "0");
                Some(ColumnInfo {
                    name,
                    data_type,
                    nullable: !not_null && !primary_key,
                    default,
                    primary_key,
                    comment: None,
                })
            })
            .collect())
    }

    fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        let mut sets = Vec::new();
        let mut start = 0;
        while start < sql.len() {
            let rest = &sql[start..];
            let (stmt, used) = self
                .db
                .prepare(rest)
                .map_err(|(rc, _)| self.db.error(rc, Some((sql, start))))?;
            let text = &rest[..used];
            let Some(stmt) = stmt else {
                // Only space, comments or an empty `;`.
                if used == 0 {
                    break;
                }
                start += used;
                continue;
            };
            start += used.max(1);
            sets.push(run(&stmt, text, max_rows).map_err(|rc| self.db.error(rc, None))?);
        }
        Ok(sets)
    }

    fn change_token(&self) -> Result<Option<String>> {
        // `data_version` moves when another connection commits, the change
        // count when this one writes, `schema_version` with the schema.
        Ok(self
            .db
            .rows(
                "SELECT (SELECT data_version FROM pragma_data_version) || '/' || \
                 total_changes() || '/' || \
                 (SELECT schema_version FROM pragma_schema_version)",
                &[],
            )?
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten()))
    }
}

/// Runs one prepared statement to the end, keeping at most `max_rows`.
fn run(stmt: &crate::db::Stmt<'_>, text: &str, max_rows: usize) -> Result<ResultSet, i32> {
    let width = stmt.column_count();
    let command = command(text);
    if width == 0 {
        while stmt.step()? {}
        let affected = matches!(command.as_str(), "INSERT" | "UPDATE" | "DELETE" | "REPLACE")
            .then(|| stmt.db().changes());
        return Ok(ResultSet::command(command, affected));
    }
    let declared: Vec<Option<String>> = (0..width).map(|c| stmt.declared_type(c)).collect();
    let mut numeric: Vec<Option<bool>> = declared
        .iter()
        .map(|kind| kind.as_deref().map(numeric_affinity))
        .collect();
    let mut rows = RowsBuilder::new(width, max_rows);
    let mut cells: Vec<Option<String>> = Vec::with_capacity(width);
    while stmt.step()? {
        if rows.is_full() {
            // Marks the result truncated; the rest is not read.
            rows.push([]).map_err(|_| libsqlite3_sys::SQLITE_TOOBIG)?;
            break;
        }
        cells.clear();
        for (column, numeric) in numeric.iter_mut().enumerate() {
            let value = stmt.value(column);
            // Expressions have no declared type: the first value tells.
            if numeric.is_none() && !matches!(value, Value::Null) {
                *numeric = Some(matches!(value, Value::Integer(_) | Value::Real(_)));
            }
            cells.push(value.display());
        }
        rows.push(cells.iter().map(Option::as_deref))
            .map_err(|_| libsqlite3_sys::SQLITE_TOOBIG)?;
    }
    let columns = (0..width)
        .map(|column| {
            ColumnMeta::new(
                stmt.column_name(column),
                declared[column].clone(),
                numeric[column].unwrap_or(false),
            )
        })
        .collect();
    let mut set = ResultSet::query(columns, rows);
    if !stmt.read_only() {
        // `INSERT ... RETURNING` and friends.
        set.affected = Some(stmt.db().changes());
    }
    set.command = Some(command);
    Ok(set)
}

/// Whether a declared type has numeric affinity, by SQLite's rules (and
/// `DECIMAL`, `NUMERIC`, `BOOLEAN`, dates aside).
fn numeric_affinity(declared: &str) -> bool {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        return true;
    }
    if ["CHAR", "CLOB", "TEXT", "BLOB"]
        .iter()
        .any(|t| upper.contains(t))
        || upper.is_empty()
    {
        return false;
    }
    ["REAL", "FLOA", "DOUB", "NUM", "DEC"]
        .iter()
        .any(|t| upper.contains(t))
}

/// The statement's first keyword, upper-cased: its command tag.
fn command(text: &str) -> String {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(line) = rest.strip_prefix("--") {
            rest = line.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(block) = rest.strip_prefix("/*") {
            rest = block.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_skip_comments() {
        assert_eq!(command("  select 1"), "SELECT");
        assert_eq!(
            command("-- note\n/* x */ insert into t values (1);"),
            "INSERT"
        );
        assert_eq!(command("/* only */"), "");
    }

    #[test]
    fn affinity_follows_sqlite() {
        for numeric in [
            "INTEGER",
            "bigint",
            "REAL",
            "double precision",
            "NUMERIC(10,2)",
            "decimal",
        ] {
            assert!(numeric_affinity(numeric), "{numeric}");
        }
        for text in ["TEXT", "varchar(20)", "BLOB", ""] {
            assert!(!numeric_affinity(text), "{text}");
        }
    }

    #[test]
    fn names_are_quoted() {
        assert_eq!(quote("main"), "\"main\"");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
    }
}
