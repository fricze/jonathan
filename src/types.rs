use csv::StringRecord;
use egui::Context;
use egui_dock::{DockState, NodeIndex, SurfaceIndex};
use std::collections::{HashMap, HashSet};

use std::sync::mpsc::{Receiver, Sender};

#[derive(Clone, Default)]
pub struct FileHeader {
    pub name: String,
    pub visible: bool,
    pub sort: Option<SortOrder>,
}

pub type TabId = usize;
pub type ColumnId = usize;
pub type Filter = String;
pub type Filename = String;

pub type Ping = bool;

/// (master row index, record). The master index is assigned once when a file
/// is loaded and carried through sort/filter so edits to a filtered or sorted
/// view can always be written back to the correct row in master data.
pub type SheetRow = (usize, StringRecord);
pub type SheetVec = Vec<SheetRow>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReplaceScope {
    /// Only the given actual column index.
    CurrentColumn(ColumnId),
    AllColumns,
}

pub enum UiMessage {
    OpenFile(String, Option<TabId>),
    FilterSheet(Filename, Filter, TabId, Option<usize>),
    SortSheet(Filename, (ColumnId, SortOrder), TabId),
    FilterGlobal(Filter),
    /// Result of a background sort/filter. Carries the request epoch it was
    /// computed for so a stale result (superseded by a newer request for the
    /// same file+tab) can be dropped instead of racing to overwrite the
    /// latest one.
    SetDisplayData(SheetVec, String, TabId, u64),
    SetMaster(SheetVec, String),
    /// filename, tab_id, row_nr (in displayed data), actual col index, new value
    EditCell(Filename, TabId, u64, usize, String),
    /// filename, tab_id, anchor_row (in displayed data), anchor visible col
    /// index, pasted grid of values (row-major, grows right/down from anchor)
    PasteCells(Filename, TabId, u64, usize, Vec<Vec<String>>),
    /// filename, tab_id, text to find, replacement text, scope
    ReplaceAll(Filename, TabId, String, String, ReplaceScope),
    SaveFile(Filename),
    Undo,
    Redo,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

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

#[derive(Default)]
pub struct SheetTab {
    pub id: usize,
    pub chosen_file: String,
    pub columns: HashMap<Filename, Vec<FileHeader>>,
    /// Currently edited cell: (row_nr, visible col index)
    pub editing_cell: Option<(u64, usize)>,
    pub edit_buffer: String,
    pub selection: SelectionState,
    /// Last known visible row range (from previous frame's prepare())
    pub last_visible_rows: Option<std::ops::Range<u64>>,
    /// Replacement text for the find-and-replace panel; find text reuses
    /// this tab's filter text.
    pub replace_text: String,
    /// Whether replace-all applies to every column instead of just the one
    /// last sorted/clicked (see CsvTabViewer::ui replace scope handling).
    pub replace_all_columns: bool,
}

pub type Chan<Msg> = (Sender<Msg>, Receiver<Msg>);

/// Send a `UiMessage`, logging to stderr on failure instead of the caller
/// having to repeat the same `if let Err(e) = ... { eprintln!(...) }` boilerplate.
pub trait SendUiMessage {
    fn send_msg(&self, msg: UiMessage);
}

impl SendUiMessage for Sender<UiMessage> {
    fn send_msg(&self, msg: UiMessage) {
        if let Err(e) = self.send(msg) {
            eprintln!("Worker: Failed to send message to UI thread: {:?}", e);
        }
    }
}

pub type Filters = HashMap<(Filename, TabId), String>;

/// Returns the sheet data to display for a given file+tab:
/// - the filtered/sorted view if one exists
/// - master data if no filter is active
/// - an empty slice if a filter is pending but results haven't arrived yet
pub fn active_sheet_data<'a>(
    master: &'a HashMap<Filename, SheetVec>,
    filtered: &'a HashMap<(Filename, TabId), SheetVec>,
    filename: &str,
    tab_id: TabId,
    filter_active: bool,
) -> &'a SheetVec {
    use std::sync::LazyLock;
    static EMPTY: LazyLock<SheetVec> = LazyLock::new(Vec::new);
    match (master.get(filename), filtered.get(&(filename.to_string(), tab_id))) {
        (Some(_), None) if filter_active => &EMPTY,
        (Some(data), None) => data,
        (Some(_), Some(data)) => data,
        _ => &EMPTY,
    }
}

