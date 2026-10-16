//! Opening the source of a function, procedure or view in an editor.
//!
//! The query that reads it depends on the database: PostgreSQL rebuilds it
//! with `pg_get_functiondef` / `pg_get_viewdef`, MySQL answers `SHOW
//! CREATE`, and anything else is asked through `information_schema`.

use violet_app::{CachePolicy, ExecOptions};
use violet_core::{
    ConnectionId, Dialect as _, ObjectKind, QueryOutput, Result, SqlDialect, string_literal,
};

use super::VioletApp;
use super::action::Action;
use super::toasts::Toast;
use super::workspace::TabKind;

/// A definition being read.
pub struct PendingDefinition {
    connection: ConnectionId,
    /// The editor's title, which also names the object in messages.
    title: String,
    /// Tried when the first query fails: MySQL keeps functions and
    /// procedures apart, and the tree does not say which a routine is.
    fallback: Option<String>,
    /// Appended to the text read, such as the `;` PostgreSQL leaves out.
    suffix: &'static str,
}

/// The database family a driver speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    Postgres,
    MySql,
    Other,
}

impl Family {
    pub(super) fn of(driver: &str) -> Self {
        let driver = driver.to_ascii_lowercase();
        if driver.contains("postgres") {
            Self::Postgres
        } else if driver.contains("mysql") || driver.contains("maria") {
            Self::MySql
        } else {
            Self::Other
        }
    }
}

