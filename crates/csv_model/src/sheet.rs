//! The storage boundary for a loaded file. Everything the app needs from the
//! rows of a file -- reading cells, editing, inserting/removing rows and
//! columns, filter/sort, saving -- goes through [`Sheet`], so the backing
//! store (today an in-memory `Vec`) can be swapped for a lazy, file-backed one
//! without touching the UI or the undo logic.
//!
//! Vocabulary: a row's *position* is its index in the sheet's current order
//! (shifts on insert/remove); its *id* is the stable master-row number
//! assigned at load and never reused. Columns are plain field indices; the
//! app resolves its own stable column ids to indices before calling in.

use std::borrow::Cow;

use csv::StringRecord;

use crate::{SheetRow, SheetVec, SortOrder, View, edit_record, filter_indices, sort_indices};

pub trait Sheet: Send + Sync {
    /// Number of rows.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stable id of the row at `position`.
    fn row_id(&self, position: usize) -> Option<usize>;

    /// One past the largest row id present (0 when empty). Backends that can
    /// answer without visiting every row should override this.
    fn next_row_id(&self) -> usize {
        (0..self.len())
            .filter_map(|i| self.row_id(i))
            .max()
            .map_or(0, |m| m + 1)
    }

    /// Current position of the row with stable id `row_id`.
    fn position_of(&self, row_id: usize) -> Option<usize>;

    /// Value of one cell, by row position and field index.
    fn cell(&self, position: usize, col: usize) -> Option<Cow<'_, str>>;

    /// The whole row at `position`.
    fn record(&self, position: usize) -> Option<Cow<'_, StringRecord>>;

    /// Overwrite one cell, addressed by stable row id. `false` if the row or
    /// column doesn't exist.
    fn set_cell(&mut self, row_id: usize, col: usize, value: &str) -> bool;

    /// Remove and return the row at `position`.
    fn remove_row(&mut self, position: usize) -> Option<SheetRow>;

    /// Insert `row` at `position` (clamped to the end).
    fn insert_row(&mut self, position: usize, row: SheetRow);

    /// Remove field `col` from every row. Returns each row's removed value
    /// (keyed by row id) so the removal can be undone.
    fn remove_column(&mut self, col: usize) -> Vec<(usize, String)>;

    /// Insert a field at `col` into every row: the value from
    /// `values_by_row` (keyed by row id) where present, empty otherwise.
    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]);

    /// Positions of rows with any cell containing `needle`.
    fn filter(&self, needle: &str) -> View;

    /// `view` sorted by column `sort_by.0` (stable; numbers by value).
    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View;

    /// A frozen copy that background threads can filter/sort while the UI
    /// keeps editing the original. Only the read methods are meaningful on it.
    fn snapshot(&self) -> Box<dyn Sheet>;
}

/// All rows held in memory as `StringRecord`s.
#[derive(Clone, Default)]
pub struct MemorySheet {
    rows: SheetVec,
}

impl MemorySheet {
    pub fn new(rows: SheetVec) -> Self {
        MemorySheet { rows }
    }

    pub fn rows(&self) -> &SheetVec {
        &self.rows
    }
}

impl From<SheetVec> for MemorySheet {
    fn from(rows: SheetVec) -> Self {
        MemorySheet { rows }
    }
}

impl Sheet for MemorySheet {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn row_id(&self, position: usize) -> Option<usize> {
        self.rows.get(position).map(|(id, _)| *id)
    }

    fn position_of(&self, row_id: usize) -> Option<usize> {
        self.rows.iter().position(|(id, _)| *id == row_id)
    }

    fn next_row_id(&self) -> usize {
        self.rows
            .iter()
            .map(|(id, _)| *id)
            .max()
            .map_or(0, |m| m + 1)
    }

