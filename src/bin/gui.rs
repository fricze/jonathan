#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use csv::Reader;
use csv::StringRecord;
use egui::Key;
use egui::ScrollArea;
use std::rc::Rc;

use std::cell::RefCell;

use egui::Ui;
use egui::scroll_area::ScrollAreaOutput;
use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use std::{
    collections::HashSet,
    ops::{Add, Sub},
};

use egui_extras::{Column, Table, TableBuilder};

use eframe::egui;
use jonathan::read_csv;

enum WorkerMessage {
    FilteredData(Vec<String>),
    SetHeaders(Vec<FileHeader>),
    SetData(Vec<StringRecord>),
    // Could add other messages like Progress(f32), Error(String), etc.
}

enum UiMessage {
    OpenFile(String),
    LoadPage(usize),
    FilterData(String),
}

fn open_csv_file(path: &str) -> (Reader<File>, Vec<FileHeader>) {
    match read_csv::iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .map(|name| FileHeader {
                    name: name.to_string(),
                    visible: true,
                })
                .collect::<Vec<_>>();
            return (csv_reader, headers);
        }
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    };
}

impl MyApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx_to_worker, rx_from_ui) = mpsc::channel::<UiMessage>();
        let (tx_to_ui, rx_from_worker) = mpsc::channel::<WorkerMessage>();

        thread::spawn(move || {
            let mut file_reader: Option<Reader<File>> = None;
            let data: Rc<RefCell<Vec<StringRecord>>> = Rc::new(RefCell::new(vec![]));
            // The background thread will continuously listen for new filter text
            for ui_message in rx_from_ui {
                match ui_message {
                    UiMessage::OpenFile(file_name) => {
                        let (reader, headers) = open_csv_file(&file_name);
                        file_reader = Some(reader);

                        let new_data = file_reader
                            .as_mut()
                            .unwrap()
                            .records()
                            .filter_map(|record| record.ok())
                            .collect::<Vec<_>>();

                        // let first_page = new_data.iter().take(10000).cloned().collect();
                        let first_page = new_data.iter().cloned().collect();

                        let mut mut_data = data.borrow_mut();
                        *mut_data = new_data;

                        if let Err(e) = tx_to_ui.send(WorkerMessage::SetData(first_page)) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                            break; // UI thread probably disconnected, exit worker
                        }

                        if let Err(e) = tx_to_ui.send(WorkerMessage::SetHeaders(headers)) {
                            eprintln!("Worker: Failed to send filtered data to UI thread: {:?}", e);
                            break; // UI thread probably disconnected, exit worker
                        }
                    }
                    UiMessage::LoadPage(page_number) => {
                        let page_handle = data.borrow();
                        let page = page_handle
                            .iter()
                            .cloned()
                            .skip(page_number * 100)
                            .take(100)
                            .collect::<Vec<_>>();

                        if let Err(e) = tx_to_ui.send(WorkerMessage::SetData(page)) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                            break; // UI thread probably disconnected, exit worker
                        }
                    }
                    UiMessage::FilterData(filter) => {}
                }
            }

            println!("Worker: Exiting.");
        });

        MyApp {
            filename: "".to_owned(),
            headers: None,
            data: None,
            scroll_y: 0.0,
            inner_rect: 0.0,
            content_height: 0.0,
            filter: "".to_owned(),
            dropped_files: Vec::new(),
            picked_path: None,
            page: 0,
            loading: false,
            reader: None,
            sender_to_worker: tx_to_worker,
            receiver_from_worker: rx_from_worker,
            reversed: false,
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 240.0]) // wide enough for the drag-drop overlay text
            .with_drag_and_drop(true),
        ..Default::default()
    };

    let title = &format!("CSV Reader.");

    eframe::run_native(
        title,
        options,
        Box::new(|cc| Ok(Box::new(MyApp::new(cc)))), // <-- Wrap in Ok()
    )
}

struct FileHeader {
    name: String,
    visible: bool,
}

