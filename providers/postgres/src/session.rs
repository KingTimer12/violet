//! An open PostgreSQL session.
//!
//! Statements the user runs go through the simple query protocol: results
//! come back in the server's text form, so every type renders (extensions'
//! and arrays included), several statements run in one go, and each
//! statement's columns arrive with their type ids. Catalog queries use the
//! extended protocol, so names travel as bind parameters.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use fallible_iterator::FallibleIterator as _;
use postgres_protocol::IsNull;
use postgres_protocol::message::backend::{DataRowBody, Message};
use postgres_protocol::message::frontend;
use violet_sdk::{
    ColumnInfo, ColumnMeta, ConnectParams, DbError, DbObject, ObjectKind, Result, ResultSet,
    RowsBuilder, ServerInfo, Session,
};

use crate::catalog;
use crate::types;
use crate::wire::{BackendKey, Wire, protocol, server_error};

pub struct PgSession {
    wire: RefCell<Wire>,
    key: Option<BackendKey>,
    version: String,
    /// Type names learnt from `pg_type`, by id.
    type_names: RefCell<HashMap<u32, String>>,
}

impl PgSession {
    pub(crate) fn connect(params: &ConnectParams) -> Result<Self> {
        let mut wire = Wire::open(params)?;
        let (key, version) = wire.startup(params)?;
        log::info!("connected to PostgreSQL {version}");
        Ok(Self {
            wire: RefCell::new(wire),
            key,
            version,
            type_names: RefCell::new(HashMap::new()),
        })
    }

    /// Runs a query through the extended protocol, with text parameters;
    /// rows as text. For the catalog, whose results are small.
    fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
        let mut wire = self.wire.borrow_mut();
        wire.queue(|buf| frontend::parse("", sql, [], buf))?;
        wire.queue(|buf| {
            frontend::bind(
                "",
                "",
                [],
                params.iter(),
                |value, buf| {
                    buf.extend_from_slice(value.as_bytes());
                    Ok(IsNull::No)
                },
                [],
                buf,
            )
            .map_err(|error| match error {
                frontend::BindError::Serialization(error) => error,
                frontend::BindError::Conversion(error) => std::io::Error::other(error),
            })
        })?;
        wire.queue(|buf| frontend::execute("", 0, buf))?;
        wire.queue(|buf| {
            frontend::sync(buf);
            Ok(())
        })?;
        wire.flush()?;

