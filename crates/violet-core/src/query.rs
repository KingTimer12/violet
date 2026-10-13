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
    pub rows: Rows,
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

/// A result set's cells, row-major in one text buffer.
///
/// A grid of `Vec<Vec<Option<String>>>` costs an allocation per cell and
/// scatters them across the heap; ten thousand rows of twenty columns are
/// two hundred thousand allocations. Here the whole grid is two: the text
/// and the end offset of each cell, so reading a row walks memory in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    width: usize,
    text: String,
    /// Where each cell's text ends in `text`; [`NULL`] marks SQL `NULL`.
    ends: Vec<usize>,
}

/// Offsets never reach `isize::MAX`, so the top bit is free to flag `NULL`.
const NULL: usize = 1 << (usize::BITS - 1);

impl Rows {
    /// An empty grid of `width` columns.
    #[must_use]
    pub fn new(width: usize) -> Self {
        Self {
            width,
            ..Self::default()
        }
    }

    /// Appends a row. Missing cells are `NULL` and extra ones are dropped,
    /// so every row is `width` wide. A grid made without a width takes the
    /// first row's.
    pub fn push<'a>(&mut self, cells: impl IntoIterator<Item = Option<&'a str>>) {
        let start = self.ends.len();
        for cell in cells {
            if let Some(text) = cell {
                self.text.push_str(text);
                self.ends.push(self.text.len());
            } else {
                self.ends.push(self.text.len() | NULL);
            }
        }
        if self.width == 0 && start == 0 {
            self.width = self.ends.len();
        }
        let end = start + self.width;
        if self.ends.len() > end {
            self.text.truncate(self.ends[end - 1] & !NULL);
            self.ends.truncate(end);
        }
        while self.ends.len() < end {
            self.ends.push(self.text.len() | NULL);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len().checked_div(self.width).unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        (index < self.len()).then_some(Row { rows: self, index })
    }

    /// The cell at `row`, `column`: `None` when out of range, `Some(None)`
    /// for `NULL`.
    #[must_use]
    pub fn get(&self, row: usize, column: usize) -> Option<Option<&str>> {
        self.row(row)?.get(column)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Row<'_>> {
        (0..self.len()).map(move |index| Row { rows: self, index })
    }

    /// Bytes of text held, for sizing exports.
    #[must_use]
    pub fn text_len(&self) -> usize {
        self.text.len()
    }

    /// Keeps the first `rows` rows.
    pub fn truncate(&mut self, rows: usize) {
        let cells = rows.saturating_mul(self.width);
        if cells < self.ends.len() {
            self.text.truncate(self.cell_start(cells));
            self.ends.truncate(cells);
        }
    }

    /// Appends `other`'s rows, as when the next page of a result arrives.
    /// Rows of another width are squared to this one's.
    pub fn append(&mut self, other: &Self) {
        if self.width == other.width || self.is_empty() {
            if self.is_empty() {
                self.width = other.width;
            }
            let base = self.text.len();
            self.text.push_str(&other.text);
            self.ends.extend(
                other
                    .ends
                    .iter()
                    .map(|end| ((end & !NULL) + base) | (end & NULL)),
            );
        } else {
            for row in other.iter() {
                self.push(row.iter());
            }
        }
    }

    fn cell_start(&self, cell: usize) -> usize {
        cell.checked_sub(1)
            .map_or(0, |previous| self.ends[previous] & !NULL)
    }

    fn cell(&self, cell: usize) -> Option<&str> {
        let end = self.ends[cell];
        (end & NULL == 0).then(|| &self.text[self.cell_start(cell)..end])
    }
}

/// One row of [`Rows`].
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    rows: &'a Rows,
    index: usize,
}

