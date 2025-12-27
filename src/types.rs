use egui::Context;
use egui_dock::{DockState, NodeIndex, SurfaceIndex};
use polars::frame::DataFrame;
use std::collections::HashMap;

use std::sync::mpsc::{Receiver, Sender};

#[derive(Clone, Default)]
pub struct FileHeader {
    pub name: String,
    pub visible: bool,
    pub sort: Option<SortOrder>,
    // pub dtype: Option<String>,
    // pub unique_vals: Vec<String>,
    // pub sort_dir: Option<bool>,
}

pub type TabId = usize;
pub type ColumnId = usize;
pub type Filter = String;
pub type Filename = String;

pub type Ping = bool;

pub enum UiMessage {
    OpenFile(String, Option<TabId>),
    FilterSheet(Filename, Filter, TabId, Option<usize>),
    SortSheet(Filename, (ColumnId, SortOrder), TabId),
    FilterGlobal(Filter),
    SetSorted(DataFrame, String, TabId),
    SetMaster(DataFrame, String, Option<TabId>, Vec<FileHeader>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Default)]
pub struct SheetTab {
    pub id: usize,
    pub chosen_file: String,
    pub columns: HashMap<Filename, Vec<FileHeader>>,
}

pub type Chan<Msg> = (Sender<Msg>, Receiver<Msg>);

pub type Filters = HashMap<(Filename, TabId), String>;

pub struct MyApp {
    pub dropped_files: Vec<egui::DroppedFile>,
    pub picked_path: Option<String>,
    pub loading: bool,
    pub worker_chan: Chan<UiMessage>,
    pub ui_chan: Chan<Ping>,
    pub sort_by_column: Option<usize>,
    pub sort_order: Option<SortOrder>,
    pub master_data: HashMap<String, DataFrame>,
    // Filtered data is stored per file and per tab. Filtered data coming from
    // one master file can be used in many tabs. It's all Arcs, so
    // even if we show the same data in many tabs, filtered in different ways
    // we're just using references to the same master data.
    pub filtered_data: HashMap<(Filename, TabId), DataFrame>,
    pub tree: DockState<SheetTab>,
    pub counter: usize,
    pub files_list: Vec<String>,
    pub global_filter: String,
    pub filters: Filters,
    pub df: DataFrame,
    pub filtered_df: DataFrame,
}

pub struct CsvTabViewer<'a> {
    pub added_nodes: &'a mut Vec<(SurfaceIndex, NodeIndex, Filename)>,
    pub master_data: &'a HashMap<Filename, DataFrame>,
    pub filtered_data: &'a HashMap<(Filename, TabId), DataFrame>,
    pub ctx: &'a Context,
    pub sender: &'a Sender<UiMessage>,
    pub files_list: &'a Vec<String>,
    pub tabs_no: usize,
    pub focused_tab: Option<usize>,
    pub global_filter: &'a String,
    pub filters: &'a mut Filters,
    // pub df: &'a mut DataFrame,
    // pub filtered_df: &'a mut DataFrame,
}
