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

use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

use csv::StringRecord;

use crate::{SheetRow, SheetVec, SortOrder, View, edit_record, filter_indices, sort_indices};

macro_rules! undo_token {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone)]
        pub struct $name {
            payload: Arc<dyn Any + Send + Sync>,
            size_bytes: usize,
        }

        impl $name {
            /// `size_bytes` is the memory this token keeps alive (its own
            /// struct plus heap data), which the undo history sums to bound
            /// its size. Backends compute it when they build the token.
            pub fn new<T: Any + Send + Sync>(payload: T, size_bytes: usize) -> Self {
                $name { payload: Arc::new(payload), size_bytes }
            }

            pub fn payload<T: Any>(&self) -> Option<&T> {
                self.payload.downcast_ref()
            }

            pub fn size_bytes(&self) -> usize {
                self.size_bytes
            }
        }
    };
}

undo_token! {
    /// What `remove_column` hands back so the removal can be undone with
    /// `restore_column`. Opaque: each backend keeps whatever is cheapest for it
    /// (an in-memory sheet keeps the removed values; a file-backed one keeps a
    /// copy of its small column map). Cheap to clone.
    RemovedColumn
}

undo_token! {
    /// What `insert_column` hands back so the insertion can be undone exactly
    /// with `undo_insert_column`, including the padding it added to short rows.
    InsertedColumn
}

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

    /// Remove field `col` from every row (a row with no field `col` is left
    /// alone). The result undoes it via [`Sheet::restore_column`].
    fn remove_column(&mut self, col: usize) -> RemovedColumn;

    /// Undo a `remove_column(col)`: put each removed value back at `col`, and
    /// touch no row that had no value there. Exact inverse provided the sheet is
    /// as it was right after the removal, i.e. everything done since has been
    /// undone in reverse order, which is how the app's undo stack works. In any
    /// other state the result is unspecified (but never panics).
    fn restore_column(&mut self, col: usize, removed: &RemovedColumn);

    /// Insert a new field at `col` into every row: the value from
    /// `values_by_row` (keyed by row id) where present, empty otherwise. A row
    /// with fewer than `col` fields is first padded with empty fields, so the
    /// new column is at index `col` in every row.
    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]) -> InsertedColumn;

    /// Undo an `insert_column(col, ..)` exactly: the new field is taken out of
    /// every row and the padding it added to short rows is taken off again.
    /// Same precondition as `restore_column`: everything done since has been
    /// undone in reverse order; otherwise the result is unspecified (but never
    /// a panic).
    fn undo_insert_column(&mut self, col: usize, inserted: &InsertedColumn);

    /// Positions of rows with any cell containing `needle`.
    fn filter(&self, needle: &str) -> View;

    /// `view` sorted by column `sort_by.0` (stable; numbers by value).
    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View;

    /// Memory held because of changes since the file was opened: rows copied
    /// in for editing, the row order, the column map. Read caches are not
    /// counted. An in-memory sheet cannot separate its edits from its data and
    /// reports 0.
    fn edit_bytes(&self) -> usize {
        0
    }

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

    fn remove_column(&mut self, col: usize) -> RemovedColumn {
        let mut removed: Vec<(usize, String)> = Vec::new();
        for (id, record) in self.rows.iter_mut() {
            if let Some(value) = record.get(col) {
                removed.push((*id, value.to_string()));
                *record = record
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != col)
                    .map(|(_, f)| f)
                    .collect();
            }
        }
        let size = std::mem::size_of::<Vec<(usize, String)>>()
            + removed.capacity() * std::mem::size_of::<(usize, String)>()
            + removed.iter().map(|(_, v)| v.capacity()).sum::<usize>();
        RemovedColumn::new(removed, size)
    }

    fn restore_column(&mut self, col: usize, removed: &RemovedColumn) {
        let Some(values) = removed.payload::<Vec<(usize, String)>>() else {
            return;
        };
        let values: std::collections::HashMap<usize, &str> =
            values.iter().map(|(id, v)| (*id, v.as_str())).collect();
        for (id, record) in self.rows.iter_mut() {
            if let Some(value) = values.get(id) {
                let mut fields: Vec<&str> = record.iter().collect();
                fields.resize(fields.len().max(col), "");
                fields.insert(col, value);
                *record = fields.into_iter().collect();
            }
        }
    }

    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]) -> InsertedColumn {
        let values: std::collections::HashMap<usize, &str> = values_by_row
            .iter()
            .map(|(id, v)| (*id, v.as_str()))
            .collect();
        let mut padded: Vec<(usize, usize)> = Vec::new();
        for (id, record) in self.rows.iter_mut() {
            let mut fields: Vec<&str> = record.iter().collect();
            if fields.len() < col {
                padded.push((*id, fields.len()));
            }
            // A row shorter than `col` is padded so the new column lines up.
            fields.resize(fields.len().max(col), "");
            fields.insert(col, values.get(id).copied().unwrap_or(""));
            *record = fields.into_iter().collect();
        }
        let size = std::mem::size_of::<Vec<(usize, usize)>>()
            + padded.capacity() * std::mem::size_of::<(usize, usize)>();
        InsertedColumn::new(padded, size)
    }

    fn undo_insert_column(&mut self, col: usize, inserted: &InsertedColumn) {
        let Some(padded) = inserted.payload::<Vec<(usize, usize)>>() else {
            return;
        };
        let padded: std::collections::HashMap<usize, usize> = padded.iter().copied().collect();
        for (id, record) in self.rows.iter_mut() {
            let mut fields: Vec<&str> = record.iter().collect();
            if col < fields.len() {
                fields.remove(col);
            }
            // Take off the padding the insert added to a short row.
            if let Some(&old_len) = padded.get(id) {
                fields.truncate(old_len);
            }
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
    fn remove_then_restore_column_is_an_exact_inverse() {
        let mut s = sheet(&[(0, &["a", "b", "c"]), (1, &["d", "e", "f"]), (2, &["g"])]);
        let token = s.remove_column(1);
        assert_eq!(s.record(0).unwrap().iter().collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(s.record(2).unwrap().iter().collect::<Vec<_>>(), ["g"]);
        s.restore_column(1, &token);
        assert_eq!(
            s.record(1).unwrap().iter().collect::<Vec<_>>(),
            ["d", "e", "f"]
        );
        assert_eq!(
            s.record(2).unwrap().iter().collect::<Vec<_>>(),
            ["g"],
            "the short row gains nothing"
        );
        s.insert_column(0, &[]);
        assert_eq!(
            s.record(0).unwrap().iter().collect::<Vec<_>>(),
            ["", "a", "b", "c"]
        );
    }

    #[test]
    fn restoring_with_a_token_from_another_kind_of_sheet_does_nothing() {
        let mut s = sheet(&[(0, &["a", "b"])]);
        s.restore_column(1, &RemovedColumn::new(42u8, 1));
        assert_eq!(s.record(0).unwrap().iter().collect::<Vec<_>>(), ["a", "b"]);
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

    #[test]
    fn empty_needle_matches_a_row_with_no_cells() {
        let s = sheet(&[(0, &["a"]), (1, &[])]);
        assert_eq!(s.filter(""), vec![0, 1]);
        assert_eq!(s.filter("a"), vec![0]);
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
