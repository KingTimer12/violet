//! An open MySQL session.
//!
//! Statements go through the text protocol (`COM_QUERY`) with multiple
//! statements and results allowed: every type arrives as text, scripts
//! run in one go, and each result says whether another follows. Binary
//! strings and blobs are shown in hex, as MySQL reads them back.

use std::borrow::Cow;
use std::cell::RefCell;

use violet_sdk::{
    ColumnInfo, ColumnMeta, ConnectParams, DbError, DbObject, ObjectKind, Result, ResultSet, Rows,
    RowsBuilder, ServerInfo, Session,
};

use crate::catalog;
use crate::packet::Reader;
use crate::script;
use crate::types::{self, Column};
use crate::wire::{COM_QUERY, MAX_PAYLOAD, MORE_RESULTS, Wire, server_error};

pub struct MySession {
    wire: RefCell<Wire>,
    /// From the greeting, in case `VERSION()` is not answered.
    version: String,
    /// The database the profile names; empty for none.
    database: String,
}

/// What one statement produced.
enum Outcome {
    Rows {
        columns: Vec<Column>,
        rows: RowsBuilder,
    },
    Command {
        affected: u64,
    },
}

impl MySession {
    pub(crate) fn connect(params: &ConnectParams) -> Result<Self> {
        let (wire, greeting) = Wire::connect(params)?;
        log::info!(
            "connected to {} (connection {})",
            product(&greeting.version),
            greeting.connection_id
        );
        Ok(Self {
            wire: RefCell::new(wire),
            version: greeting.version,
            database: params.database.trim().to_owned(),
        })
    }

    /// Runs `sql`, keeping at most `max_rows` rows of each result.
    fn run(&self, sql: &str, max_rows: usize) -> Result<Vec<Outcome>> {
        let mut wire = self.wire.borrow_mut();
        wire.command(COM_QUERY, sql.as_bytes())?;
        let mut outcomes = Vec::new();
        loop {
            let packet = wire.read_packet()?;
            let status = match packet.first() {
                Some(0x00) => {
                    let (affected, status) = ok_packet(&packet)?;
                    outcomes.push(Outcome::Command { affected });
                    status
                }
                Some(0xFF) => return Err(server_error(&packet)),
                // LOAD DATA LOCAL asks for a file of ours: there is none.
                // Answering with nothing makes the server fail the
                // statement.
                Some(0xFB) => {
                    wire.write_packet(&[])?;
                    continue;
                }
                _ => {
                    let count = Reader::new(&packet)
                        .lenenc()?
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| DbError::protocol("Malformed column count"))?;
                    let mut columns = Vec::with_capacity(count.min(4096));
                    for _ in 0..count {
                        columns.push(column(&wire.read_packet()?)?);
                    }
                    if !wire.deprecate_eof() {
                        wire.read_packet()?;
                    }
                    let mut rows = RowsBuilder::new(columns.len(), max_rows);
                    let status = loop {
                        let row = wire.read_packet()?;
                        match row.first() {
                            // A row starting with 0xFE holds a cell of 16 MiB
                            // or more, so it spans packets.
                            Some(0xFE) if row.len() < MAX_PAYLOAD => {
                                break end_status(&row, wire.deprecate_eof())?;
                            }
                            Some(0xFF) => return Err(server_error(&row)),
                            _ => push_row(&row, &columns, &mut rows)?,
                        }
                    };
                    outcomes.push(Outcome::Rows { columns, rows });
                    status
                }
            };
            if status & MORE_RESULTS == 0 {
                return Ok(outcomes);
            }
        }
    }

    /// Runs a catalog query; its first result, as text.
    fn query(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        for outcome in self.run(sql, usize::MAX)? {
            if let Outcome::Rows { columns, rows } = outcome {
                return Ok(cells(&rows.finish(), columns.len()));
            }
        }
        Ok(Vec::new())
    }
}

/// A column definition packet.
fn column(packet: &[u8]) -> Result<Column> {
    let mut r = Reader::new(packet);
    // Catalog, schema, table and original table, then the name.
    for _ in 0..4 {
        r.lenenc_bytes()?;
    }
    let name = r.lenenc_text()?;
    let _original_name = r.lenenc_bytes()?;
    let _fixed = r.lenenc()?;
    let charset = r.u16()?;
    let length = r.u32()?;
    let kind = r.u8()?;
    let flags = r.u16()?;
    Ok(Column {
        name,
        kind,
        charset,
        length,
        flags,
    })
}

