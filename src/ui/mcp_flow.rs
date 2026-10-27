//! Answers the agent's MCP tool calls (`crate::mcp`) from what Violet
//! knows: its connections, the catalog the tree loads, and the open editor.
//! Catalog data not loaded yet is loaded as completion would; no tool reads
//! rows or runs a statement.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::Value;
use violet_core::{ConnectionId, Language};

use super::VioletApp;
use super::action::Action;
use super::state::{ConnectionCatalog, Link};
use crate::completion::{Catalog, Need};
use crate::mcp::Call;

/// How long a call waits for the catalog before answering with what is
/// loaded.
const WAIT: Duration = Duration::from_secs(20);

/// A call waiting for the catalog.
pub struct Pending {
    call: Call,
    since: Instant,
}

impl Pending {
    pub fn new(call: Call) -> Self {
        Self {
            call,
            since: Instant::now(),
        }
    }
}

/// A tool's answer: done, or waiting for these to load.
enum Answer {
    Done(Result<String, String>),
    Loading(ConnectionId, Vec<Need>, String),
}

impl VioletApp {
    /// Answers the calls that can be answered; the rest wait for the
    /// catalog, up to [`WAIT`].
    pub(super) fn answer_tools(&mut self, pending: Vec<Pending>) -> Vec<Pending> {
        let mut waiting = Vec::new();
        for pending in pending {
            let answer = match self.tool(&pending.call.tool, &pending.call.arguments) {
                Answer::Done(answer) => answer,
                Answer::Loading(id, needs, partial) => {
                    if pending.since.elapsed() < WAIT {
                        self.actions.push(Action::LoadForCompletion(id, needs));
                        waiting.push(pending);
                        continue;
                    }
                    Ok(format!(
                        "{partial}\n(Some of the catalog did not load in time; this is what \
                         Violet has.)"
                    ))
                }
            };
            let _ = pending.call.reply.send(answer);
        }
        waiting
    }

    fn tool(&self, tool: &str, arguments: &Value) -> Answer {
        let argument = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        match tool {
            "list_connections" => Answer::Done(Ok(self.list_connections())),
            "get_editor" => Answer::Done(self.editor_for_agent()),
            "describe_schema" => match self.tool_connection(argument("connection")) {
                Ok(id) => self.describe_schema_tool(id, argument("schema")),
                Err(error) => Answer::Done(Err(error)),
            },
            "describe_table" => match (
                self.tool_connection(argument("connection")),
                argument("table"),
            ) {
                (Ok(id), Some(table)) => self.describe_table_tool(id, argument("schema"), table),
                (Err(error), _) => Answer::Done(Err(error)),
                (_, None) => Answer::Done(Err("table is missing".to_owned())),
            },
            _ => Answer::Done(Err(format!("no tool {tool}"))),
        }
    }

    fn active_connection(&self) -> Option<ConnectionId> {
        self.workspace
            .active()
            .filter(|tab| tab.data().is_none())
            .and_then(|tab| tab.connection)
    }

    fn list_connections(&self) -> String {
        let active = self.active_connection().map(|id| self.profile_of(id));
        let mut text = String::new();
        for profile in self.profiles.all() {
            let state = match self.explorer.link(profile.id) {
                Link::Online(server) => format!(
                    "connected: {}, database {}",
                    server.version, server.database
                ),
                Link::Connecting => "connecting".to_owned(),
                Link::Offline | Link::Failed(_) => "not connected".to_owned(),
            };
            let _ = write!(
                text,
                "- {} ({}; {state})",
                profile.name,
                self.driver_name(profile.id)
            );
            if active == Some(profile.id) {
                text.push_str(" [the active editor's]");
            }
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("The user has no saved connections.");
        }
        text
    }

    /// The connection named, else the active editor's; connected.
    fn tool_connection(&self, name: Option<&str>) -> Result<ConnectionId, String> {
        let id = match name {
            Some(name) => self
                .profiles
                .all()
                .iter()
                .find(|profile| profile.name.eq_ignore_ascii_case(name))
                .map(|profile| profile.id)
                .ok_or_else(|| {
                    format!("No connection is named \"{name}\"; see list_connections.")
                })?,
            None => self
                .active_connection()
                .ok_or("The active editor has no connection; name one from list_connections.")?,
        };
        if self.explorer.link(id).is_online() {
            Ok(id)
        } else {
            Err(format!(
                "\"{}\" is not connected. Ask the user to connect it in Violet; its schema \
                 is not known until then.",
                self.profile_name(id)
            ))
        }
    }

