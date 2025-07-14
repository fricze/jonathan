#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use polars::df;
use polars::prelude::{AnyValue, CsvReadOptions, CsvWriter, DataFrame, SerReader, SerWriter};

use crate::egui::Context;
use csv::Reader;
use csv::StringRecord;
use egui::{Color32, Key, ScrollArea, TextFormat};
use itertools::Itertools;
use shared_arena::{ArenaArc, SharedArena};
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
    SetDf(DataFrame),
}

enum UiMessage {
    OpenFile(String),
    FilterData(String, Option<usize>),
    FilterColumns(HashSet<usize>),
}

// Demonstrates how to replace all fonts.
fn replace_fonts(ctx: &egui::Context) {
    // Start with the default fonts (we will be adding to them rather than replacing them).
    let mut fonts = egui::FontDefinitions::default();

    // Install my own font (maybe supporting non-latin characters).
    // .ttf and .otf files supported.
    fonts.font_data.insert(
        "IBMPlex".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../IBM_Plex_Mono/IBMPlexMono-Regular.ttf"
        ))),
    );

    // Put my font first (highest priority) for proportional text:
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "IBMPlex".to_owned());

    // Put my font as last fallback for monospace:
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "IBMPlex".to_owned());

    // .push("IBMPlex".to_owned());

    // Tell egui to use these fonts:
    ctx.set_fonts(fonts);
}

fn open_csv_file(path: &str) -> (Reader<File>, Vec<FileHeader>) {
    match read_csv::iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .map(|name| FileHeader {
                    name: name.to_string(),
                    visible: true,
                    ..FileHeader::default()
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

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 240.0]) // wide enough for the drag-drop overlay text
            .with_drag_and_drop(true),
        ..Default::default()
    };

    let title = &format!("CSV Reader.");

    let sheet_master: Arc<Mutex<Vec<ArenaArc<StringRecord>>>> = Arc::new(Mutex::new(vec![]));
    let sheet_filtered: Arc<Mutex<Vec<ArenaArc<StringRecord>>>> = Arc::new(Mutex::new(vec![]));

    let header_data: Arc<Mutex<Vec<FileHeader>>> = Arc::new(Mutex::new(vec![]));

    let csv_arena: Arc<Mutex<SharedArena<StringRecord>>> = Arc::new(Mutex::new(SharedArena::new()));

    let ui_chan = mpsc::channel::<UiMessage>();
    let worker_chan = mpsc::channel::<WorkerMessage>();

    let arena = Arc::clone(&csv_arena);

    eframe::run_native(
        title,
        options,
        Box::new(|cc| {
            replace_fonts(&cc.egui_ctx);

            let t_sheet_master = Arc::clone(&sheet_master);
            let t_sheet_filtered = Arc::clone(&sheet_filtered);

            let t_header_data = Arc::clone(&header_data);
            let t_csv_arena = Arc::clone(&arena); // Clone arena for worker thread

            thread::spawn(move || {
                // The background thread will continuously listen for new filter text
                for ui_message in ui_chan.1 {
                    match ui_message {
                        UiMessage::OpenFile(file_name) => {
                            let (mut reader, headers) = open_csv_file(&file_name);

                            let mut arena_guard = t_csv_arena.lock().unwrap();
                            *arena_guard = SharedArena::new();

                            let new_record_refs = reader
                                .records()
                                .filter_map(|record| record.ok())
                                .map(|record| arena_guard.alloc_arc(record))
                                .collect::<Vec<_>>();

                            let filtered = new_record_refs
                                .iter()
                                .map(|r| r.clone())
                                .collect::<Vec<_>>();

                            let mut master_data = t_sheet_master.lock().unwrap();
                            let mut filtered_data = t_sheet_filtered.lock().unwrap();
                            let mut header_ref = t_header_data.lock().unwrap();

                            *master_data = new_record_refs;
                            *filtered_data = filtered;

                            *header_ref = headers.clone();

                            if let Err(e) = worker_chan.0.send(WorkerMessage::SetData(())) {
                                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                                break; // UI thread probably disconnected, exit worker
                            }
                        }
                        UiMessage::FilterData(filter, column) => {
                            let master_data = t_sheet_master.lock().unwrap();

                            let filtered = if let Some(column) = column {
                                master_data
                                    .iter()
                                    .filter(|r| r.iter().nth(column).unwrap().contains(&filter))
                                    .map(|r| r.clone())
                                    .collect::<Vec<_>>()
                            } else {
                                master_data
                                    .iter()
                                    .filter(|r| r.iter().any(|c| c.contains(&filter)))
                                    .map(|r| r.clone())
                                    .collect::<Vec<_>>()
                            };

                            let mut filtered_data = t_sheet_filtered.lock().unwrap();
                            *filtered_data = filtered;

                            if let Err(e) = worker_chan.0.send(WorkerMessage::SetData(())) {
                                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                                break; // UI thread probably disconnected, exit worker
                            }
                        }
                        UiMessage::FilterColumns(hidden) => {}
                    }
                }

                println!("Worker: Exiting.");
            });

            Ok(Box::new(MyApp {
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
                sender_to_worker: ui_chan.0,
                receiver_from_worker: worker_chan.1,
                sort_by_column: None,
                sort_order: None,
                sheet_master,
                sheet_filtered,
                header_data,
                df: None,
            }))
        }),
    )
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Clone, Default)]
struct FileHeader {
    unique_vals: Vec<String>,
    name: String,
    visible: bool,
    dtype: Option<String>,
    sort: Option<SortOrder>,
    sort_dir: Option<bool>,
}

