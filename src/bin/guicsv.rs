#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, NodeIndex, Style, SurfaceIndex};
use poll_promise::Promise;
use std::sync::Arc;

use crate::egui::Context;
use csv::StringRecord;
use egui::{Color32, Key, ScrollArea, TextFormat};

use egui::scroll_area::ScrollAreaOutput;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::{
    collections::HashSet,
    ops::{Add, Sub},
};

use egui_extras::{Column, Table, TableBuilder};

use eframe::egui;
use jonathan::read_csv::open_csv_file;
use jonathan::ui::FileHeader;

struct TabViewer<'a> {
    added_nodes: &'a mut Vec<(SurfaceIndex, NodeIndex)>,
    promised_data: &'a Promise<Arc<ArcSheet>>,
    filtered_data: &'a Promise<Arc<ArcSheet>>,
    ctx: &'a Context,
    filter: &'a str,
    columns: &'a Option<Vec<FileHeader>>,
    sender: &'a Sender<UiMessage>,
}

impl egui_dock::TabViewer for TabViewer<'_> {
    type Tab = usize;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        format!("Tab {tab}").into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        ui.label(format!("Content of {tab}"));

        if ui.button("Open file…").clicked() {
            open_file_dialog(&self.sender);
        }

        let table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .min_scrolled_height(0.0);

        let mut columns = self.columns.clone();
        let table_ui = display_table_headers(&mut columns, table);

        display_table(
            self.ctx,
            table_ui,
            &self.filter,
            &self.columns,
            &self.promised_data,
            &self.filtered_data,
            &self.sender,
            // self.sort_order.unwrap_or(SortOrder::Dsc),
            // self.sort_by_column,
        );
    }

    fn on_close(&mut self, _tab: &mut Self::Tab) -> OnCloseResponse {
        println!("Closed tab: {_tab}");
        OnCloseResponse::Close
    }

    fn on_add(&mut self, surface: SurfaceIndex, node: NodeIndex) {
        self.added_nodes.push((surface, node));
    }
}

enum UiMessage {
    OpenFile(String),
    FilterData(String, Option<usize>),
}

// Demonstrates how to replace all fonts.
fn replace_fonts(ctx: &egui::Context) {
    // Start with the default fonts (we will be adding to them rather than replacing them).
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "IBMPlex".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../IBM_Plex_Mono/IBMPlexMono-Regular.ttf"
        ))),
    );

    // Put font first (highest priority)
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "IBMPlex".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "IBMPlex".to_owned());

    // Tell egui to use these fonts:
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 240.0]) // wide enough for the drag-drop overlay text
            .with_drag_and_drop(true),
        ..Default::default()
    };

    let title = &format!("CSV Reader.");

    let ui_chan = mpsc::channel::<UiMessage>();

    eframe::run_native(
        title,
        options,
        Box::new(|cc| {
            replace_fonts(&cc.egui_ctx);

            Ok(Box::new(MyApp {
                columns: None,
                scroll_y: 0.0,
                inner_rect: 0.0,
                content_height: 0.0,
                filter: "".to_owned(),
                dropped_files: Vec::new(),
                picked_path: None,
                loading: false,
                sender: ui_chan.0,
                receiver: ui_chan.1,
                sort_by_column: None,
                sort_order: None,
                promised_data: poll_promise::Promise::spawn_thread("empty_data", move || {
                    Arc::new(vec![])
                }),
                filtered_data: poll_promise::Promise::spawn_thread("empty_data", move || {
                    Arc::new(vec![])
                }),
                tree: DockState::new(vec![1]),
                counter: 2,
            }))
        }),
    )
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

type ArcSheet = Vec<Arc<StringRecord>>;

