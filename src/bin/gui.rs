#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use csv::Reader;
use csv::StringRecord;
use egui::{Color32, Key, ScrollArea, TextFormat};
use itertools::Itertools;
use std::cmp::Ordering;
use std::str::FromStr;
use std::sync::Arc;

use std::sync::Mutex;

use egui::scroll_area::ScrollAreaOutput;
use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::{
    collections::HashSet,
    ops::{Add, Sub},
};

use egui_extras::{Column, Table, TableBuilder};

use eframe::egui;
use jonathan::read_csv;

enum WorkerMessage {
    SetData(()),
}

enum UiMessage {
    OpenFile(String),
    LoadPage(usize),
    FilterData(String),
    FilterColumns(HashSet<usize>),
}

fn open_csv_file(path: &str) -> (Reader<File>, Vec<FileHeader>) {
    match read_csv::iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .map(|name| FileHeader {
                    name: name.to_string(),
                    visible: true,
                    sort: None,
                    sort_dir: None,
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
    fn new(
        cc: &eframe::CreationContext<'_>,
        sheet_data: Arc<Mutex<Vec<StringRecord>>>,
        header_data: Arc<Mutex<Vec<FileHeader>>>,
    ) -> Self {
        let (tx_to_worker, rx_from_ui) = mpsc::channel::<UiMessage>();
        let (tx_to_ui, rx_from_worker) = mpsc::channel::<WorkerMessage>();

        let sheet = Arc::clone(&sheet_data);
        let header = Arc::clone(&header_data);

        let sheet_data = Arc::clone(&sheet_data);
        let header_data = Arc::clone(&header_data);

        thread::spawn(move || {
            let mut file_reader: Option<Reader<File>> = None;

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

                        let mut mut_data = sheet_data.lock().unwrap();
                        let mut header_ref = header_data.lock().unwrap();
                        *mut_data = new_data.clone();
                        *header_ref = headers.clone();

                        if let Err(e) = tx_to_ui.send(WorkerMessage::SetData(())) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                            break; // UI thread probably disconnected, exit worker
                        }
                    }
                    UiMessage::LoadPage(page_number) => {
                        // let page_handle = sheet_data.lock().unwrap();
                        // let page = page_handle
                        //     .iter()
                        //     .skip(page_number * 100)
                        //     .take(100)
                        //     .collect::<Vec<_>>();

                        // if let Err(e) = tx_to_ui.send(WorkerMessage::SetData(page)) {
                        //     eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        //     break; // UI thread probably disconnected, exit worker
                        // }
                    }
                    UiMessage::FilterData(filter) => {}
                    UiMessage::FilterColumns(hidden) => {
                        // let headers = header_data
                        //     .lock()
                        //     .unwrap()
                        //     .iter()
                        //     .enumerate()
                        //     .filter(|(index, _)| !hidden.contains(index))
                        //     .map(|(_, row)| row)
                        //     .collect::<Vec<_>>();

                        // let new_data = sheet_data
                        //     .lock()
                        //     .unwrap()
                        //     .iter()
                        //     .map(|record| {
                        //         return record
                        //             .iter()
                        //             .enumerate()
                        //             .filter(|(index, _)| !hidden.contains(index))
                        //             .map(|(_, value)| value)
                        //             .collect::<Vec<_>>();
                        //     })
                        //     .collect::<Vec<_>>();

                        // if let Err(e) =
                        //     tx_to_ui.send(WorkerMessage::SetHeadersAndData(headers, new_data))
                        // {
                        //     eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        //     break; // UI thread probably disconnected, exit worker
                        // }
                    }
                }
            }

            println!("Worker: Exiting.");
        });

        MyApp {
            filename: "".to_owned(),
            headers: None,
            columns: None,
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
            sort_by_column: None,
            sort_order: None,
            sheet_data: Arc::clone(&sheet),
            header_data: Arc::clone(&header),
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
    let sheet_data: Arc<Mutex<Vec<StringRecord>>> = Arc::new(Mutex::new(vec![]));
    let header_data: Arc<Mutex<Vec<FileHeader>>> = Arc::new(Mutex::new(vec![]));

    eframe::run_native(
        title,
        options,
        Box::new(|cc| Ok(Box::new(MyApp::new(cc, sheet_data, header_data)))), // <-- Wrap in Ok()
    )
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Clone)]
struct FileHeader {
    name: String,
    visible: bool,
    sort: Option<SortOrder>,
    sort_dir: Option<bool>,
}