    fn cell(&self, position: usize, col: usize) -> Option<Cow<'_, str>> {
        self.rows.get(position)?.1.get(col).map(Cow::Borrowed)
    }

    fn record(&self, position: usize) -> Option<Cow<'_, StringRecord>> {
        self.rows.get(position).map(|(_, r)| Cow::Borrowed(r))
    }

    fn set_cell(&mut self, row_id: usize, col: usize, value: &str) -> bool {
        edit_record(&mut self.rows, row_id, col, value)
    }

    fn remove_row(&mut self, position: usize) -> Option<SheetRow> {
        (position < self.rows.len()).then(|| self.rows.remove(position))
    }

    fn insert_row(&mut self, position: usize, row: SheetRow) {
        self.rows.insert(position.min(self.rows.len()), row);
    }

    fn remove_column(&mut self, col: usize) -> Vec<(usize, String)> {
        let removed = self
            .rows
            .iter()
            .filter_map(|(id, r)| r.get(col).map(|v| (*id, v.to_string())))
            .collect();
        for (_, record) in self.rows.iter_mut() {
            *record = record
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != col)
                .map(|(_, f)| f)
                .collect();
        }
        removed
    }

    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]) {
        let values: std::collections::HashMap<usize, &str> = values_by_row
            .iter()
            .map(|(id, v)| (*id, v.as_str()))
            .collect();
        for (id, record) in self.rows.iter_mut() {
            let mut fields: Vec<&str> = record.iter().collect();
            fields.insert(col.min(fields.len()), values.get(id).copied().unwrap_or(""));
            *record = fields.into_iter().collect();
        }
    }

    fn filter(&self, needle: &str) -> View {
        filter_indices(&self.rows, needle)
    }

    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View {
        sort_indices(&self.rows, view, sort_by)
    }

    fn snapshot(&self) -> Box<dyn Sheet> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(rows: &[(usize, &[&str])]) -> MemorySheet {
        MemorySheet::new(
            rows.iter()
                .map(|(id, f)| (*id, StringRecord::from(f.to_vec())))
                .collect(),
        )
    }

    #[test]
    fn ids_and_positions_diverge_after_removal() {
        let mut s = sheet(&[(0, &["a"]), (1, &["b"]), (2, &["c"])]);
        assert_eq!(s.remove_row(0).unwrap().0, 0);
        assert_eq!(s.row_id(0), Some(1));
        assert_eq!(s.position_of(2), Some(1));
        assert_eq!(s.position_of(0), None);
        assert_eq!(s.remove_row(9), None);
    }

    #[test]
    fn insert_row_clamps_and_cells_read_back() {
        let mut s = sheet(&[(0, &["a"])]);
        s.insert_row(99, (5, StringRecord::from(vec!["z"])));
        assert_eq!(s.len(), 2);
        assert_eq!(s.cell(1, 0).as_deref(), Some("z"));
        assert_eq!(s.cell(1, 7), None);
        assert_eq!(s.record(0).unwrap().get(0), Some("a"));
    }

    #[test]
    fn set_cell_addresses_by_row_id() {
        let mut s = sheet(&[(7, &["a", "b"]), (3, &["c", "d"])]);
        assert!(s.set_cell(3, 1, "X"));
        assert_eq!(s.cell(1, 1).as_deref(), Some("X"));
        assert!(!s.set_cell(99, 0, "no"));
        assert!(!s.set_cell(3, 9, "no"));
    }

    #[test]
    fn remove_and_insert_column_round_trip() {
        let mut s = sheet(&[(0, &["a", "b", "c"]), (1, &["d", "e", "f"])]);
        let removed = s.remove_column(1);
        assert_eq!(removed, vec![(0, "b".to_string()), (1, "e".to_string())]);
        assert_eq!(s.record(0).unwrap().iter().collect::<Vec<_>>(), ["a", "c"]);
        s.insert_column(1, &removed);
        assert_eq!(
            s.record(1).unwrap().iter().collect::<Vec<_>>(),
            ["d", "e", "f"]
        );
        s.insert_column(0, &[]);
        assert_eq!(
            s.record(0).unwrap().iter().collect::<Vec<_>>(),
            ["", "a", "b", "c"]
        );
    }

    #[test]
    fn next_row_id_is_one_past_the_largest_id() {
        assert_eq!(MemorySheet::default().next_row_id(), 0);
        let mut s = sheet(&[(7, &["a"]), (3, &["b"])]);
        assert_eq!(s.next_row_id(), 8);
        // Removing the largest id must not let it be handed out again by
        // the sheet itself; the app tracks that separately, so this only
        // reports what is present.
        s.remove_row(0);
        assert_eq!(s.next_row_id(), 4);
    }

    #[test]
    fn snapshot_is_unaffected_by_later_edits() {
        let mut s = sheet(&[(0, &["a"])]);
        let snap = s.snapshot();
        s.set_cell(0, 0, "changed");
        assert_eq!(snap.cell(0, 0).as_deref(), Some("a"));
    }

    #[test]
    fn filter_and_sort_delegate() {
        let s = sheet(&[(0, &["10"]), (1, &["2"]), (2, &["x"])]);
        assert_eq!(s.filter("2"), vec![1]);
        assert_eq!(s.sort(vec![0, 1, 2], (0, SortOrder::Asc)), vec![1, 0, 2]);
    }

    fn make_memory(rows: SheetVec) -> Box<dyn Sheet> {
        Box::new(MemorySheet::new(rows))
    }

    #[test]
    fn memory_sheet_meets_the_sheet_contract() {
        crate::conformance::run_all(&make_memory);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(300))]

        #[test]
        fn memory_sheet_agrees_with_the_reference_model(
            initial in crate::conformance::initial_rows(),
            ops in crate::conformance::ops(),
        ) {
            crate::conformance::check_ops(&make_memory, &initial, &ops);
        }
    }
}