/// Rows affected and the status flags of an OK packet.
fn ok_packet(packet: &[u8]) -> Result<(u64, u16)> {
    let mut r = Reader::new(packet);
    r.u8()?;
    let affected = r.lenenc()?.unwrap_or_default();
    let _last_insert_id = r.lenenc()?;
    Ok((affected, r.u16()?))
}

/// The status flags of the packet that ends a result's rows: an OK packet
/// (with `CLIENT_DEPRECATE_EOF`) or an EOF one.
fn end_status(packet: &[u8], deprecate_eof: bool) -> Result<u16> {
    if deprecate_eof {
        return ok_packet(packet).map(|(_, status)| status);
    }
    let mut r = Reader::new(packet);
    r.u8()?;
    let _warnings = r.u16()?;
    r.u16()
}

fn push_row(row: &[u8], columns: &[Column], rows: &mut RowsBuilder) -> Result<()> {
    if rows.is_full() {
        // Keep reading, so later statements run, but hold nothing.
        return rows.push([]);
    }
    let mut r = Reader::new(row);
    let mut texts: Vec<Option<Cow<'_, str>>> = Vec::with_capacity(columns.len());
    for column in columns {
        texts.push(r.lenenc_bytes()?.map(|bytes| {
            if column.shows_hex() {
                Cow::Owned(types::hex(bytes))
            } else {
                String::from_utf8_lossy(bytes)
            }
        }));
    }
    rows.push(texts.iter().map(|cell| cell.as_deref()))
}

/// Collected rows back as cells, for catalog queries.
fn cells(rows: &Rows, width: usize) -> Vec<Vec<Option<String>>> {
    const NULL: u32 = 1 << 31;
    let mut start = 0usize;
    let mut cells: Vec<Option<String>> = Vec::with_capacity(rows.ends.len());
    for end in &rows.ends {
        let offset = (end & !NULL) as usize;
        cells.push(
            (end & NULL == 0).then(|| rows.text.get(start..offset).unwrap_or_default().to_owned()),
        );
        start = offset;
    }
    if width == 0 {
        return Vec::new();
    }
    cells.chunks(width).map(<[_]>::to_vec).collect()
}

/// `MySQL 8.4.11` or `MariaDB 11.4.3`, from what `VERSION()` returns.
fn product(version: &str) -> String {
    // MariaDB greets with a fake 5.5.5 prefix for old clients.
    let version = version.strip_prefix("5.5.5-").unwrap_or(version);
    match version.find("-MariaDB") {
        Some(at) => format!("MariaDB {}", &version[..at]),
        None => format!("MySQL {version}"),
    }
}

/// Commands whose affected rows mean something.
fn counts_rows(keyword: &str) -> bool {
    matches!(
        keyword,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD" | "CALL"
    )
}

fn relation_kind(table_type: &str) -> Option<ObjectKind> {
    Some(match table_type {
        "BASE TABLE" | "SYSTEM VERSIONED" => ObjectKind::Table,
        "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
        "SEQUENCE" => ObjectKind::Sequence,
        _ => return None,
    })
}

fn cell(row: &[Option<String>], index: usize) -> Option<String> {
    row.get(index).cloned().flatten()
}

fn flag(row: &[Option<String>], index: usize) -> bool {
    row.get(index).and_then(Option::as_deref) == Some("1")
}

/// Comments as the server keeps them, without its placeholders.
fn comment(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.is_empty() && text != "VIEW")
}

impl Session for MySession {
    fn server_info(&self) -> Result<ServerInfo> {
        let rows = self.query(catalog::SERVER_INFO)?;
        let row = rows
            .first()
            .ok_or_else(|| DbError::protocol("The server sent no information"))?;
        let version = cell(row, 0).unwrap_or_else(|| self.version.clone());
        // CURRENT_USER() is `user@host`; the host is the server's view.
        let user = cell(row, 2)
            .map(|user| {
                user.rsplit_once('@')
                    .map_or(user.clone(), |(u, _)| u.to_owned())
            })
            .unwrap_or_default();
        Ok(ServerInfo {
            version: product(&version),
            database: cell(row, 1).unwrap_or_default(),
            user,
        })
    }

    fn databases(&self) -> Result<Vec<String>> {
        Ok(self
            .query(catalog::SCHEMAS)?
            .into_iter()
            .filter_map(|row| cell(&row, 0))
            .collect())
    }