struct MyApp {
    columns: Option<Vec<FileHeader>>,
    scroll_y: f32,
    inner_rect: f32,
    content_height: f32,
    filter: String,
    dropped_files: Vec<egui::DroppedFile>,
    picked_path: Option<String>,
    loading: bool,
    sender: Sender<UiMessage>,
    receiver: Receiver<UiMessage>,
    sort_by_column: Option<usize>,
    sort_order: Option<SortOrder>,
    promised_data: Promise<Arc<ArcSheet>>,
    filtered_data: Promise<Arc<ArcSheet>>,

    tree: DockState<usize>,
    counter: usize,
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
    filter: &str,
    columns: &Option<Vec<FileHeader>>,
    promised_data: &Promise<Arc<ArcSheet>>,
    filtered_data: &Promise<Arc<ArcSheet>>,
    sender: &Sender<UiMessage>,
) -> ScrollAreaOutput<()> {
    let default: Vec<FileHeader> = vec![];

    let visible_columns = columns
        .as_ref()
        .unwrap_or(&default)
        .iter()
        .enumerate()
        .filter(|(_, c)| c.visible)
        .map(|(index, _)| index)
        .collect::<Vec<usize>>();

    let filtered_data = filtered_data.ready();
    let master_data = promised_data.ready();

    let def_vec: Vec<Arc<StringRecord>> = vec![];
    let default_data: Arc<Vec<Arc<StringRecord>>> = Arc::new(def_vec);

    let sheet_data = match filtered_data {
        Some(data) if !data.is_empty() => data,
        _ => master_data.unwrap_or(&default_data),
    };

    return table_ui.body(|body| {
        let table_height = sheet_data.len();

        body.rows(18.0, table_height, |mut row| {
            let row_index = row.index();

            let row_data = sheet_data.get(row_index).unwrap();

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
                                if input.modifiers.command {
                                    if let Err(e) = &sender.send(UiMessage::FilterData(
                                        text.to_string(),
                                        Some(col_index),
                                    )) {
                                        eprintln!(
                                            "Worker: Failed to send page data to UI thread: {:?}",
                                            e
                                        );
                                    }
                                } else {
                                    if let Err(e) = &sender.send(UiMessage::FilterData(
                                        text.to_string(),
                                        Some(col_index),
                                    )) {
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

fn load_file(app: &mut MyApp, ctx: &egui::Context, file_name: String) {
    app.picked_path = Some(file_name.clone());

    let (mut reader, headers) = open_csv_file(&file_name);

    app.columns = Some(headers.clone());
    app.loading = true;

    app.promised_data = poll_promise::Promise::spawn_thread("slow_operation", move || {
        Arc::new(
            reader
                .records()
                .filter_map(|record| record.ok())
                .map(|r| Arc::new(r))
                .collect::<Vec<_>>(),
        )
    });

    ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
}

fn open_file_dialog(sender: &Sender<UiMessage>) {
    if let Some(path) = rfd::FileDialog::new().pick_file() {
        if let Err(e) = sender.send(UiMessage::OpenFile(path.display().to_string())) {
            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
        }
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

fn display_table_headers<'a>(
    columns: &mut Option<Vec<FileHeader>>,
    table: TableBuilder<'a>,
) -> Table<'a> {
    let mut table = table;

    let mut def_headers: Vec<FileHeader> = vec![];
    let headers = columns.as_mut().unwrap_or(def_headers.as_mut());

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

                                // if asc {
                                //     app.sort_order = Some(SortOrder::Asc);
                                //     app.sort_by_column = Some(idx);
                                // } else {
                                //     app.sort_order = Some(SortOrder::Dsc);
                                //     app.sort_by_column = Some(idx);
                                // }
                            }
                        },
                    );
                });
            });
    })
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                UiMessage::FilterData(filter, column) => {
                    self.filter = filter;
                    self.loading = true;

                    if let Some(master_data) = self.promised_data.ready() {
                        let cloned = Arc::clone(&master_data);
                        let filter = self.filter.clone();

                        self.filtered_data =
                            poll_promise::Promise::spawn_thread("filter_sheet", move || {
                                Arc::new(
                                    cloned
                                        .iter()
                                        .filter(|r| r.iter().any(|c| c.contains(&filter)))
                                        .map(|r| r.clone())
                                        .collect::<Vec<_>>(),
                                )
                            });
                    }
                }
                UiMessage::OpenFile(file) => load_file(self, ctx, file),
            }
        }

        let mut added_nodes = Vec::new();

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show_add_buttons(true)
            .show(
                ctx,
                &mut TabViewer {
                    added_nodes: &mut added_nodes,
                    promised_data: &self.promised_data,
                    filtered_data: &self.filtered_data,
                    ctx: &ctx,
                    filter: &self.filter,
                    columns: &self.columns,
                    sender: &self.sender,
                },
            );

        added_nodes.drain(..).for_each(|(surface, node)| {
            self.tree.set_focused_node_and_surface((surface, node));
            self.tree.push_to_focused_leaf(self.counter);
            self.counter += 1;
        });

        // egui::CentralPanel::default().show(&ctx, |ui| {
        //     ui.label(if self.promised_data.ready().is_none() {
        //         "Loading..."
        //     } else {
        //         "File loaded"
        //     });

        //     ui.label(if self.filtered_data.ready().is_none() {
        //         "Filtering..."
        //     } else {
        //         "File ready"
        //     });

        //     if let Some(picked_path) = &self.picked_path {
        //         ui.label(format!("CSV reader :: {}", picked_path));
        //     } else {
        //         ui.label(format!("CSV reader"));
        //     }

        //     if let Some(headers) = self.columns.as_mut() {
        //         display_headers(ui, headers);
        //     }

        //     if ui.button("Open file…").clicked() {
        //         open_file_dialog(self);
        //     }

        //     ui.separator();

        //     if ui.text_edit_singleline(&mut self.filter).changed() {
        //         if let Some(master_data) = self.promised_data.ready() {
        //             let cloned = Arc::clone(&master_data);
        //             let filter = self.filter.clone();

        //             self.filtered_data =
        //                 poll_promise::Promise::spawn_thread("filter_sheet", move || {
        //                     Arc::new(
        //                         cloned
        //                             .iter()
        //                             .filter(|r| r.iter().any(|c| c.contains(&filter)))
        //                             .map(|r| r.clone())
        //                             .collect::<Vec<_>>(),
        //                     )
        //                 });
        //         }
        //     };

        //     ui.separator();

        //     ScrollArea::horizontal().show(ui, |ui| {
        //         let mut table = TableBuilder::new(ui)
        //             .striped(true)
        //             .resizable(true)
        //             .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        //             .min_scrolled_height(0.0);

        //         if ctx.input(|i| i.key_pressed(Key::Escape)) {
        //             self.filter = "".to_string();
        //             if let Err(e) = self
        //                 .sender
        //                 .send(UiMessage::FilterData("".to_string(), None))
        //             {
        //                 eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
        //             }
        //         }

        //         table = handle_key_nav(self, ctx, table);

        //         let table_ui = display_table_headers(&mut self.columns, table);

        //         let scroll_area = display_table(
        //             ctx,
        //             table_ui,
        //             &self.filter,
        //             &self.columns,
        //             &self.promised_data,
        //             &self.filtered_data,
        //             &self.sender,
        //             // self.sort_order.unwrap_or(SortOrder::Dsc),
        //             // self.sort_by_column,
        //         );

        //         let content_height = scroll_area.content_size[1];

        //         self.content_height = content_height;

        //         let offset = scroll_area.state.offset[1];
        //         self.scroll_y = offset;
        //         self.inner_rect = scroll_area.inner_rect.height();
        //     });
        // });

        // preview_files_being_dropped(ctx);

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

                if let Err(e) = self.sender.send(UiMessage::OpenFile(str_path.to_string())) {
                    eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                }
            }
        });

        if let Some(file_name) = file_name {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
        }
    }
}
