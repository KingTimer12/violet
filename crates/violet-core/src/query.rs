//! The results of running SQL.

use std::time::Duration;

/// A column of a result set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// The database's name for the type (`int4`, `text`...), when known.
    pub type_name: Option<String>,
    /// Numbers are right aligned in the grid.
    pub numeric: bool,
}

/// One statement's result: rows, or the count a command affected.
///
/// Values are kept as the server's text form; `None` is SQL `NULL`. Text is
/// what the grid shows and copies, and it represents every type a driver
/// may meet, including ones Violet has never heard of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows a command inserted, updated or deleted, or a query returned.
    pub affected: Option<u64>,
    /// The command tag, such as `SELECT` or `CREATE TABLE`, when known.
    pub command: Option<String>,
    /// More rows existed than [`ExecLimits::max_rows`] allowed.
    pub truncated: bool,
}

impl ResultSet {
    #[must_use]
    pub fn has_rows(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// Everything one run of an editor produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryOutput {
    pub sets: Vec<ResultSet>,
    pub elapsed: Duration,
}

/// Bounds on what a run may bring back, so a careless `SELECT *` on a huge
/// table cannot exhaust memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecLimits {
    pub max_rows: usize,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self { max_rows: 10_000 }
    }
}
