//! The cells picked in a results grid, as DBeaver picks them: a click picks
//! one cell, Ctrl (⌘ on macOS) adds or removes one, Shift picks the block
//! from the last click, and Ctrl+Shift adds that block. The row numbers
//! pick whole rows the same way.

use std::collections::BTreeSet;

use egui::Modifiers;

/// A row and a column.
pub type CellPos = (usize, usize);

/// How a click changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Picks only what was clicked.
    Replace,
    /// Adds what was clicked, or removes it when already picked.
    Toggle,
    /// Picks the block from the anchor to what was clicked.
    Extend,
    /// Adds the block from the anchor to what was clicked.
    AddExtend,
}

impl Mode {
    pub fn from_modifiers(modifiers: Modifiers) -> Self {
        match (modifiers.command, modifiers.shift) {
            (false, false) => Self::Replace,
            (true, false) => Self::Toggle,
            (false, true) => Self::Extend,
            (true, true) => Self::AddExtend,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Ordered by row, then column, so a row's cells are one range.
    cells: BTreeSet<CellPos>,
    /// The cell clicked last, which editing follows. `None` after picking
    /// whole rows.
    pub focus: Option<CellPos>,
    /// Where Shift extends from.
    anchor: Option<CellPos>,
}

impl Selection {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn contains(&self, cell: CellPos) -> bool {
        self.cells.contains(&cell)
    }

    /// Whether any cell of `row` is picked.
    pub fn has_row(&self, row: usize) -> bool {
        self.cells.range((row, 0)..(row + 1, 0)).next().is_some()
    }

    /// Picked cells, by row then column.
    pub fn cells(&self) -> impl Iterator<Item = CellPos> + '_ {
        self.cells.iter().copied()
    }

    /// Rows with a picked cell, in order.
    pub fn rows(&self) -> Vec<usize> {
        let mut rows: Vec<usize> = self.cells.iter().map(|(row, _)| *row).collect();
        rows.dedup();
        rows
    }

    /// Columns with a picked cell, in order.
    pub fn columns(&self) -> Vec<usize> {
        let columns: BTreeSet<usize> = self.cells.iter().map(|(_, column)| *column).collect();
        columns.into_iter().collect()
    }

    /// A click on a cell.
    pub fn click(&mut self, cell: CellPos, mode: Mode) {
        let anchor = self.anchor.unwrap_or(cell);
        match mode {
            Mode::Replace => {
                self.cells.clear();
                self.cells.insert(cell);
                self.anchor = Some(cell);
            }
            Mode::Toggle => {
                if !self.cells.remove(&cell) {
                    self.cells.insert(cell);
                }
                self.anchor = Some(cell);
            }
            Mode::Extend => {
                self.cells.clear();
                self.add_block(anchor, cell);
            }
            Mode::AddExtend => self.add_block(anchor, cell),
        }
        self.focus = self.cells.contains(&cell).then_some(cell);
    }

    /// A click on a row number: the same, for whole rows of `columns`
    /// cells.
    pub fn click_row(&mut self, row: usize, columns: usize, mode: Mode) {
        if columns == 0 {
            return;
        }
        let last = columns - 1;
        let anchor = self.anchor.map_or(row, |(row, _)| row);
        match mode {
            Mode::Replace => {
                self.cells.clear();
                self.add_block((row, 0), (row, last));
                self.anchor = Some((row, 0));
            }
            Mode::Toggle => {
                let whole = (0..columns).all(|column| self.cells.contains(&(row, column)));
                if whole {
                    self.cells.retain(|(r, _)| *r != row);
                } else {
                    self.add_block((row, 0), (row, last));
                }
                self.anchor = Some((row, 0));
            }
            Mode::Extend => {
                self.cells.clear();
                self.add_block((anchor, 0), (row, last));
            }
            Mode::AddExtend => self.add_block((anchor, 0), (row, last)),
        }
        self.focus = None;
    }

    /// Every cell of a `rows` × `columns` grid.
    pub fn select_all(&mut self, rows: usize, columns: usize) {
        if rows == 0 || columns == 0 {
            return;
        }
        self.cells.clear();
        self.add_block((0, 0), (rows - 1, columns - 1));
        self.anchor = Some((0, 0));
    }

    /// Adds the block between two corners, inclusive; the anchor stays.
    fn add_block(&mut self, from: CellPos, to: CellPos) {
        let rows = from.0.min(to.0)..=from.0.max(to.0);
        let columns = from.1.min(to.1)..=from.1.max(to.1);
        for row in rows {
            for column in columns.clone() {
                self.cells.insert((row, column));
            }
        }
        if self.anchor.is_none() {
            self.anchor = Some(from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_picks_one_cell() {
        let mut selection = Selection::default();
        selection.click((1, 1), Mode::Replace);
        selection.click((2, 0), Mode::Replace);
        assert_eq!(selection.cells().collect::<Vec<_>>(), [(2, 0)]);
        assert_eq!(selection.focus, Some((2, 0)));
    }

    #[test]
    fn ctrl_adds_and_removes_cells() {
        let mut selection = Selection::default();
        selection.click((0, 0), Mode::Replace);
        selection.click((3, 2), Mode::Toggle);
        assert_eq!(selection.len(), 2);
        selection.click((0, 0), Mode::Toggle);
        assert_eq!(selection.cells().collect::<Vec<_>>(), [(3, 2)]);
        assert_eq!(selection.focus, None);
    }

    #[test]
    fn shift_picks_the_block_from_the_anchor() {
        let mut selection = Selection::default();
        selection.click((3, 2), Mode::Replace);
        selection.click((1, 1), Mode::Extend);
        assert_eq!(selection.len(), 6);
        assert_eq!(selection.rows(), [1, 2, 3]);
        assert_eq!(selection.columns(), [1, 2]);
        // The anchor stays: a second Shift click picks another block.
        selection.click((3, 3), Mode::Extend);
        assert_eq!(selection.cells().collect::<Vec<_>>(), [(3, 2), (3, 3)]);
    }

    #[test]
    fn ctrl_shift_adds_a_block() {
        let mut selection = Selection::default();
        selection.click((0, 0), Mode::Replace);
        selection.click((5, 5), Mode::Toggle);
        selection.click((6, 5), Mode::AddExtend);
        assert_eq!(
            selection.cells().collect::<Vec<_>>(),
            [(0, 0), (5, 5), (6, 5)]
        );
    }

    #[test]
    fn row_numbers_pick_whole_rows() {
        let mut selection = Selection::default();
        selection.click_row(2, 3, Mode::Replace);
        assert_eq!(selection.len(), 3);
        selection.click_row(4, 3, Mode::Extend);
        assert_eq!(selection.rows(), [2, 3, 4]);
        selection.click_row(3, 3, Mode::Toggle);
        assert_eq!(selection.rows(), [2, 4]);
        assert!(selection.has_row(2) && !selection.has_row(3));
    }
}