    /// The session's own database alone: the others show as databases.
    /// Without one, every database.
    fn schemas(&self) -> Result<Vec<String>> {
        if !self.database.is_empty() {
            return Ok(vec![self.database.clone()]);
        }
        Ok(self
            .query(catalog::SCHEMAS)?
            .into_iter()
            .filter_map(|row| cell(&row, 0))
            .collect())
    }

    fn objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let mut objects: Vec<DbObject> = self
            .query(&catalog::relations(schema))?
            .into_iter()
            .filter_map(|row| {
                Some(DbObject {
                    name: cell(&row, 0)?,
                    kind: relation_kind(row.get(1)?.as_deref()?)?,
                    comment: comment(cell(&row, 2)),
                    estimated_rows: cell(&row, 3).and_then(|n| n.parse().ok()),
                })
            })
            .collect();
        objects.extend(
            self.query(&catalog::routines(schema))?
                .into_iter()
                .filter_map(|row| {
                    Some(DbObject {
                        name: cell(&row, 0)?,
                        kind: ObjectKind::Function,
                        comment: comment(cell(&row, 1)),
                        estimated_rows: None,
                    })
                }),
        );
        Ok(objects)
    }

    fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        Ok(self
            .query(&catalog::columns(schema, relation))?
            .into_iter()
            .filter_map(|row| {
                Some(ColumnInfo {
                    name: cell(&row, 0)?,
                    data_type: cell(&row, 1).unwrap_or_default(),
                    nullable: flag(&row, 2),
                    default: cell(&row, 3),
                    primary_key: flag(&row, 4),
                    comment: comment(cell(&row, 5)),
                })
            })
            .collect())
    }

    fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
        let outcomes = self.run(sql, max_rows)?;
        // Statements and results pair up unless a procedure returned
        // several results.
        let keywords = script::keywords(sql);
        let paired = keywords.len() == outcomes.len();
        Ok(outcomes
            .into_iter()
            .enumerate()
            .map(|(i, outcome)| {
                let keyword = keywords.get(i).filter(|_| paired);
                match outcome {
                    Outcome::Command { affected } => {
                        let counted = keyword.is_none_or(|k| counts_rows(k));
                        ResultSet::command(
                            keyword.map_or("OK", String::as_str),
                            counted.then_some(affected),
                        )
                    }
                    Outcome::Rows { columns, rows } => {
                        let meta = columns
                            .iter()
                            .map(|c| ColumnMeta::new(c.name.clone(), c.type_name(), c.numeric()))
                            .collect();
                        let mut result = ResultSet::query(meta, rows);
                        result.command = Some(keyword.cloned().unwrap_or_else(|| "SELECT".into()));
                        result
                    }
                }
            })
            .collect())
    }
}

impl Drop for MySession {
    fn drop(&mut self) {
        self.wire.get_mut().quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_are_named_from_the_version() {
        assert_eq!(product("8.4.11"), "MySQL 8.4.11");
        assert_eq!(
            product("5.5.5-10.11.6-MariaDB-1:10.11.6"),
            "MariaDB 10.11.6"
        );
        assert_eq!(product("11.4.3-MariaDB-log"), "MariaDB 11.4.3");
    }

    #[test]
    fn rows_come_back_as_cells() {
        let columns = vec![
            Column {
                name: "a".into(),
                kind: 253,
                charset: 255,
                length: 0,
                flags: 0,
            },
            Column {
                name: "b".into(),
                kind: 252,
                charset: types::BINARY,
                length: 0,
                flags: 0,
            },
        ];
        let mut rows = RowsBuilder::new(2, 10);
        let mut row = vec![2, b'h', b'i', 2, 0xCA, 0xFE];
        push_row(&row, &columns, &mut rows).expect("row");
        row = vec![crate::packet::NULL, 0];
        push_row(&row, &columns, &mut rows).expect("nulls");
        assert_eq!(
            cells(&rows.finish(), 2),
            vec![
                vec![Some("hi".to_owned()), Some("0xCAFE".to_owned())],
                vec![None, Some("0x".to_owned())],
            ]
        );
    }

    #[test]
    fn results_end_with_ok_or_eof() {
        // OK: header, affected, insert id, status, warnings.
        assert_eq!(
            end_status(&[0xFE, 0, 0, 0x0A, 0, 0, 0], true).expect("ok"),
            0x0A
        );
        // EOF: header, warnings, status.
        assert_eq!(end_status(&[0xFE, 0, 0, 0x08, 0], false).expect("eof"), 8);
        assert_eq!(ok_packet(&[0, 3, 0, 2, 0]).expect("ok"), (3, 2));
    }
}