struct MyApp {
    filename: String,
    headers: Option<Vec<FileHeader>>,
    data: Option<Vec<StringRecord>>,
    scroll_y: f32,
    inner_rect: f32,
    content_height: f32,
    filter: String,
    dropped_files: Vec<egui::DroppedFile>,
    picked_path: Option<String>,
    page: usize,
    loading: bool,
    reader: Option<csv::Reader<File>>,
    // Channel for sending messages from the UI thread to the background thread
    sender_to_worker: Sender<UiMessage>,
    // Channel for receiving messages from the background thread to the UI thread
    receiver_from_worker: Receiver<WorkerMessage>,
    reversed: bool,
}

fn preview_files_being_dropped(ctx: &egui::Context) {
    use egui::{Align2, Color32, Id, LayerId, Order, TextStyle};
    use std::fmt::Write as _;

    if !ctx.input(|i| i.raw.hovered_files.is_empty()) {
        let text = ctx.input(|i| {
            let mut text = "Dropping files:\n".to_owned();
            for file in &i.raw.hovered_files {
                if let Some(path) = &file.path {
                    write!(text, "\n{}", path.display()).ok();
                } else if !file.mime.is_empty() {
                    write!(text, "\n{}", file.mime).ok();
                } else {
                    text += "\n???";
                }
            }
            text
        });

        let painter =
            ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("file_drop_target")));

        let screen_rect = ctx.screen_rect();
        painter.rect_filled(screen_rect, 0.0, Color32::from_black_alpha(192));
        painter.text(
            screen_rect.center(),
            Align2::CENTER_CENTER,
            text,
            TextStyle::Heading.resolve(&ctx.style()),
            Color32::WHITE,
        );
    }
}

fn display_table(
    table_ui: Table,
    filter: &str,
    data: &Vec<StringRecord>,
    hidden: HashSet<usize>,
) -> ScrollAreaOutput<()> {
    return table_ui.body(|body| {
        let filtered_rows = data
            .iter()
            .filter(|row| row.iter().find(|text| text.contains(filter)).is_some())
            .collect::<Vec<_>>();

        body.rows(18.0, filtered_rows.len(), |mut row| {
            let row_index = row.index();

            let row_data = &filtered_rows[row_index];

            row_data
                .iter()
                .enumerate()
                .filter(|(index, _)| !hidden.contains(index))
                .for_each(|(_, data)| {
                    row.col(|ui| {
                        ui.label(data);
                    });
                });
        });
    });
}

