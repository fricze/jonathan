#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use csv::StringRecord;
use egui::Key;
use egui::ScrollArea;
use egui::scroll_area::ScrollAreaOutput;
use std::fs::File;
use std::path::PathBuf;
use std::{
    collections::HashSet,
    ops::{Add, Sub},
};

use egui_extras::{Column, Table, TableBuilder};

use eframe::egui;
use jonathan::read_csv;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 240.0]) // wide enough for the drag-drop overlay text
            .with_drag_and_drop(true),
        ..Default::default()
    };

    let app = MyApp {
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
    };

    let title = &format!("CSV Reader. {}", app.filename);

    eframe::run_native(title, options, Box::new(|_cc| Ok(Box::<MyApp>::new(app))))
}

struct FileHeader {
    name: String,
    visible: bool,
}

#[derive(Default)]
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
}

fn open_csv_file(app: &mut MyApp, path: &str) {
    match read_csv::iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            app.reader = Some(csv_reader);
            app.headers = Some(
                headers
                    .into_iter()
                    .map(|name| FileHeader {
                        name: name.to_string(),
                        visible: true,
                    })
                    .collect(),
            );
        }
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    };
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
    return table_ui.body(|mut body| {
        let filtered_rows = data
            .iter()
            .filter(|row| row.iter().find(|text| text.contains(filter)).is_some());

        for row_data in filtered_rows {
            body.row(20.0, |mut row| {
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
        }
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
            ui.label(format!(
                "CSV reader :: {}, {}, {}, {}, {}",
                self.filename,
                self.content_height,
                self.inner_rect.add(self.scroll_y),
                self.page,
                self.loading
            ));

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

                    let page_size = 100;
                    // Reader keeps state, so if I've taken some amount of records already
                    // from it, I should not call .skip when taking next records.
                    // The position is already moved.
                    let data = self.reader.as_mut().unwrap().records().take(page_size);
                    let filtered_data = data.filter_map(|record| record.ok()).collect::<Vec<_>>();

                    if !filtered_data.is_empty() {
                        self.data.as_mut().unwrap().extend(filtered_data);
                        self.page = page_no;
                    }
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

                    open_csv_file(self, str_path);

                    let page_size = 100;
                    let data = self.reader.as_mut().unwrap().records().take(page_size);
                    let filtered_data = data.filter_map(|record| record.ok()).collect::<Vec<_>>();

                    self.data = Some(filtered_data);

                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name));
                }
            }

            show_dropped_files(ui, &self.dropped_files);

            ui.separator();

            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Write something here"));

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
                            ui.heading(&file_header.name);
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

                open_csv_file(self, str_path);
            }
        });

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            file_name.unwrap_or("".to_string()),
        ));
    }
}
