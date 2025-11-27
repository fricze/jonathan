#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use csv::StringRecord;
use egui::Key;
use egui::{Align, Align2, Id, LayerId, Order, Response, Stroke, TextStyle};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, NodeIndex, Style, SurfaceIndex};
use poll_promise::Promise;
use std::sync::Arc;
use std::thread;

use egui::Color32;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};

mod new_table;
mod read_csv;
mod rect;
mod table;
mod types;
mod ui;

use crate::types::{Ping, SortOrder};
use eframe::egui;
use read_csv::open_csv_file;
use types::{ArcSheet, FileHeader, Filename, MyApp, SheetTab, TabId, TabViewer, UiMessage};

fn get_last_element_from_path(s: &str) -> Option<&str> {
    s.split('/').last()
}

fn file_button(ui: &mut egui::Ui, file: &str) -> Response {
    let mut l: Option<Response> = None;
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(4, 8)) // Horizontal 20, Vertical 10 padding
        .show(ui, |ui| {
            l = Some(ui.selectable_label(false, file));
        });

    let mut rect = l.unwrap().rect;

    rect.set_left(rect.left() - 4.0);
    rect.set_top(rect.top() - 4.0);
    rect.set_width(120.0);
    rect.set_height(rect.height() + 8.0);

    let response = ui
        .put(rect, egui::Label::new(""))
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let border_color = if response.hovered() {
        Color32::from_rgb(100, 100, 255)
    } else {
        Color32::GRAY
    };

    let stroke_rect = rect.clone();
    ui.painter().rect_stroke(
        stroke_rect,
        4.0,
        Stroke::new(1.0, border_color),
        egui::StrokeKind::Outside,
    );

    response
}

impl egui_dock::TabViewer for TabViewer<'_> {
    type Tab = SheetTab;

    fn add_popup(&mut self, ui: &mut egui::Ui, surface: SurfaceIndex, node: NodeIndex) {
        ui.set_min_width(120.0);
        ui.set_max_width(120.0);

        ui.with_layout(egui::Layout::default().with_cross_align(Align::Min), |ui| {
            ui.set_width(120.0);

            if file_button(ui, "Empty tab").clicked() {
                self.added_nodes.push((surface, node, "".to_string()));
            }

            for path in self.files_list {
                if let Some(file) = get_last_element_from_path(path) {
                    if file_button(ui, file).clicked() {
                        self.added_nodes.push((surface, node, path.to_string()));
                    }
                }
            }
        });
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        let file = get_last_element_from_path(&tab.chosen_file);
        let tab_id = &tab.id;

        if let Some(file) = file {
            if file.is_empty() {
                format!("[tab {tab_id}] Load file").into()
            } else {
                format!("[tab {tab_id}] {file}").into()
            }
        } else {
            format!("[tab {tab_id}] Load file").into()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let tab_id = tab.id;

        ui.add_space(4.0);

        if ui.button("Open file…").clicked() {
            open_file_dialog(&self.sender, &tab_id);
        }

        ui.add_space(4.0);

        if !self.files_list.is_empty() {
            let radio = &tab.chosen_file;
            egui::ComboBox::from_label("Select file")
                .selected_text(get_last_element_from_path(radio).unwrap_or(""))
                .show_ui(ui, |ui| {
                    for file in self.files_list {
                        let filename = file.clone();
                        ui.selectable_value(
                            &mut tab.chosen_file,
                            filename,
                            get_last_element_from_path(file).unwrap_or(""),
                        );
                    }
                });

            ui.add_space(4.0);
        }

        let chosen_file = &tab.chosen_file.clone();

        if !chosen_file.is_empty() {
            if let Some(filter) = tab.filter.get_mut(chosen_file) {
                ui.horizontal_wrapped(|ui| {
                    if ui.text_edit_singleline(filter).changed() {
                        if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                            chosen_file.to_string(),
                            filter.to_string(),
                            tab_id,
                            None,
                        )) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }

                    if ui.button("Clear (esc)").clicked() {
                        if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                            chosen_file.to_string(),
                            "".to_string(),
                            tab_id,
                            None,
                        )) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }
                });
            }
        }

        if let Some(focused_tab) = self.focused_tab {
            if focused_tab == tab.id {
                if self.ctx.input(|i| i.key_pressed(Key::Escape)) {
                    if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                        chosen_file.to_string(),
                        "".to_string(),
                        tab_id,
                        None,
                    )) {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }
            }
        }

        if let Some(sheet) = self.promised_data.get(chosen_file) {
            if sheet.is_empty() {
                let painter = self
                    .ctx
                    .layer_painter(LayerId::new(Order::Foreground, Id::new("file_drop_target")));

                let screen_rect = self.ctx.content_rect();
                painter.rect_filled(screen_rect, 0.0, Color32::from_black_alpha(192));
                painter.text(
                    screen_rect.center(),
                    Align2::CENTER_CENTER,
                    "Loading…",
                    TextStyle::Heading.resolve(&self.ctx.style()),
                    Color32::WHITE,
                );
            }
        }

        if let Some(columns) = tab.columns.get_mut(chosen_file) {
            display_headers(ui, columns.as_mut());
            ui.add_space(4.0);

            if let Some(master_data) = self.promised_data.get(chosen_file) {
                let filtered_data = self.filtered_data.get(&(chosen_file.to_string(), tab_id));

                let filter = if !self.global_filter.is_empty() {
                    self.global_filter
                } else if let Some(chosen_file) = tab.filter.get(chosen_file) {
                    chosen_file
                } else {
                    &"".to_string()
                };

                let sheet_data = match filtered_data {
                    // Some(filtered_data) if !filter.is_empty() => filtered_data,
                    Some(filtered_data) if !filtered_data.is_empty() => filtered_data,
                    _ => master_data,
                };

                // let id_salt = Id::new("table_demo");
                // let state_id = egui_table::Table::new().id_salt(id_salt).get_id(ui); // Note: must be here (in the correct outer `ui` scope) to be correct.
                let len = sheet_data.len();

                let first_row = sheet_data.get(0);
                let num_columns = if let Some(first_row) = first_row {
                    first_row.len()
                } else {
                    0
                };

                let col_len = columns.len();

                let mut t = new_table::TableDemo {
                    data: sheet_data,
                    num_columns,
                    columns: columns.as_mut(),
                    num_rows: len as u64,
                    num_sticky_cols: if col_len > 0 { 1 } else { 0 },
                    default_column: egui_table::Column::new(50.0)
                        .range(10.0..=500.0)
                        .resizable(true),
                    auto_size_mode: egui_table::AutoSizeMode::default(),
                    top_row_height: 24.0,
                    row_height: 18.0,
                    is_row_expanded: Default::default(),
                    prefetched: vec![],
                    sender: self.sender,
                    tab_id: tab_id,
                    filename: chosen_file.clone(),
                };

                t.ui(ui);
            }
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if self.tabs_no > 1 {
            OnCloseResponse::Close
        } else {
            OnCloseResponse::Ignore
        }
    }

    // fn on_add(&mut self, surface: SurfaceIndex, node: NodeIndex) {
    //     self.added_nodes.push((surface, node));
    // }
}