struct MyApp {
    filename: String,
    headers: Option<Vec<FileHeader>>,
    columns: Option<Vec<FileHeader>>,
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
    sort_by_column: Option<usize>,
    sort_order: Option<SortOrder>,
    sheet_data: Arc<Mutex<Vec<StringRecord>>>,
    header_data: Arc<Mutex<Vec<FileHeader>>>,
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
    sheet_data: &Arc<Mutex<Vec<StringRecord>>>,
    sort_order: SortOrder,
    sort_by_column: Option<usize>,
) -> ScrollAreaOutput<()> {
    return table_ui.body(|body| {
        let guard = sheet_data.lock().unwrap();

        let rows = if let Some(column) = sort_by_column {
            guard
                .iter()
                .sorted_by(|a, b| {
                    let a = a.get(column).unwrap_or("");
                    let b = b.get(column).unwrap_or("");

                    match (f32::from_str(a), f32::from_str(b)) {
                        (Ok(a_f), Ok(b_f)) => match sort_order {
                            SortOrder::Asc => a_f.partial_cmp(&b_f).unwrap_or(Ordering::Equal),
                            SortOrder::Dsc => b_f.partial_cmp(&a_f).unwrap_or(Ordering::Equal),
                        },
                        (_, _) => match sort_order {
                            SortOrder::Asc => a.cmp(b),
                            SortOrder::Dsc => b.cmp(a),
                        },
                    }
                })
                .collect::<Vec<_>>()
        } else {
            guard.iter().collect::<Vec<_>>()
        };

        body.rows(18.0, rows.len(), |mut row| {
            let row_index = row.index();

            let row_data = &rows[row_index];

            row_data.iter().enumerate().for_each(|(_, text)| {
                row.col(|ui| {
                    if filter.is_empty() {
                        ui.label(text);
                    } else {
                        use egui::text::LayoutJob;

                        if text.contains(&filter) {
                            let mut job = LayoutJob::default();

                            if text == filter {
                                job.append(
                                    text,
                                    0.0,
                                    TextFormat {
                                        color: Color32::YELLOW,
                                        ..Default::default()
                                    },
                                );

                                ui.label(job);
                            } else {
                                let text: Vec<&str> = text.split(filter).collect();

                                if text.len() == 1 {
                                    job.append(
                                        filter,
                                        0.0,
                                        TextFormat {
                                            color: Color32::YELLOW,
                                            ..Default::default()
                                        },
                                    );
                                    job.append(text[0], 0.0, TextFormat::default());
                                    ui.label(job);
                                } else if text.len() == 2 {
                                    job.append(text[0], 0.0, TextFormat::default());
                                    job.append(
                                        filter,
                                        0.0,
                                        TextFormat {
                                            color: Color32::YELLOW,
                                            ..Default::default()
                                        },
                                    );
                                    job.append(text[1], 0.0, TextFormat::default());

                                    ui.label(job);
                                }
                            }
                        } else {
                            ui.label(text);
                        }
                    }
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
                    WorkerMessage::SetData(data) => {
                        ctx.request_repaint();
                    }
                }
            }

            if let Some(picked_path) = &self.picked_path {
                ui.label(format!("CSV reader :: {}", picked_path));
            } else {
                ui.label(format!("CSV reader"));
            }

            if let Some(headers) = self.columns.as_mut() {
                ui.horizontal_wrapped(|ui| {
                    let mut hidden = HashSet::new();

                    for (index, file_header) in headers.iter_mut().enumerate() {
                        if file_header.visible {
                            hidden.remove(&index);
                        } else {
                            hidden.insert(index);
                        }

                        if ui
                            .checkbox(&mut file_header.visible, &file_header.name)
                            .on_hover_text(format!("Show/hide column {}", file_header.name))
                            .clicked()
                        {
                            if file_header.visible {
                                hidden.remove(&index);
                            } else {
                                hidden.insert(index);
                            }

                            if let Err(e) = self
                                .sender_to_worker
                                .send(UiMessage::FilterColumns(hidden.clone()))
                            {
                                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                            }
                        }
                    }
                });
            }

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
            }

            if ui.button("Open file…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    let file_name = path.display().to_string();
                    self.picked_path = Some(file_name.clone());

                    let (_, headers) = open_csv_file(&file_name);

                    self.columns = Some(headers);

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

            ui.text_edit_singleline(&mut self.filter);

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

                let mut headers = self
                    .header_data
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect::<Vec<FileHeader>>();

                for _ in headers.iter().enumerate() {
                    table = table.column(Column::auto());
                }

                let table_ui = table.header(20.0, |mut header| {
                    headers
                        .iter_mut()
                        .enumerate()
                        .for_each(|(idx, file_header)| {
                            header.col(|ui| {
                                egui::Sides::new().show(
                                    ui,
                                    |ui| {
                                        let name = &file_header.name;
                                        ui.heading(name);
                                    },
                                    |ui| {
                                        let asc = file_header.sort_dir.unwrap_or(false);

                                        if ui.button(if asc { "⬆" } else { "⬇" }).clicked() {
                                            file_header.sort_dir = Some(!asc);

                                            if asc {
                                                self.sort_order = Some(SortOrder::Asc);
                                                self.sort_by_column = Some(idx);
                                            } else {
                                                self.sort_order = Some(SortOrder::Dsc);
                                                self.sort_by_column = Some(idx);
                                            }
                                        }
                                    },
                                );
                            });
                        });
                });

                let scroll_area = display_table(
                    table_ui,
                    &self.filter,
                    &self.sheet_data,
                    self.sort_order.unwrap_or(SortOrder::Dsc),
                    self.sort_by_column,
                );

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