        let mut rows = Vec::new();
        let mut failure = None;
        loop {
            match wire.next()? {
                Message::DataRow(row) => rows.push(owned_cells(&row)?),
                Message::ErrorResponse(body) => failure = Some(server_error(&body)),
                Message::ReadyForQuery(_) => break,
                _ => {}
            }
        }
        failure.map_or(Ok(rows), Err)
    }

    /// The names of `oids` the built-in table does not know, from
    /// `pg_type`, remembered for the session.
    fn learn_type_names(&self, oids: &[u32]) {
        let unknown: Vec<String> = {
            let known = self.type_names.borrow();
            let mut unknown: Vec<u32> = oids
                .iter()
                .copied()
                .filter(|oid| types::name(*oid).is_none() && !known.contains_key(oid))
                .collect();
            unknown.sort_unstable();
            unknown.dedup();
            unknown.iter().map(u32::to_string).collect()
        };
        if unknown.is_empty() {
            return;
        }
        // Inside a failed transaction this errors; the types then stay
        // unnamed, which is all.
        let sql = format!(
            "SELECT oid::int8::text, typname::text FROM pg_catalog.pg_type WHERE oid IN ({})",
            unknown.join(",")
        );
        if let Ok(rows) = self.query(&sql, &[]) {
            let mut known = self.type_names.borrow_mut();
            for row in rows {
                if let (Some(oid), Some(name)) = (cell(&row, 0), cell(&row, 1))
                    && let Ok(oid) = oid.parse()
                {
                    known.insert(oid, name);
                }
            }
        }
    }

    fn type_name(&self, oid: u32) -> Option<String> {
        types::name(oid)
            .map(str::to_owned)
            .or_else(|| self.type_names.borrow().get(&oid).cloned())
    }

    /// Runs `sql` through the simple protocol.
    fn run(&self, sql: &str, max_rows: usize) -> Result<Vec<PendingSet>> {
        let mut wire = self.wire.borrow_mut();
        wire.queue(|buf| frontend::query(sql, buf))?;
        wire.flush()?;

        let mut sets = Vec::new();
        let mut current: Option<PendingSet> = None;
        let mut failure = None;
        let mut refused_copy = false;
        loop {
            match wire.next()? {
                Message::RowDescription(body) => {
                    let mut columns = Vec::new();
                    let mut fields = body.fields();
                    while let Some(field) = fields.next().map_err(protocol)? {
                        columns.push((field.name().to_owned(), field.type_oid()));
                    }
                    current = Some(PendingSet {
                        rows: RowsBuilder::new(columns.len(), max_rows),
                        cells: Vec::new(),
                        columns,
                        tag: None,
                    });
                }
                Message::DataRow(row) => {
                    if let Some(set) = &mut current {
                        set.push(&row)?;
                    }
                }
                Message::CommandComplete(body) => {
                    let mut set = current.take().unwrap_or_default();
                    set.tag = Some(body.tag().map_err(protocol)?.to_owned());
                    sets.push(set);
                }
                Message::CopyInResponse(_) => {
                    wire.queue(|buf| {
                        frontend::copy_fail("COPY FROM STDIN is not supported in Violet", buf)
                    })?;
                    wire.flush()?;
                    refused_copy = true;
                }
                Message::ErrorResponse(body) => {
                    // The server skips the rest of the script and only
                    // reports ready afterwards.
                    failure.get_or_insert_with(|| {
                        let mut error = server_error(&body);
                        if refused_copy {
                            error.kind = violet_sdk::ErrorKind::Unsupported;
                        }
                        error
                    });
                }
                Message::ReadyForQuery(_) => break,
                _ => {}
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if let Some(set) = current {
            sets.push(set);
        }
        Ok(sets)
    }
}

/// A statement's result while it streams in.
#[derive(Default)]
struct PendingSet {
    columns: Vec<(String, u32)>,
    rows: RowsBuilder,
    /// Scratch space for one row's cells.
    cells: Vec<Option<std::ops::Range<usize>>>,
    tag: Option<String>,
}

impl PendingSet {
    fn push(&mut self, row: &DataRowBody) -> Result<()> {
        if self.rows.is_full() {
            // Keep reading, so later statements run, but hold nothing.
            return self.rows.push([]);
        }
        self.cells.clear();
        let mut ranges = row.ranges();
        while let Some(range) = ranges.next().map_err(protocol)? {
            self.cells.push(range);
        }
        let buffer = row.buffer();
        let texts: Vec<Option<Cow<'_, str>>> = self
            .cells
            .iter()
            .map(|range| range.clone().map(|range| text(&buffer[range])))
            .collect();
        self.rows.push(texts.iter().map(|cell| cell.as_deref()))
    }

    /// The command and the count from a tag such as `INSERT 0 3`.
    fn command(&self) -> (Option<String>, Option<u64>) {
        let Some(tag) = &self.tag else {
            return (None, None);
        };
        let mut words: Vec<&str> = tag.split_whitespace().collect();
        let count = words.last().and_then(|word| word.parse().ok());
        while words.last().is_some_and(|word| word.parse::<u64>().is_ok()) {
            words.pop();
        }
        (Some(words.join(" ")), count)
    }
}

/// UTF-8 is what was asked for; anything else is shown, not dropped.
fn text(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

fn owned_cells(row: &DataRowBody) -> Result<Vec<Option<String>>> {
    let buffer = row.buffer();
    let mut cells = Vec::new();
    let mut ranges = row.ranges();
    while let Some(range) = ranges.next().map_err(protocol)? {
        cells.push(range.map(|range| text(&buffer[range]).into_owned()));
    }
    Ok(cells)
}

fn relation_kind(relkind: &str) -> Option<ObjectKind> {
    Some(match relkind {
        "r" | "p" => ObjectKind::Table,
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        "f" => ObjectKind::ForeignTable,
        "S" => ObjectKind::Sequence,
        _ => return None,
    })
}

fn cell(row: &[Option<String>], index: usize) -> Option<String> {
    row.get(index).cloned().flatten()
}

fn flag(row: &[Option<String>], index: usize) -> bool {
    row.get(index).and_then(Option::as_deref) == Some("t")
}

impl Session for PgSession {
    fn server_info(&self) -> Result<ServerInfo> {
        let rows = self.query(catalog::SERVER_INFO, &[])?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::protocol("The server sent no information"))?;
        Ok(ServerInfo {
            version: format!(
                "PostgreSQL {}",
                cell(row, 0).unwrap_or_else(|| self.version.clone())
            ),
            database: cell(row, 1).unwrap_or_default(),
            user: cell(row, 2).unwrap_or_default(),
        })
    }

    fn databases(&self) -> Result<Vec<String>> {
        Ok(self
            .query(catalog::DATABASES, &[])?
            .into_iter()
            .filter_map(|row| cell(&row, 0))
            .collect())
    }

    fn schemas(&self) -> Result<Vec<String>> {
        Ok(self
            .query(catalog::SCHEMAS, &[])?
            .into_iter()
            .filter_map(|row| cell(&row, 0))
            .collect())
    }

    fn objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let mut objects: Vec<DbObject> = self
            .query(catalog::RELATIONS, &[schema])?
            .into_iter()
            .filter_map(|row| {
                Some(DbObject {
                    name: cell(&row, 0)?,
                    kind: relation_kind(row.get(1)?.as_deref()?)?,
                    comment: cell(&row, 2),
                    estimated_rows: cell(&row, 3).and_then(|n| n.parse().ok()),
                })
            })
            .collect();
        objects.extend(
            self.query(catalog::FUNCTIONS, &[schema])?
                .into_iter()
                .filter_map(|row| {
                    Some(DbObject {
                        name: cell(&row, 0)?,
                        kind: ObjectKind::Function,
                        comment: cell(&row, 1),
                        estimated_rows: None,
                    })
                }),
        );
        Ok(objects)
    }

    fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        Ok(self
            .query(catalog::COLUMNS, &[schema, relation])?
            .into_iter()
            .filter_map(|row| {
                Some(ColumnInfo {
                    name: cell(&row, 0)?,
                    data_type: cell(&row, 1).unwrap_or_default(),
                    nullable: flag(&row, 2),
                    default: cell(&row, 3),
                    primary_key: flag(&row, 4),
                    comment: cell(&row, 5),
                })
            })
            .collect())
    }

    fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        let sets = self.run(sql, max_rows)?;
        let oids: Vec<u32> = sets
            .iter()
            .flat_map(|set| set.columns.iter().map(|(_, oid)| *oid))
            .collect();
        self.learn_type_names(&oids);
        Ok(sets
            .into_iter()
            .map(|set| {
                let (command, count) = set.command();
                if set.columns.is_empty() {
                    return ResultSet::command(command.unwrap_or_default(), count);
                }
                let columns = set
                    .columns
                    .iter()
                    .map(|(name, oid)| {
                        ColumnMeta::new(name.clone(), self.type_name(*oid), types::numeric(*oid))
                    })
                    .collect();
                let mut result = ResultSet::query(columns, set.rows);
                result.command = command;
                result.affected = count.or(result.affected);
                result
            })
            .collect())
    }

    fn change_token(&self) -> Result<Option<String>> {
        Ok(self
            .query(catalog::CHANGE_TOKEN, &[])?
            .first()
            .and_then(|row| cell(row, 0)))
    }

    fn cancel_ticket(&self) -> Option<Vec<u8>> {
        self.key.map(BackendKey::to_ticket)
    }
}

impl Drop for PgSession {
    fn drop(&mut self) {
        self.wire.get_mut().terminate();
    }
}
