use csv::StringRecord;
use std::collections::HashMap;

use egui_dock::DockState;
use poll_promise::Promise;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Clone, Default)]
pub struct FileHeader {
    pub unique_vals: Vec<String>,
    pub name: String,
    pub visible: bool,
    pub dtype: Option<String>,
    pub sort: Option<SortOrder>,
    pub sort_dir: Option<bool>,
}

pub type TabId = usize;
pub type Filter = String;
pub type Filename = String;

pub enum UiMessage {
    OpenFile(String, usize),
    FilterData(Filename, Filter, TabId, Option<usize>),
}

pub type ArcSheet = Vec<Arc<StringRecord>>;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Default)]
pub struct SheetTab {
    pub id: usize,
    pub radio: String,
    pub filename: String,
    pub scroll_y: f32,
    pub inner_rect: f32,
    pub content_height: f32,
}

pub struct MyApp {
    pub columns: HashMap<(String, usize), Vec<FileHeader>>,
    pub filter: HashMap<(String, usize), String>,
    pub dropped_files: Vec<egui::DroppedFile>,
    pub picked_path: Option<String>,
    pub loading: bool,
    pub sender: Sender<UiMessage>,
    pub receiver: Receiver<UiMessage>,
    pub sort_by_column: Option<usize>,
    pub sort_order: Option<SortOrder>,
    pub promised_data: HashMap<String, Promise<Arc<ArcSheet>>>,
    pub filtered_data: HashMap<(String, usize), Promise<Arc<ArcSheet>>>,
    pub tree: DockState<SheetTab>,
    pub counter: usize,
}