/// A single-cell edit that can be undone/redone. Holds the value the cell
/// had *before* the edit being recorded was applied, so undoing means
/// writing `old_value` back and redoing means re-applying whatever was
/// overwritten.
#[derive(Clone)]
pub struct UndoEntry {
    pub filename: Filename,
    pub master_row: usize,
    pub col: usize,
    pub old_value: String,
}

pub struct MyApp {
    pub picked_path: Option<String>,
    pub loading: bool,
    pub worker_chan: Chan<UiMessage>,
    pub ui_chan: Chan<Ping>,
    pub sheets_data: HashMap<String, SheetVec>,
    // Filtered/sorted views keyed by (filename, tab_id). Each tab can show
    // the same master file filtered or sorted differently.
    pub filtered_data: HashMap<(Filename, TabId), SheetVec>,
    pub tree: DockState<SheetTab>,
    pub counter: usize,
    pub files_list: Vec<String>,
    pub global_filter: String,
    pub filters: Filters,
    pub dirty_files: HashSet<Filename>,
    /// Bumped each time a sort/filter is requested for a (filename, tab_id);
    /// used to drop results from superseded background requests.
    pub request_epoch: HashMap<(Filename, TabId), u64>,
    /// Global undo/redo history for cell edits. A new edit clears redo_stack.
    pub undo_stack: Vec<UndoEntry>,
    pub redo_stack: Vec<UndoEntry>,
}

pub struct CsvTabViewer<'a> {
    pub added_nodes: &'a mut Vec<(SurfaceIndex, NodeIndex, Filename)>,
    pub promised_data: &'a HashMap<Filename, SheetVec>,
    pub filtered_data: &'a HashMap<(Filename, TabId), SheetVec>,
    pub ctx: &'a Context,
    pub sender: &'a Sender<UiMessage>,
    pub files_list: &'a Vec<String>,
    pub tabs_no: usize,
    pub focused_tab: Option<usize>,
    pub global_filter: &'a String,
    pub filters: &'a mut Filters,
    pub dirty_files: &'a HashSet<Filename>,
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

    #[test]
    fn active_sheet_data_prefers_filtered_view_when_present() {
        let mut master = HashMap::new();
        master.insert("f.csv".to_string(), vec![(0usize, StringRecord::from(vec!["m"]))]);
        let mut filtered = HashMap::new();
        filtered.insert(
            ("f.csv".to_string(), 1usize),
            vec![(0usize, StringRecord::from(vec!["filtered"]))],
        );

        let data = active_sheet_data(&master, &filtered, "f.csv", 1, true);
        assert_eq!(data[0].1.get(0), Some("filtered"));
    }

    #[test]
    fn active_sheet_data_falls_back_to_master_when_no_filter_active() {
        let mut master = HashMap::new();
        master.insert("f.csv".to_string(), vec![(0usize, StringRecord::from(vec!["m"]))]);
        let filtered = HashMap::new();

        let data = active_sheet_data(&master, &filtered, "f.csv", 1, false);
        assert_eq!(data[0].1.get(0), Some("m"));
    }

    #[test]
    fn active_sheet_data_returns_empty_when_filter_pending() {
        let mut master = HashMap::new();
        master.insert("f.csv".to_string(), vec![(0usize, StringRecord::from(vec!["m"]))]);
        let filtered = HashMap::new();

        let data = active_sheet_data(&master, &filtered, "f.csv", 1, true);
        assert!(data.is_empty());
    }
}