    fn describe_schema_tool(&self, id: ConnectionId, schema: Option<&str>) -> Answer {
        let catalog = ConnectionCatalog {
            explorer: &self.explorer,
            id: Some(id),
        };
        let all = catalog.schemas();
        let schemas = match schema {
            Some(schema) => match all.iter().find(|name| name.eq_ignore_ascii_case(schema)) {
                Some(name) => vec![name.clone()],
                None if all.is_empty() => vec![schema.to_owned()],
                None => {
                    return Answer::Done(Err(format!(
                        "No schema \"{schema}\". The connection has: {}",
                        all.join(", ")
                    )));
                }
            },
            None => crate::ai::schemas_to_describe(self.search_path(id), all),
        };
        let described = crate::ai::describe(&catalog, &schemas, self.language(id) == Language::Sql);
        let mut text = format!(
            "{} ({})\n",
            self.profile_name(id),
            match self.explorer.link(id) {
                Link::Online(server) => server.version.clone(),
                _ => self.driver_name(id),
            }
        );
        if !all.is_empty() {
            let _ = writeln!(text, "All schemas: {}", all.join(", "));
        }
        text.push_str(&described.text);
        if described.needs.is_empty() {
            Answer::Done(Ok(text))
        } else {
            Answer::Loading(id, described.needs, text)
        }
    }

    fn describe_table_tool(&self, id: ConnectionId, schema: Option<&str>, table: &str) -> Answer {
        let catalog = ConnectionCatalog {
            explorer: &self.explorer,
            id: Some(id),
        };
        let schemas = match schema {
            Some(schema) => vec![schema.to_owned()],
            None => {
                crate::ai::schemas_to_describe(self.search_path(id), catalog.schemas())
            }
        };
        // The table, in the first schema that has it.
        let mut needs = Vec::new();
        let mut found = None;
        for name in &schemas {
            match catalog.objects(name) {
                Some(objects) => {
                    if let Some(object) = objects
                        .iter()
                        .find(|object| object.name.eq_ignore_ascii_case(table))
                    {
                        found = Some((name.clone(), object));
                        break;
                    }
                }
                None => needs.push(Need::Objects(name.clone())),
            }
        }
        let Some((schema, object)) = found else {
            if needs.is_empty() {
                return Answer::Done(Err(format!(
                    "No table or view \"{table}\" in {}.",
                    schemas.join(", ")
                )));
            }
            return Answer::Loading(id, needs, format!("\"{table}\" was not found yet."));
        };
        let columns = catalog.columns(&schema, &object.name);
        let keys = catalog.foreign_keys(&schema);
        let sql = self.language(id) == Language::Sql;
        let mut needs = Vec::new();
        if columns.is_none() {
            needs.push(Need::Columns(schema.clone(), object.name.clone()));
        }
        if sql && keys.is_none() {
            needs.push(Need::ForeignKeys(schema.clone()));
        }
        let mut text = format!("{:?} {schema}.{}", object.kind, object.name);
        if let Some(rows) = object.estimated_rows.filter(|rows| *rows >= 0) {
            let _ = write!(text, ", about {rows} rows");
        }
        if let Some(comment) = &object.comment {
            let _ = write!(text, " -- {comment}");
        }
        text.push('\n');
        for column in columns.unwrap_or_default() {
            let _ = write!(text, "  {} {}", column.name, column.data_type);
            if column.primary_key {
                text.push_str(" PRIMARY KEY");
            }
            if !column.nullable {
                text.push_str(" NOT NULL");
            }
            if let Some(default) = &column.default {
                let _ = write!(text, " DEFAULT {default}");
            }
            if let Some(comment) = &column.comment {
                let _ = write!(text, " -- {comment}");
            }
            text.push('\n');
        }
        for key in keys.unwrap_or_default() {
            let from = key.table.eq_ignore_ascii_case(&object.name);
            let to = key.ref_table.eq_ignore_ascii_case(&object.name)
                && key.ref_schema.eq_ignore_ascii_case(&schema);
            if from || to {
                let _ = writeln!(
                    text,
                    "  foreign key {}.{}({}) -> {}.{}({})",
                    key.schema,
                    key.table,
                    key.columns.join(", "),
                    key.ref_schema,
                    key.ref_table,
                    key.ref_columns.join(", ")
                );
            }
        }
        if needs.is_empty() {
            Answer::Done(Ok(text))
        } else {
            Answer::Loading(id, needs, text)
        }
    }

    /// The active editor for the agent: its text and selection, with
    /// passwords hidden, and its connection.
    pub(super) fn editor_for_agent(&self) -> Result<String, String> {
        let tab = self
            .workspace
            .active()
            .filter(|tab| tab.data().is_none())
            .ok_or("No SQL editor is open.")?;
        let language = tab.connection.map_or(Language::Sql, |id| self.language(id));
        let mut text = format!("Editor \"{}\"", tab.title);
        match tab.connection {
            Some(id) => {
                let _ = write!(text, " on the connection \"{}\"", self.profile_name(id));
                if let Link::Online(server) = self.explorer.link(id) {
                    let _ = write!(text, " ({}, database {})", server.version, server.database);
                }
            }
            None => text.push_str(" with no connection"),
        }
        let body = crate::lang::redact_secrets(language, &tab.sql);
        let _ = write!(text, ":\n```\n{body}\n```\n");
        if let Some(selected) = tab
            .sql
            .get(tab.selection.clone())
            .filter(|selected| !selected.trim().is_empty())
        {
            let selected = crate::lang::redact_secrets(language, selected);
            let _ = write!(text, "Selected:\n```\n{selected}\n```\n");
        }
        Ok(text)
    }
}
