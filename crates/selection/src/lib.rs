//! Cell-selection geometry for a grid: single/toggle/range selection and
//! drag-to-select over `(row, col)` coordinates. No UI dependency -- every
//! method is plain state transition, independently testable without egui.

use std::collections::HashSet;

#[derive(Default)]
pub struct SelectionState {
    pub selected_cells: HashSet<(u64, usize)>,
    /// Fixed corner for range operations (keyboard nav, shift+click, drag)
    pub anchor_cell: Option<(u64, usize)>,
    /// Movable corner of the selection rectangle
    pub selection_end: Option<(u64, usize)>,
    /// Cell where a drag-select started
    pub drag_origin: Option<(u64, usize)>,
}

impl SelectionState {
    /// The current movable corner: `selection_end` if set, otherwise `anchor_cell`.
    pub fn cursor(&self) -> Option<(u64, usize)> {
        self.selection_end.or(self.anchor_cell)
    }

    pub fn contains(&self, row: u64, col: usize) -> bool {
        self.selected_cells.contains(&(row, col))
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_origin.is_some()
    }

    /// Clear everything and select a single cell, resetting the anchor.
    pub fn select_single(&mut self, row: u64, col: usize) {
        self.selected_cells.clear();
        self.selected_cells.insert((row, col));
        self.anchor_cell = Some((row, col));
        self.selection_end = None;
    }

    /// Toggle a cell in/out of the selection; updates anchor but keeps other cells.
    pub fn toggle(&mut self, row: u64, col: usize) {
        if self.selected_cells.contains(&(row, col)) {
            self.selected_cells.remove(&(row, col));
        } else {
            self.selected_cells.insert((row, col));
        }
        self.anchor_cell = Some((row, col));
    }

    /// Fill the rectangle from `anchor_cell` to `(row, col)` and update `selection_end`.
    /// Falls back to `select_single` if there is no anchor yet.
    pub fn extend_to(&mut self, row: u64, col: usize) {
        if let Some((anchor_row, anchor_col)) = self.anchor_cell {
            self.fill_rect(anchor_row, anchor_col, row, col);
            self.selection_end = Some((row, col));
        } else {
            self.select_single(row, col);
        }
    }

    pub fn start_drag(&mut self, row: u64, col: usize) {
        self.drag_origin = Some((row, col));
        self.anchor_cell = Some((row, col));
        self.selection_end = None;
        self.selected_cells.clear();
        self.selected_cells.insert((row, col));
    }

    /// Extend the drag rectangle from `drag_origin` to `(row, col)`.
    pub fn update_drag(&mut self, row: u64, col: usize) {
        if let Some((origin_row, origin_col)) = self.drag_origin {
            self.fill_rect(origin_row, origin_col, row, col);
            self.selection_end = Some((row, col));
        }
    }

    pub fn end_drag(&mut self) {
        self.drag_origin = None;
    }

    fn fill_rect(&mut self, r1: u64, c1: usize, r2: u64, c2: usize) {
        let row_min = r1.min(r2);
        let row_max = r1.max(r2);
        let col_min = c1.min(c2);
        let col_max = c1.max(c2);
        self.selected_cells.clear();
        for r in row_min..=row_max {
            for c in col_min..=col_max {
                self.selected_cells.insert((r, c));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_single_clears_previous_selection() {
        let mut s = SelectionState::default();
        s.extend_to(0, 0);
        s.select_single(3, 3);
        assert!(s.selected_cells.contains(&(3, 3)));
        assert_eq!(s.selected_cells.len(), 1);
        assert_eq!(s.anchor_cell, Some((3, 3)));
        assert_eq!(s.selection_end, None);
    }

    #[test]
    fn extend_to_fills_rectangle_from_anchor() {
        let mut s = SelectionState::default();
        s.select_single(1, 1);
        s.extend_to(3, 2);
        for r in 1..=3 {
            for c in 1..=2 {
                assert!(s.contains(r, c), "expected ({r},{c}) selected");
            }
        }
        assert_eq!(s.selected_cells.len(), 6);
        assert_eq!(s.cursor(), Some((3, 2)));
    }

    #[test]
    fn toggle_adds_and_removes_without_clearing_others() {
        let mut s = SelectionState::default();
        s.select_single(0, 0);
        s.toggle(1, 1);
        assert!(s.contains(0, 0));
        assert!(s.contains(1, 1));
        s.toggle(1, 1);
        assert!(!s.contains(1, 1));
        assert!(s.contains(0, 0));
    }

    #[test]
    fn drag_updates_rectangle_and_ends_cleanly() {
        let mut s = SelectionState::default();
        s.start_drag(0, 0);
        s.update_drag(2, 2);
        assert_eq!(s.selected_cells.len(), 9);
        assert!(s.is_dragging());
        s.end_drag();
        assert!(!s.is_dragging());
    }
}
