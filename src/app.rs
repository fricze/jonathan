use egui::Key;
use egui_dock::{DockArea, Style};
use std::path::PathBuf;
use std::thread;

use crate::data::{filter_data, sort_data};
use crate::menu::OPEN_FILE_ID;
use crate::read_csv::open_csv_file;
use crate::types::{CsvTabViewer, MyApp, SheetTab, SortOrder, UiMessage, active_sheet_data};

#[cfg(target_os = "macos")]
use muda::MenuEvent;

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

impl MyApp {
    pub fn load_file(&mut self, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
        self.picked_path = Some(file_name.clone());

        self.files_list.push(file_name.clone());

        let (mut reader, headers) = open_csv_file(&file_name);

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;
            sheet_tab.columns.insert(file_name.clone(), headers.clone());

            if let Some(tab_id) = tab_id {
                if sheet_tab.id == tab_id {
                    sheet_tab.chosen_file = file_name.clone();
                    self.filters
                        .insert((file_name.clone(), tab_id), "".to_string());
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
                .collect::<Vec<_>>();

            if let Err(e) = chan.send(UiMessage::SetMaster(master_data, file_name.clone())) {
                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
            }

            ctx.request_repaint();
        });
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
                let filter_active = self
                    .filters
                    .get(&(filename.to_string(), tab_id))
                    .map_or(false, |f| !f.is_empty());

                let sheet_data = active_sheet_data(
                    &self.sheets_data,
                    &self.filtered_data,
                    &filename,
                    tab_id,
                    filter_active,
                );

                if !sheet_data.is_empty() {
                    let master_clone = sheet_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();

                    thread::spawn(move || {
                        let sorted = sort_data(master_clone, sort_order);

                        if let Err(e) =
                            chan.send(UiMessage::SetDisplayData(sorted, filename, tab_id))
                        {
                            eprintln!("Worker: Failed to send sorted data to UI thread: {:?}", e);
                        }

                        ctx.request_repaint();
                    });
                }
            };
        }
    }

    fn filter_current_sheet(
        &mut self,
        ctx: &egui::Context,
        filename: String,
        filter: String,
        tab_id: usize,
    ) {
        let chan = self.worker_chan.0.clone();

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;
            let filter = filter.clone();

            if sheet_tab.id == tab_id {
                if let Some(master_data) = self.sheets_data.get(&filename) {
                    let master_clone = master_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();

                    thread::spawn(move || {
                        let filtered = filter_data(master_clone, filter);

                        if let Err(e) =
                            chan.send(UiMessage::SetDisplayData(filtered, filename, tab_id))
                        {
                            eprintln!("Worker: Failed to send filtered data to UI thread: {:?}", e);
                        }

                        ctx.request_repaint();
                    });
                }
            };
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle macOS menu events
        #[cfg(target_os = "macos")]
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id.as_ref() == OPEN_FILE_ID {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .pick_file()
                {
                    if let Some(path_str) = path.to_str() {
                        let _ = self
                            .worker_chan
                            .0
                            .send(UiMessage::OpenFile(path_str.to_string(), None));
                    }
                }
            }
        }

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
                UiMessage::SetDisplayData(sorted, file_name, tab_id) => {
                    self.filtered_data.insert((file_name, tab_id), sorted);
                }
                UiMessage::FilterGlobal(filter) => {
                    self.global_filter = filter;
                }
                UiMessage::FilterSheet(filename, filter, tab_id, _column) => {
                    self.filters
                        .insert((filename.clone(), tab_id), filter.clone());
                    self.filter_current_sheet(ctx, filename, filter, tab_id);
                }
                UiMessage::SortSheet(filename, sort_order, tab_id) => {
                    self.sort_current_sheet(ctx, filename, sort_order, tab_id);
                }
                UiMessage::OpenFile(file, tab) => self.load_file(ctx, file, tab),
                UiMessage::EditCell(filename, tab_id, row_nr, actual_col, new_value) => {
                    let updated = if let Some(filtered) =
                        self.filtered_data.get_mut(&(filename.clone(), tab_id))
                    {
                        if let Some(record) = filtered.get_mut(row_nr as usize) {
                            let mut fields: Vec<String> =
                                record.iter().map(|s| s.to_string()).collect();
                            if let Some(field) = fields.get_mut(actual_col) {
                                *field = new_value.clone();
                                *record = fields.iter().map(|s| s.as_str()).collect();
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !updated {
                        if let Some(sheet) = self.sheets_data.get_mut(&filename) {
                            if let Some(record) = sheet.get_mut(row_nr as usize) {
                                let mut fields: Vec<String> =
                                    record.iter().map(|s| s.to_string()).collect();
                                if let Some(field) = fields.get_mut(actual_col) {
                                    *field = new_value;
                                    *record = fields.iter().map(|s| s.as_str()).collect();
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut added_nodes = Vec::new();

        let tabs_no = self.tree.iter_all_tabs().count();
        let focused_tab = self.tree.find_active_focused().map(|(_, tab)| tab.id);

        egui::TopBottomPanel::top("top_panel").show(ctx, |_ui| {
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
        });

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show_add_buttons(true)
            .show_add_popup(true)
            .show(
                ctx,
                &mut CsvTabViewer {
                    added_nodes: &mut added_nodes,
                    promised_data: &self.sheets_data,
                    filtered_data: &self.filtered_data,
                    ctx: &ctx,
                    sender: &self.worker_chan.0,
                    files_list: &self.files_list,
                    tabs_no,
                    focused_tab,
                    global_filter: &self.global_filter,
                    filters: &mut self.filters,
                },
            );

        added_nodes.drain(..).for_each(|(surface, node, filename)| {
            self.tree.set_focused_node_and_surface((surface, node));

            let columns = self
                .tree
                .iter_all_tabs()
                .last()
                .map(|(_, tab)| tab.columns.clone())
                .unwrap_or_default();

            self.tree.push_to_focused_leaf(SheetTab {
                id: self.counter,
                columns,
                chosen_file: filename,
                ..Default::default()
            });

            self.counter += 1;
        });

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
