#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use clap::Parser;
use csv::StringRecord;
use egui::Key;
use egui_file::FileDialog;
use std::{
    collections::HashSet,
    ops::{Add, Sub},
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use egui_extras::{Column, TableBuilder};

use eframe::egui;
use jonathan::read_csv;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Path to the CSV file
    file: String,
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([320.0, 240.0]),
        ..Default::default()
    };

    let args = Args::parse();

    let (data, headers) = match read_csv::read_csv(&args.file) {
        Ok((data, headers)) => (data, headers),
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    };

    let app = MyApp {
        filename: args.file,
        // headers: Some(
        //     headers
        //         .into_iter()
        //         .map(|name| FileHeader {
        //             name: name.to_string(),
        //             visible: true,
        //         })
        //         .collect(),
        // ),
        // data: Some(data),
        headers: None,
        data: None,
        scroll_y: 0.0,
        inner_rect: 0.0,
        content_height: 0.0,
        filter: "".to_owned(),
        opened_file: None,
        open_file_dialog: None,
    };

    eframe::run_native(
        &format!("Jonathan CSV Reader. File: {}", app.filename),
        options,
        Box::new(|_cc| Ok(Box::<MyApp>::new(app))),
    )
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
    opened_file: Option<PathBuf>,
    open_file_dialog: Option<FileDialog>,
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(format!("CSV reader :: {}", self.filename));

            if let Some(headers) = self.headers.as_mut() {
                ui.horizontal(|ui| {
                    for file_header in headers.iter_mut() {
                        ui.checkbox(&mut file_header.visible, &file_header.name)
                            .on_hover_text(format!("Show/hide column {}", file_header.name));
                    }
                });
            }

            if (ui.button("Open")).clicked() {
                let filter = Box::new({
                    let ext = Some(OsStr::new("csv"));
                    move |path: &Path| -> bool { path.extension() == ext }
                });
                let mut dialog =
                    FileDialog::open_file(self.opened_file.clone()).show_files_filter(filter);
                dialog.open();
                self.open_file_dialog = Some(dialog);
            }

            if let Some(dialog) = &mut self.open_file_dialog {
                if dialog.show(ctx).selected() {
                    if let Some(file) = dialog.path() {
                        self.opened_file = Some(file.to_path_buf());

                        let path = file.to_path_buf();
                        let read_path = path.to_str().unwrap_or("");

                        match read_csv::read_csv(&read_path) {
                            Ok((data, headers)) => {
                                self.data = Some(data);
                                self.headers = Some(
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
                }
            }

            ui.separator();

            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Write something here"));

            ui.separator();

            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .min_scrolled_height(0.0);

            if ctx.input(|i| i.key_pressed(Key::PageUp)) {
                table =
                    table.vertical_scroll_offset(self.scroll_y.sub(self.inner_rect / 2.0).max(0.0));
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

            if let Some(headers) = self.headers.as_ref() {
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

                if let Some(data) = self.data.as_ref() {
                    let scroll_area = table_ui.body(|mut body| {
                        let filtered_rows = data.iter().filter(|row| {
                            row.iter()
                                .find(|text| text.contains(&self.filter))
                                .is_some()
                        });

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

                    let content_height = scroll_area.content_size[1];

                    self.content_height = content_height;

                    let offset = scroll_area.state.offset[1];
                    self.scroll_y = offset;
                    self.inner_rect = scroll_area.inner_rect.height();
                }
            }
        });
    }
}