fn show_dropped_files(ui: &mut egui::Ui, dropped_files: &Vec<egui::DroppedFile>) {
    if !dropped_files.is_empty() {
        ui.group(|ui| {
            ui.label("Dropped files:");

            for file in dropped_files {
                let mut info = if let Some(path) = &file.path {
                    path.display().to_string()
                } else if !file.name.is_empty() {
                    file.name.clone()
                } else {
                    "???".to_owned()
                };

                let mut additional_info = vec![];

                if file.mime != "csv" {
                    additional_info.push(format!("type: {}", file.mime));
                }

                if !file.mime.is_empty() {
                    additional_info.push(format!("type: {}", file.mime));
                }
                if let Some(bytes) = &file.bytes {
                    additional_info.push(format!("{} bytes", bytes.len()));
                }
                if !additional_info.is_empty() {
                    info += &format!(" ({})", additional_info.join(", "));
                }

                ui.label(info);
            }
        });
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            while let Ok(message) = self.receiver_from_worker.try_recv() {
                match message {
                    WorkerMessage::SetHeaders(data) => {
                        self.headers = Some(data);
                        ctx.request_repaint();
                    }
                    WorkerMessage::FilteredData(data) => {}
                    WorkerMessage::SetData(data) => {
                        self.data = Some(data);
                        ctx.request_repaint();
                    }
                }
            }

            ui.label(format!("CSV reader :: {}", self.filename,));

            if let Some(headers) = self.headers.as_mut() {
                ui.horizontal_wrapped(|ui| {
                    for file_header in headers.iter_mut() {
                        ui.checkbox(&mut file_header.visible, &file_header.name)
                            .on_hover_text(format!("Show/hide column {}", file_header.name));
                    }
                });
            }

            ui.label("Drag-and-drop files onto the window!");

            if let Some(picked_path) = &self.picked_path {
                if self.filter.is_empty()
                    && self.inner_rect.add(self.scroll_y) >= self.content_height
                {
                    let page_no = self.page + 1;

                    // if let Err(e) = self.sender_to_worker.send(UiMessage::LoadPage(page_no)) {
                    //     eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    // } else {
                    //     self.page = self.page + 1;
                    // }
                }

                ui.horizontal(|ui| {
                    ui.label("Picked file:");
                    ui.monospace(picked_path);
                });
            }

            if ui.button("Open file…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    let str_path = path.to_str().unwrap_or("");
                    let file_name = path.display().to_string();
                    self.picked_path = Some(file_name.clone());

                    if let Err(e) = self
                        .sender_to_worker
                        .send(UiMessage::OpenFile(file_name.clone()))
                    {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }

                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
                }
            }

            show_dropped_files(ui, &self.dropped_files);

            ui.separator();

            let response = ui.text_edit_singleline(&mut self.filter);
            if response.changed() {
                // self.filter_data();
            }

            ui.separator();

            ScrollArea::horizontal().show(ui, |ui| {
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .min_scrolled_height(0.0);

                if ctx.input(|i| i.key_pressed(Key::PageUp)) {
                    table = table
                        .vertical_scroll_offset(self.scroll_y.sub(self.inner_rect / 2.0).max(0.0));
                }

                if ctx.input(|i| i.key_pressed(Key::PageDown)) {
                    table = table.vertical_scroll_offset(self.scroll_y.add(self.inner_rect / 2.0));
                }

                if ctx.input(|i| i.key_pressed(Key::Home)) {
                    table = table.vertical_scroll_offset(0.0);
                }

                if ctx.input(|i| i.key_pressed(Key::End)) {
                    table = table.vertical_scroll_offset(self.content_height);
                }

                let empty_headers: Vec<FileHeader> = vec![];
                let headers = self.headers.as_ref().unwrap_or(&empty_headers);

                let mut hidden = HashSet::new();

                for (index, file_header) in headers.iter().enumerate() {
                    if file_header.visible {
                        table = table.column(Column::auto());
                    } else {
                        hidden.insert(index);
                    }
                }

                let table_ui = table.header(20.0, |mut header| {
                    for file_header in headers.iter().filter(|header| header.visible) {
                        header.col(|ui| {
                            egui::Sides::new().show(
                                ui,
                                |ui| {
                                    ui.heading(&file_header.name);
                                },
                                |ui| {
                                    self.reversed ^=
                                        ui.button(if self.reversed { "⬆" } else { "⬇" }).clicked();
                                },
                            );
                        });
                    }
                });

                let empty_data: Vec<StringRecord> = vec![];
                let data = self.data.as_ref().unwrap_or(&empty_data);

                let scroll_area = display_table(table_ui, &self.filter, data, hidden);

                let content_height = scroll_area.content_size[1];

                self.content_height = content_height;

                let offset = scroll_area.state.offset[1];
                self.scroll_y = offset;
                self.inner_rect = scroll_area.inner_rect.height();
            });
        });

        preview_files_being_dropped(ctx);

        let mut file_name = None;
        // Collect dropped files:
        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                let files = &i.raw.dropped_files;
                self.dropped_files.clone_from(files);

                let default_path = PathBuf::default();
                let path = files[0].path.as_ref().unwrap_or(&default_path);
                file_name = Some(path.display().to_string());

                let str_path = path.to_str().unwrap_or("");

                // open_csv_file(self, str_path);
            }
        });

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            file_name.unwrap_or("".to_string()),
        ));
    }
}