impl<'a> Row<'a> {
    /// The cell in `column`: `None` when out of range, `Some(None)` for
    /// `NULL`.
    #[must_use]
    pub fn get(self, column: usize) -> Option<Option<&'a str>> {
        (column < self.rows.width).then(|| self.rows.cell(self.index * self.rows.width + column))
    }

    pub fn iter(self) -> impl ExactSizeIterator<Item = Option<&'a str>> {
        let rows = self.rows;
        let first = self.index * rows.width;
        // Slicing the row once leaves one bounds check, not one per cell, and
        // each cell starts where the previous one ended.
        let mut start = rows.cell_start(first);
        rows.ends[first..first + rows.width]
            .iter()
            .map(move |&end| {
                let stop = end & !NULL;
                let cell = (end & NULL == 0).then(|| &rows.text[start..stop]);
                start = stop;
                cell
            })
    }

    #[must_use]
    pub fn to_vec(self) -> Vec<Option<String>> {
        self.iter().map(|cell| cell.map(str::to_owned)).collect()
    }
}

/// Everything one run of an editor produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryOutput {
    pub sets: Vec<ResultSet>,
    pub elapsed: Duration,
    /// Set when the run read one page of a longer result.
    pub page: Option<PageInfo>,
}

/// A window of a query's rows, as a data view fetches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Page {
    pub offset: u64,
    pub limit: u64,
}

impl Page {
    /// Rows fetched at a time, as DBeaver's default fetch size.
    pub const SIZE: u64 = 200;

    #[must_use]
    pub const fn first() -> Self {
        Self {
            offset: 0,
            limit: Self::SIZE,
        }
    }
}

/// Where a fetched page sits in its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageInfo {
    pub offset: u64,
    pub limit: u64,
    /// More rows follow this page.
    pub has_more: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_text_and_null_apart() {
        let mut rows = Rows::new(3);
        rows.push([Some("1"), Some(""), None]);
        rows.push([None, Some("é"), Some("x")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.get(0, 0), Some(Some("1")));
        assert_eq!(rows.get(0, 1), Some(Some("")));
        assert_eq!(rows.get(0, 2), Some(None));
        assert_eq!(rows.get(1, 1), Some(Some("é")));
        assert_eq!(rows.get(1, 3), None);
        assert_eq!(rows.get(2, 0), None);
        assert_eq!(
            rows.row(1).map(Row::to_vec),
            Some(vec![None, Some("é".into()), Some("x".into())])
        );
    }

    #[test]
    fn rows_are_squared_to_the_width() {
        let mut rows = Rows::new(2);
        rows.push([Some("a")]);
        rows.push([Some("b"), Some("c"), Some("dropped")]);
        rows.push([Some("d"), None]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.get(0, 1), Some(None));
        assert_eq!(rows.get(1, 1), Some(Some("c")));
        assert_eq!(rows.get(2, 0), Some(Some("d")));
        assert_eq!(rows.text_len(), 4);
    }

    #[test]
    fn rows_truncate_and_append_by_whole_rows() {
        let mut rows = Rows::new(2);
        rows.push([Some("a"), None]);
        rows.push([Some("bb"), Some("c")]);
        rows.push([Some("d"), Some("e")]);
        rows.truncate(2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.text_len(), 4);
        rows.truncate(5);
        assert_eq!(rows.len(), 2);

        let mut next = Rows::new(2);
        next.push([None, Some("f")]);
        next.push([Some("g"), Some("h")]);
        rows.append(&next);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.get(1, 0), Some(Some("bb")));
        assert_eq!(rows.get(2, 0), Some(None));
        assert_eq!(rows.get(2, 1), Some(Some("f")));
        assert_eq!(rows.get(3, 1), Some(Some("h")));

        let mut empty = Rows::default();
        empty.append(&next);
        assert_eq!((empty.width(), empty.len()), (2, 2));
    }

    #[test]
    fn rows_without_a_width_take_the_first_rows() {
        let mut rows = Rows::default();
        rows.push([Some("a"), Some("b")]);
        assert_eq!((rows.width(), rows.len()), (2, 1));
        assert!(Rows::default().is_empty());
    }
}