struct MyApp {
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
    sort_by_column: Option<usize>,
    sort_order: Option<SortOrder>,
    sheet_master: Arc<Mutex<Vec<ArenaArc<StringRecord>>>>,
    sheet_filtered: Arc<Mutex<Vec<ArenaArc<StringRecord>>>>,
    header_data: Arc<Mutex<Vec<FileHeader>>>,
    df: Option<DataFrame>,
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
    ctx: &Context,
    table_ui: Table,
    app: &mut MyApp,
    sort_order: SortOrder,
    sort_by_column: Option<usize>,
) -> ScrollAreaOutput<()> {
    let filter = app.filter.clone();

    let default: Vec<FileHeader> = vec![];

    let visible_columns = app
        .columns
        .as_ref()
        .unwrap_or(&default)
        .iter()
        .enumerate()
        .filter(|(_, c)| c.visible)
        .map(|(index, _)| index)
        .collect::<Vec<usize>>();

    let sheet = Arc::clone(&app.sheet_filtered);

    let filtered_data = sheet.lock().unwrap();

    return table_ui.body(|body| {
        let table_height = filtered_data.len();

        body.rows(18.0, table_height, |mut row| {
            let row_index = row.index();

            let row_data = filtered_data.get(row_index).unwrap();

            row_data
                .iter()
                .map(|text| text.to_string())
                .enumerate()
                .filter(|(index, _)| visible_columns.contains(index))
                .for_each(|(col_index, text)| {
                    let text: &str = text.as_ref();

                    row.col(|ui| {
                        let label = if filter.is_empty() {
                            ui.label(text)
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

                                    ui.label(job)
                                } else {
                                    let text: Vec<&str> = text.split(&filter).collect();

                                    if text.len() == 1 {
                                        job.append(
                                            &filter,
                                            0.0,
                                            TextFormat {
                                                color: Color32::YELLOW,
                                                ..Default::default()
                                            },
                                        );
                                        job.append(text[0], 0.0, TextFormat::default());
                                        ui.label(job)
                                    } else if text.len() == 2 {
                                        job.append(text[0], 0.0, TextFormat::default());
                                        job.append(
                                            &filter,
                                            0.0,
                                            TextFormat {
                                                color: Color32::YELLOW,
                                                ..Default::default()
                                            },
                                        );
                                        job.append(text[1], 0.0, TextFormat::default());

                                        ui.label(job)
                                    } else {
                                        ui.label(job)
                                    }
                                }
                            } else {
                                ui.label(text)
                            }
                        };

                        if label.clicked() {
                            ctx.input(|input| {
                                app.filter = text.to_string();
                                app.loading = true;

                                if input.modifiers.command {
                                    if let Err(e) = &app.sender_to_worker.send(
                                        UiMessage::FilterData(app.filter.clone(), Some(col_index)),
                                    ) {
                                        eprintln!(
                                            "Worker: Failed to send page data to UI thread: {:?}",
                                            e
                                        );
                                    }
                                } else {
                                    if let Err(e) = &app.sender_to_worker.send(
                                        UiMessage::FilterData(app.filter.clone(), Some(col_index)),
                                    ) {
                                        eprintln!(
                                            "Worker: Failed to send page data to UI thread: {:?}",
                                            e
                                        );
                                    }
                                }
                            })
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

fn display_headers(ui: &mut egui::Ui, headers: &mut Vec<FileHeader>) {
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
            }
        }
    });
}

fn open_file(app: &mut MyApp, ctx: &egui::Context) {
    if let Some(path) = rfd::FileDialog::new().pick_file() {
        let file_name = path.display().to_string();
        app.picked_path = Some(file_name.clone());

        let (_, headers) = open_csv_file(&file_name);

        app.columns = Some(headers.clone());

        app.loading = true;
        if let Err(e) = app
            .sender_to_worker
            .send(UiMessage::OpenFile(file_name.clone()))
        {
            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
    }
}

fn handle_key_nav<'a>(
    app: &mut MyApp,
    ctx: &egui::Context,
    table: TableBuilder<'a>,
) -> TableBuilder<'a> {
    let mut table = table;

    if ctx.input(|i| i.key_pressed(Key::PageUp)) {
        table = table.vertical_scroll_offset(app.scroll_y.sub(app.inner_rect / 2.0).max(0.0));
    }

    if ctx.input(|i| i.key_pressed(Key::PageDown)) {
        table = table.vertical_scroll_offset(app.scroll_y.add(app.inner_rect / 2.0));
    }

    if ctx.input(|i| i.key_pressed(Key::Home)) {
        table = table.vertical_scroll_offset(0.0);
    }

    if ctx.input(|i| i.key_pressed(Key::End)) {
        table = table.vertical_scroll_offset(app.content_height);
    }

    return table;
}

fn display_table_headers<'a>(app: &mut MyApp, table: TableBuilder<'a>) -> Table<'a> {
    let mut table = table;

    let mut def_headers: Vec<FileHeader> = vec![];
    let headers = app.columns.as_mut().unwrap_or(def_headers.as_mut());

    for _ in headers.iter().filter(|c| c.visible) {
        table = table.column(Column::auto());
    }

    table.header(20.0, |mut header| {
        headers
            .iter_mut()
            .filter(|c| c.visible)
            .enumerate()
            .for_each(|(idx, file_header)| {
                header.col(|ui| {
                    egui::containers::Sides::new().show(
                        ui,
                        |ui| {
                            let name = &file_header.name;

                            if let Some(dtype) = file_header.dtype.clone() {
                                ui.heading(format!("{} ({})", name, dtype))
                                    .on_hover_ui(|ui| {
                                        ScrollArea::vertical().show(ui, |ui| {
                                            ui.style_mut().interaction.selectable_labels = true;
                                            ui.label(file_header.unique_vals.join("; "));
                                        });
                                    });
                            } else {
                                ui.heading(name);
                            }
                        },
                        |ui| {
                            let asc = file_header.sort_dir.unwrap_or(false);

                            if ui.button(if asc { "⬆" } else { "⬇" }).clicked() {
                                file_header.sort_dir = Some(!asc);

                                if asc {
                                    app.sort_order = Some(SortOrder::Asc);
                                    app.sort_by_column = Some(idx);
                                } else {
                                    app.sort_order = Some(SortOrder::Dsc);
                                    app.sort_by_column = Some(idx);
                                }
                            }
                        },
                    );
                });
            });
    })
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(&ctx, |ui| {
            while let Ok(message) = self.receiver_from_worker.try_recv() {
                match message {
                    WorkerMessage::SetData(data) => {
                        self.loading = false;
                    }
                    WorkerMessage::SetDf(df) => {
                        let columns = df.get_columns();

                        let cols = columns
                            .iter()
                            .map(|c| FileHeader {
                                unique_vals: c
                                    .unique()
                                    .unwrap_or(polars::prelude::Column::default())
                                    .into_materialized_series()
                                    .iter()
                                    .map(|v| v.to_string())
                                    .collect::<Vec<_>>(),
                                name: c.name().to_string(),
                                visible: true,
                                dtype: Some(c.dtype().to_string()),
                                ..FileHeader::default()
                            })
                            .collect::<Vec<_>>();

                        self.df = Some(df);

                        self.loading = false;
                        ctx.request_repaint();
                    }
                }
            }

            ui.label(if self.loading { "Loading..." } else { "Ready" });

            if let Some(picked_path) = &self.picked_path {
                ui.label(format!("CSV reader :: {}", picked_path));
            } else {
                ui.label(format!("CSV reader"));
            }

            if let Some(headers) = self.columns.as_mut() {
                display_headers(ui, headers);
            }

            if ui.button("Open file…").clicked() {
                open_file(self, ctx);
            }

            ui.separator();

            if ui.text_edit_singleline(&mut self.filter).changed() {
                self.loading = true;
                if let Err(e) = self
                    .sender_to_worker
                    .send(UiMessage::FilterData(self.filter.clone(), None))
                {
                    eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                }
            };

            ui.separator();

            ScrollArea::horizontal().show(ui, |ui| {
                let mut table = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .min_scrolled_height(0.0);

                if ctx.input(|i| i.key_pressed(Key::Escape)) {
                    self.filter = "".to_string();
                    if let Err(e) = self
                        .sender_to_worker
                        .send(UiMessage::FilterData("".to_string(), None))
                    {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }

                table = handle_key_nav(self, ctx, table);

                let table_ui = display_table_headers(self, table);

                let scroll_area = display_table(
                    ctx,
                    table_ui,
                    self,
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

                let (_, headers) = open_csv_file(str_path);

                self.columns = Some(headers.clone());

                self.loading = true;

                if let Err(e) = self
                    .sender_to_worker
                    .send(UiMessage::OpenFile(str_path.to_string()))
                {
                    eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                }
            }
        });

        if let Some(file_name) = file_name {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
        }
    }
}
