#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use egui::Key;
use egui_dock::{DockArea, DockState, Style};
use polars::prelude::*;
use std::thread;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self};

mod new_table;
mod tabs;
mod types;

use crate::types::{FileHeader, Ping, SortOrder};
use eframe::egui;
use types::{CsvTabViewer, MyApp, SheetTab, UiMessage};

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
                master_data: HashMap::new(),
                filtered_data: HashMap::new(),
                tree: DockState::new(vec![SheetTab {
                    id: 1,
                    ..Default::default()
                }]),
                counter: 2,
                files_list: vec![],
                global_filter: "".to_string(),
                filters: HashMap::new(),
                df: DataFrame::empty(),
                filtered_df: DataFrame::empty(),
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

fn filter_df_contains(df: &DataFrame, needle: &str) -> PolarsResult<DataFrame> {
    if needle.is_empty() {
        return Ok(df.clone());
    }

    let mut mask: Option<BooleanChunked> = None;

    for s in df.get_columns() {
        let is_string = matches!(s.dtype(), DataType::String);
        if !is_string {
            continue;
        }

        let ca = s.str()?; // works for Utf8/String
        let col_mask: BooleanChunked = ca
            .into_iter()
            .map(|opt| opt.map_or(false, |v| v.contains(needle)))
            .collect();

        mask = Some(match mask {
            None => col_mask,
            Some(prev) => &prev | &col_mask,
        });
    }

    let mask = mask.unwrap_or_else(|| BooleanChunked::full(PlSmallStr::EMPTY, false, df.height()));
    df.filter(&mask)
}

fn load_polars(file_name: &str) -> Result<DataFrame, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(file_name)?;
    let df = CsvReader::new(file).finish()?;

    // let csv = LazyCsvReader::new(file_name)
    //     .with_has_header(true)
    //     .finish()?;

    Ok(df)
}

impl MyApp {
    fn load_file(&mut self, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
        self.picked_path = Some(file_name.clone());

        self.files_list.push(file_name.clone());

        let chan = self.worker_chan.0.clone();
        let ctx = ctx.clone();

        let t_file_name = file_name.clone();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name.clone()));

        self.loading = true;

        thread::spawn(move || {
            if let Ok(df) = load_polars(&t_file_name) {
                let headers = df
                    .get_columns()
                    .iter()
                    .map(|col| FileHeader {
                        name: col.name().to_string(),
                        visible: true,
                        ..FileHeader::default()
                    })
                    .collect::<Vec<_>>();

                if let Err(e) = chan.send(UiMessage::SetMaster(
                    df,
                    t_file_name.clone(),
                    tab_id,
                    headers,
                )) {
                    eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                }

                ctx.request_repaint();
            }
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
                let filter = self.filters.get(&(filename.to_string(), tab_id));

                let sheet_data = match (
                    self.master_data.get(&filename),
                    self.filtered_data.get(&(filename.to_string(), tab_id)),
                    filter,
                ) {
                    (Some(_), None, Some(filter)) if !filter.is_empty() => &DataFrame::empty(),
                    (Some(master), None, _) => master,
                    (Some(_), Some(filtered), _) => filtered,
                    _ => &DataFrame::empty(),
                };

                if !sheet_data.is_empty() {
                    let master_clone = sheet_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();

                    thread::spawn(move || {
                        // let sorted = sort_data(master_clone, sort_order);

                        // if let Err(e) = chan.send(UiMessage::SetSorted(sorted, filename, tab_id)) {
                        //     eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        // }

                        // ctx.request_repaint();
                    });
                }
            };
        }

        ()
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
                if let Some(master_data) = self.master_data.get(&filename) {
                    let master_clone = master_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();

                    // thread::spawn(move || {
                    //     let filtered = filter_data(master_clone, filter);

                    //     if let Err(e) = chan.send(UiMessage::SetSorted(filtered, filename, tab_id))
                    //     {
                    //         eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    //     }

                    //     ctx.request_repaint();
                    // });
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
                UiMessage::SetMaster(master, file_name, tab_id, headers) => {
                    self.master_data.insert(file_name.clone(), master);

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
                }
                UiMessage::SetSorted(sorted, file_name, tab_id) => {
                    // self.filtered_data.insert((file_name, tab_id), sorted);
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
                    self.filters
                        .insert((filename.clone(), tab_id), filter.clone());
                    if let Some(master_df) = self.master_data.get(&filename)
                        && let Ok(df) = filter_df_contains(master_df, &filter)
                    {
                        self.filtered_data.insert((filename, tab_id), df);
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
        });

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show_add_buttons(true)
            .show_add_popup(true)
            .show(
                ctx,
                &mut CsvTabViewer {
                    added_nodes: &mut added_nodes,
                    master_data: &self.master_data,
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

            let last_tab = self.tree.iter_all_tabs().last().unwrap().1;
            let columns = last_tab.columns.clone();

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