/// The query that reads `name`'s definition, a second one to try if it
/// fails, and what to append to the text. `None` when the names cannot be
/// written safely as literals.
fn definition_sql(
    family: Family,
    dialect: SqlDialect,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Option<(String, Option<String>, &'static str)> {
    let qualified = format!(
        "{}.{}",
        dialect.quote_ident(schema),
        dialect.quote_ident(name)
    );
    let routine = matches!(kind, ObjectKind::Function);
    Some(match family {
        Family::Postgres if routine => {
            // The tree names functions with their arguments, `f(a integer)`,
            // which tells overloads apart.
            let sql = format!(
                "SELECT CASE WHEN p.prokind = 'a' THEN NULL \
                 ELSE pg_catalog.pg_get_functiondef(p.oid) END \
                 FROM pg_catalog.pg_proc p \
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = {schema} \
                 AND (p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')' = {name} \
                 OR p.proname = {name}) \
                 LIMIT 1",
                schema = string_literal(schema)?,
                name = string_literal(name)?,
            );
            (sql, None, ";\n")
        }
        Family::Postgres => {
            let create = if kind == ObjectKind::MaterializedView {
                "CREATE MATERIALIZED VIEW"
            } else {
                "CREATE OR REPLACE VIEW"
            };
            let sql = format!(
                "SELECT {prefix} || pg_catalog.pg_get_viewdef(c.oid, true) \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {schema} AND c.relname = {name}",
                prefix = string_literal(&format!("{create} {qualified} AS\n"))?,
                schema = string_literal(schema)?,
                name = string_literal(name)?,
            );
            (sql, None, "\n")
        }
        Family::MySql if routine => (
            format!("SHOW CREATE FUNCTION {qualified}"),
            Some(format!("SHOW CREATE PROCEDURE {qualified}")),
            "\n",
        ),
        Family::MySql => (format!("SHOW CREATE VIEW {qualified}"), None, ";\n"),
        Family::Other if routine => (
            format!(
                "SELECT routine_definition FROM information_schema.routines \
                 WHERE routine_schema = {} AND routine_name = {}",
                string_literal(schema)?,
                string_literal(name)?,
            ),
            None,
            "\n",
        ),
        Family::Other => (
            format!(
                "SELECT view_definition FROM information_schema.views \
                 WHERE table_schema = {} AND table_name = {}",
                string_literal(schema)?,
                string_literal(name)?,
            ),
            None,
            "\n",
        ),
    })
}

/// The definition in a result: the `Create …` column of a `SHOW CREATE`,
/// else the first.
fn definition_text(output: &QueryOutput) -> Option<String> {
    let set = output.sets.iter().find(|set| set.has_rows())?;
    let column = set
        .columns
        .iter()
        .position(|column| {
            column.name.starts_with("Create ") || column.name == "SQL Original Statement"
        })
        .unwrap_or(0);
    let text = set.rows.get(0, column).flatten()?;
    (!text.trim().is_empty()).then(|| text.to_owned())
}

/// The query that reads a trigger's definition, and what to append to it.
fn trigger_sql(
    family: Family,
    dialect: SqlDialect,
    schema: &str,
    table: &str,
    name: &str,
) -> Option<(String, &'static str)> {
    Some(match family {
        Family::Postgres => (
            format!(
                "SELECT pg_catalog.pg_get_triggerdef(t.oid, true) \
                 FROM pg_catalog.pg_trigger t \
                 JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {} AND c.relname = {} AND t.tgname = {}",
                string_literal(schema)?,
                string_literal(table)?,
                string_literal(name)?,
            ),
            ";\n",
        ),
        Family::MySql => (
            format!(
                "SHOW CREATE TRIGGER {}.{}",
                dialect.quote_ident(schema),
                dialect.quote_ident(name)
            ),
            "\n",
        ),
        Family::Other => (
            format!(
                "SELECT action_statement FROM information_schema.triggers \
                 WHERE trigger_schema = {} AND trigger_name = {}",
                string_literal(schema)?,
                string_literal(name)?,
            ),
            "\n",
        ),
    })
}

impl VioletApp {
    /// Reads the definition of `schema.name` and opens it in an editor.
    pub(super) fn open_definition(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        schema: String,
        name: String,
        kind: ObjectKind,
    ) {
        if self.wait_for_connection(connection, || Action::OpenDefinition {
            connection,
            schema: schema.clone(),
            name: name.clone(),
            kind,
        }) {
            return;
        }
        let family = self.family(connection);
        let dialect = self.dialect(connection);
        let sql = definition_sql(family, dialect, &schema, &name, kind);
        let Some((sql, fallback, suffix)) = sql else {
            self.toasts.push(ctx, unsafe_name());
            return;
        };
        self.read_definition(connection, definition_title(&name), sql, fallback, suffix);
    }

    /// Reads a trigger's definition and opens it in an editor.
    pub(super) fn open_trigger(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        schema: String,
        table: String,
        name: String,
    ) {
        if self.wait_for_connection(connection, || Action::OpenTrigger {
            connection,
            schema: schema.clone(),
            table: table.clone(),
            name: name.clone(),
        }) {
            return;
        }
        let family = self.family(connection);
        let dialect = self.dialect(connection);
        let Some((sql, suffix)) = trigger_sql(family, dialect, &schema, &table, &name) else {
            self.toasts.push(ctx, unsafe_name());
            return;
        };
        self.read_definition(connection, name, sql, None, suffix);
    }

    /// The database family of a connection's driver.
    pub(super) fn family(&self, connection: ConnectionId) -> Family {
        Family::of(&self.driver_of(connection).unwrap_or_default())
    }

    /// When `connection` is not open: connects it and keeps `action` to
    /// run once it is. Returns whether it had to.
    pub(super) fn wait_for_connection(
        &mut self,
        connection: ConnectionId,
        action: impl FnOnce() -> Action,
    ) -> bool {
        if self.backend.is_connected(connection) {
            return false;
        }
        self.waiting.push((connection, action()));
        self.connect(connection, None);
        true
    }

    /// Runs `sql` and opens what it returns in an editor titled `title`; an
    /// editor already showing it is brought forward instead.
    fn read_definition(
        &mut self,
        connection: ConnectionId,
        title: String,
        sql: String,
        fallback: Option<String>,
        suffix: &'static str,
    ) {
        let open = self.workspace.tabs.iter().find(|tab| {
            tab.connection == Some(connection)
                && matches!(tab.kind, TabKind::Script)
                && tab.title == title
                && tab.file.is_none()
        });
        if let Some(tab) = open {
            self.workspace.active = Some(tab.id);
            return;
        }
        let request = self.backend.execute(connection, sql, READ);
        self.definitions.insert(
            request,
            PendingDefinition {
                connection,
                title,
                fallback,
                suffix,
            },
        );
    }

    /// Opens a definition once it is read.
    pub(super) fn definition_read(
        &mut self,
        ctx: &egui::Context,
        mut pending: PendingDefinition,
        result: Result<QueryOutput>,
    ) {
        match result {
            Ok(output) => match definition_text(&output) {
                Some(text) => {
                    let mut sql = text.trim_end().to_owned();
                    sql.push_str(pending.suffix);
                    let tab = self.workspace.open(
                        pending.title,
                        Some(pending.connection),
                        sql,
                        TabKind::Script,
                    );
                    self.workspace.active = Some(tab);
                }
                None => self.toasts.push(
                    ctx,
                    Toast::error(
                        "No definition to show",
                        format!(
                            "The server returned no source for {}. Aggregates, built-in \
                             routines and objects you lack privileges on have none.",
                            pending.title
                        ),
                    ),
                ),
            },
            Err(error) => match pending.fallback.take() {
                Some(sql) => {
                    let request = self.backend.execute(pending.connection, sql, READ);
                    self.definitions.insert(request, pending);
                }
                None => self.toasts.push(
                    ctx,
                    Toast::error("Could not read the definition", error.message),
                ),
            },
        }
    }
}

fn unsafe_name() -> Toast {
    Toast::error(
        "Could not read the definition",
        "The name holds characters that cannot be sent safely in a query.",
    )
}

/// Reads a definition: never from the cache, which may hold an older one.
const READ: ExecOptions = ExecOptions {
    cache: CachePolicy::Refresh,
    page: None,
};

/// An editor's title for `name`: without a function's arguments.
fn definition_title(name: &str) -> String {
    name.split_once('(')
        .map_or(name, |(name, _)| name)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_follow_the_driver() {
        assert_eq!(Family::of("postgres"), Family::Postgres);
        assert_eq!(Family::of("mysql"), Family::MySql);
        assert_eq!(Family::of("mariadb"), Family::MySql);
        assert_eq!(Family::of("sqlite"), Family::Other);
    }

    #[test]
    fn postgres_functions_match_their_arguments() {
        let (sql, fallback, _) = definition_sql(
            Family::Postgres,
            SqlDialect::ANSI,
            "public",
            "add(a integer, b integer)",
            ObjectKind::Function,
        )
        .expect("query");
        assert!(sql.contains("pg_get_functiondef"));
        assert!(sql.contains("= 'add(a integer, b integer)'"));
        assert!(fallback.is_none());
    }

    #[test]
    fn mysql_tries_functions_then_procedures() {
        let dialect = SqlDialect {
            quote_open: '`',
            quote_close: '`',
            ..SqlDialect::ANSI
        };
        let (sql, fallback, _) = definition_sql(
            Family::MySql,
            dialect,
            "shop",
            "total",
            ObjectKind::Function,
        )
        .expect("query");
        assert_eq!(sql, "SHOW CREATE FUNCTION `shop`.`total`");
        assert_eq!(
            fallback.as_deref(),
            Some("SHOW CREATE PROCEDURE `shop`.`total`")
        );
    }

    #[test]
    fn unsafe_names_are_never_sent() {
        assert!(
            definition_sql(
                Family::Postgres,
                SqlDialect::ANSI,
                "public",
                "f\\'",
                ObjectKind::Function
            )
            .is_none()
        );
    }

    #[test]
    fn triggers_are_read_per_database() {
        let (sql, _) = trigger_sql(
            Family::Postgres,
            SqlDialect::ANSI,
            "public",
            "orders",
            "audit",
        )
        .expect("query");
        assert!(sql.contains("pg_get_triggerdef") && sql.contains("t.tgname = 'audit'"));
        let (sql, _) =
            trigger_sql(Family::MySql, SqlDialect::ANSI, "shop", "orders", "audit").expect("query");
        assert_eq!(sql, "SHOW CREATE TRIGGER \"shop\".\"audit\"");
    }

    #[test]
    fn titles_drop_the_arguments() {
        assert_eq!(definition_title("add(a integer)"), "add");
        assert_eq!(definition_title("orders_view"), "orders_view");
    }
}
