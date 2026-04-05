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

pub type SheetVec = Vec<StringRecord>;

pub enum UiMessage {
    OpenFile(String, Option<TabId>),
    FilterSheet(Filename, Filter, TabId, Option<usize>),
    SortSheet(Filename, (ColumnId, SortOrder), TabId),
    FilterGlobal(Filter),
    SetDisplayData(SheetVec, String, TabId),
    SetMaster(SheetVec, String),
    /// filename, tab_id, row_nr (in displayed data), actual col index, new value
    EditCell(Filename, TabId, u64, usize, String),
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
}

pub type Chan<Msg> = (Sender<Msg>, Receiver<Msg>);

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
