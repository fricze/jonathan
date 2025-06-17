#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use clap::Parser;
use csv::StringRecord;
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
        headers,
        data,
    };

    eframe::run_native(
        "My egui App",
        options,
        Box::new(|_cc| Ok(Box::<MyApp>::new(app))),
    )
}

struct MyApp {
    filename: String,
    headers: StringRecord,
    data: Vec<StringRecord>,
}

impl Default for MyApp {
    fn default() -> Self {
        Self {
            filename: "data.csv".to_owned(),
            headers: StringRecord::default(),
            data: Vec::new(),
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .min_scrolled_height(0.0);
            // .max_scroll_height(500.0);

            for _ in 0..self.headers.len() {
                table = table.column(Column::auto());
            }

            table
                .header(20.0, |mut header| {
                    self.headers.iter().for_each(|content| {
                        header.col(|ui| {
                            ui.heading(content);
                        });
                    });
                })
                .body(|mut body| {
                    for row_data in &self.data {
                        body.row(20.0, |mut row| {
                            row_data.iter().for_each(|data| {
                                row.col(|ui| {
                                    ui.label(data);
                                });
                            });
                        });
                    }
                });
        });
    }
}