fn replace_fonts(ctx: &egui::Context) {
    // Start with the default fonts (we will be adding to them rather than replacing them).
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "IBMPlex".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../IBM_Plex_Mono/IBMPlexMono-Regular.ttf"
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

    let worker_chan = mpsc::channel::<UiMessage>();
    let ui_chan = mpsc::channel::<Ping>();

    eframe::run_native(
        title,
        options,
        Box::new(|cc| {
            replace_fonts(&cc.egui_ctx);

            Ok(Box::new(MyApp {
                worker_chan: worker_chan,
                ui_chan: ui_chan,
                sort_by_column: None,
                sort_order: None,
                dropped_files: Vec::new(),
                picked_path: None,
                loading: false,
                sheets_data: HashMap::new(),
                filtered_data: HashMap::new(),
                tree: DockState::new(vec![SheetTab {
                    id: 1,
                    ..Default::default()
                }]),
                counter: 2,
                files_list: vec![],
                global_filter: "".to_string(),
            }))
        }),
    )
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

        let screen_rect = ctx.content_rect();
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

fn display_headers(ui: &mut egui::Ui, headers: &mut Vec<FileHeader>) {
    ui.collapsing("Show/hide columns", |ui| {
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
    });
}

// fn load_file(app: &mut MyApp, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
//     app.picked_path = Some(file_name.clone());

//     app.files_list.push(file_name.clone());

//     let (mut reader, headers) = open_csv_file(&file_name);

//     for tab in app.tree.iter_all_tabs_mut() {
//         let sheet_tab = tab.1;
//         sheet_tab.columns.insert(file_name.clone(), headers.clone());
//         sheet_tab.filter.insert(file_name.clone(), "".to_string());

//         if let Some(tab_id) = tab_id {
//             if sheet_tab.id == tab_id {
//                 sheet_tab.chosen_file = file_name.clone();
//             }
//         }
//     }

//     app.loading = true;

//     let promise = poll_promise::Promise::spawn_thread("slow_operation", move || {
//         Arc::new(
//             reader
//                 .records()
//                 .filter_map(|record| record.ok())
//                 .map(|r| Arc::new(r))
//                 .collect::<Vec<_>>(),
//         )
//     });
//     app.sheets_data.insert(file_name.clone(), promise);

//     ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
// }

fn open_file_dialog(sender: &Sender<UiMessage>, tab: &usize) {
    if let Some(paths) = rfd::FileDialog::new()
        .add_filter("csv", &["csv"])
        .pick_files()
    {
        for path in paths {
            if let Err(e) = sender.send(UiMessage::OpenFile(path.display().to_string(), Some(*tab)))
            {
                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
            }
        }
    }
}

fn sort_data(
    mut master_clone: Vec<Arc<StringRecord>>,
    sort_by: (usize, SortOrder),
) -> Vec<Arc<StringRecord>> {
    master_clone.sort_by(|a, b| -> std::cmp::Ordering {
        let val_a = a.get(sort_by.0).unwrap();
        let val_b = b.get(sort_by.0).unwrap();

        if sort_by.1 == SortOrder::Dsc {
            val_a.cmp(val_b)
        } else {
            val_b.cmp(val_a)
        }
    });

    master_clone
}

fn filter_data(
    filtered_data: &mut HashMap<(Filename, TabId), Promise<Arc<ArcSheet>>>,
    sheets_data: &HashMap<String, Promise<Arc<ArcSheet>>>,
    filter: String,
    filename: String,
    tab_id: usize,
) {
    if filter.is_empty() {
        filtered_data.insert(
            (filename.clone(), tab_id),
            poll_promise::Promise::spawn_thread(format!("filter_sheet {tab_id}"), move || {
                Arc::new(vec![])
            }),
        );
    } else {
        if let Some(file) = sheets_data.get(&filename) {
            if let Some(master_data) = file.ready() {
                let master_clone = Arc::clone(&master_data);
                let filter = filter.clone();

                filtered_data.insert(
                    (filename, tab_id),
                    poll_promise::Promise::spawn_thread(
                        format!("filter_sheet {tab_id}"),
                        move || {
                            Arc::new(
                                master_clone
                                    .iter()
                                    .filter(|r| r.iter().any(|c| c.contains(&filter)))
                                    .map(|r| r.clone())
                                    .collect::<Vec<_>>(),
                            )
                        },
                    ),
                );
            }
        }
    }
}

fn filter_data_simple(
    master_data: Arc<Vec<Arc<StringRecord>>>,
    filter: String,
) -> Arc<Vec<Arc<StringRecord>>> {
    let master_clone = Arc::clone(&master_data);
    let filter = filter.clone();

    Arc::new(
        master_clone
            .iter()
            .filter(|r| r.iter().any(|c| c.contains(&filter)))
            .map(|r| r.clone())
            .collect::<Vec<_>>(),
    )
}

impl MyApp {
    fn load_file(&mut self, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
        self.picked_path = Some(file_name.clone());

        self.files_list.push(file_name.clone());

        let (mut reader, headers) = open_csv_file(&file_name);

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;
            sheet_tab.columns.insert(file_name.clone(), headers.clone());
            sheet_tab.filter.insert(file_name.clone(), "".to_string());

            if let Some(tab_id) = tab_id {
                if sheet_tab.id == tab_id {
                    sheet_tab.chosen_file = file_name.clone();
                }
            }
        }

        self.loading = true;

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name.clone()));

        let chan = self.worker_chan.0.clone();
        let ctx = ctx.clone();

        thread::spawn(move || {
            let master_data = reader
                .records()
                .filter_map(|record| record.ok())
                .map(|r| Arc::new(r))
                .collect::<Vec<_>>();

            if let Err(e) = chan.send(UiMessage::SetMaster(master_data, file_name.clone())) {
                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
            }

            ctx.request_repaint();
        });

        // let promise = poll_promise::Promise::spawn_thread("slow_operation", move || {
        //     Arc::new(
        //         reader
        //             .records()
        //             .filter_map(|record| record.ok())
        //             .map(|r| Arc::new(r))
        //             .collect::<Vec<_>>(),
        //     )
        // });
    }

    fn sort_current_sheet(
        &mut self,
        ctx: &egui::Context,
        filename: String,
        sort_order: (usize, SortOrder),
        tab_id: usize,
    ) {
        let chan = self.worker_chan.0.clone();

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;

            if sheet_tab.id == tab_id {
                if let Some(master_data) = self.sheets_data.get(&filename) {
                    let master_clone = master_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();

                    thread::spawn(move || {
                        let sorted = sort_data(master_clone, sort_order);

                        if let Err(e) = chan.send(UiMessage::SetSorted(sorted, filename, tab_id)) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }

                        ctx.request_repaint();
                    });
                }
            };
        }

        ()
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ui_ping) = self.ui_chan.1.try_recv() {
            if ui_ping {
                eprintln!("requested ui ping");

                ctx.request_repaint();
            }
        }

        while let Ok(message) = self.worker_chan.1.try_recv() {
            match message {
                UiMessage::SetMaster(master, file_name) => {
                    self.sheets_data.insert(file_name, master);
                }
                UiMessage::SetSorted(sorted, file_name, tab_id) => {
                    self.filtered_data.insert((file_name, tab_id), sorted);
                }
                UiMessage::FilterGlobal(filter) => {
                    for tab in self.tree.iter_all_tabs_mut() {
                        let sheet_tab = tab.1;
                        let chosen_file = sheet_tab.chosen_file.clone();

                        self.global_filter = filter.clone();

                        // filter_data(
                        //     &mut self.filtered_data,
                        //     &self.sheets_data,
                        //     filter.clone(),
                        //     chosen_file,
                        //     sheet_tab.id,
                        // );
                    }
                }
                UiMessage::FilterSheet(filename, filter, tab_id, column) => {
                    for tab in self.tree.iter_all_tabs_mut() {
                        let sheet_tab = tab.1;

                        if sheet_tab.id == tab_id {
                            sheet_tab.filter.insert(filename.clone(), filter.clone());

                            // filter_data(
                            //     &mut self.filtered_data,
                            //     &self.sheets_data,
                            //     filter.clone(),
                            //     filename.clone(),
                            //     tab_id,
                            // );
                        }
                    }
                }
                UiMessage::SortSheet(filename, sort_order, tab_id) => {
                    self.sort_current_sheet(ctx, filename, sort_order, tab_id);
                }
                UiMessage::OpenFile(file, tab) => self.load_file(ctx, file, tab),
            }
        }

        let mut added_nodes = Vec::new();

        let tabs_no = self.tree.iter_all_tabs().count();
        let focused_tab = self.tree.find_active_focused().map(|(_, tab)| tab.id);

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ctx.input(|input| {
                if input.key_pressed(Key::X) {
                    if let Err(e) = &self
                        .worker_chan
                        .0
                        .send(UiMessage::FilterGlobal("".to_string()))
                    {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }
            });

            ui.vertical(|ui| {
                ui.add_space(4.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label("Global filter");

                    let filter = &self.global_filter.to_string();
                    if ui.text_edit_singleline(&mut self.global_filter).changed() {
                        if let Err(e) = &self
                            .worker_chan
                            .0
                            .send(UiMessage::FilterGlobal(filter.to_string()))
                        {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }

                    if ui.button("Clear (x)").clicked() {
                        if let Err(e) = &self
                            .worker_chan
                            .0
                            .send(UiMessage::FilterGlobal("".to_string()))
                        {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }
                });

                ui.add_space(4.0);
            });
            // for lab in vec!["raz", "dwa", "trzy"] {
            //     if ui.selectable_label(false, lab.to_string()).clicked() {}
            // }
        });

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show_add_buttons(true)
            .show_add_popup(true)
            .show(
                ctx,
                &mut TabViewer {
                    added_nodes: &mut added_nodes,
                    promised_data: &self.sheets_data,
                    filtered_data: &self.filtered_data,
                    ctx: &ctx,
                    sender: &self.worker_chan.0,
                    files_list: &self.files_list,
                    tabs_no,
                    focused_tab,
                    global_filter: &self.global_filter,
                },
            );

        added_nodes.drain(..).for_each(|(surface, node, filename)| {
            self.tree.set_focused_node_and_surface((surface, node));

            let last_tab = self.tree.iter_all_tabs().last().unwrap().1;
            let columns = last_tab.columns.clone();
            let mut filter = last_tab.filter.clone();
            for value in filter.values_mut() {
                *value = "".to_string();
            }

            self.tree.push_to_focused_leaf(SheetTab {
                id: self.counter,
                columns,
                filter,
                chosen_file: filename,
                ..Default::default()
            });

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

        preview_files_being_dropped(ctx);

        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                let files = &i.raw.dropped_files;

                let default_path = PathBuf::default();

                for file in files {
                    let path = file
                        .path
                        .as_ref()
                        .unwrap_or(&default_path)
                        .to_str()
                        .unwrap_or("");

                    if let Err(e) = self
                        .worker_chan
                        .0
                        .send(UiMessage::OpenFile(path.to_string(), None))
                    {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }
            }
        });
    }
}
