//! Collecting a result's cells in the shape Violet stores them.

use crate::{DbError, Rows};

/// Marks a cell end as SQL `NULL`.
const NULL: u32 = 1 << 31;

/// Collects rows into [`Rows`]: one text buffer and the end of each cell,
/// which Violet takes without copying cell by cell.
///
/// ```
/// # use violet_sdk::RowsBuilder;
/// let mut rows = RowsBuilder::new(2, 1000);
/// rows.push([Some("1"), None]).unwrap();
/// assert_eq!(rows.len(), 1);
/// ```
#[derive(Debug, Default)]
pub struct RowsBuilder {
    width: usize,
    max_rows: usize,
    text: String,
    ends: Vec<u32>,
    rows: usize,
    truncated: bool,
}

impl RowsBuilder {
    /// Rows `width` cells wide, keeping at most `max_rows` of them.
    #[must_use]
    pub fn new(width: usize, max_rows: usize) -> Self {
        Self {
            width,
            max_rows,
            ..Self::default()
        }
    }

    /// Appends a row. Missing cells are `NULL` and extra ones are dropped.
    /// Past `max_rows` the row is not kept and the result is marked
    /// truncated; keep reading the server's answer and pushing, so later
    /// statements still run.
    pub fn push<'a>(
        &mut self,
        cells: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Result<(), DbError> {
        if self.rows >= self.max_rows {
            self.truncated = true;
            return Ok(());
        }
        let mut cells = cells.into_iter();
        for _ in 0..self.width {
            let end = match cells.next().flatten() {
                Some(text) => {
                    self.text.push_str(text);
                    self.offset()?
                }
                None => self.offset()? | NULL,
            };
            self.ends.push(end);
        }
        self.rows += 1;
        Ok(())
    }

    fn offset(&self) -> Result<u32, DbError> {
        u32::try_from(self.text.len())
            .ok()
            .filter(|len| len & NULL == 0)
            .ok_or_else(|| DbError::query("The result is larger than 2 GiB"))
    }

    /// Rows kept so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// No more rows will be kept.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.rows >= self.max_rows
    }

    /// Rows were dropped for going past `max_rows`.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub fn finish(self) -> Rows {
        Rows {
            text: self.text,
            ends: self.ends,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_padded_cut_and_capped() {
        let mut rows = RowsBuilder::new(2, 2);
        rows.push([Some("ab"), None]).expect("row");
        rows.push([Some("c")]).expect("short row");
        rows.push([Some("x"), Some("y")]).expect("over the cap");
        assert!(rows.truncated() && rows.is_full());
        let rows = rows.finish();
        assert_eq!(rows.text, "abc");
        assert_eq!(rows.ends, vec![2, 2 | NULL, 3, 3 | NULL]);
    }
}
