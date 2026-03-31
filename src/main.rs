#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use egui_dock::DockState;
use std::collections::HashMap;
use std::sync::mpsc;

mod app;
mod menu;
mod new_table;
mod read_csv;
mod tabs;
mod types;

use eframe::egui;
use types::{MyApp, Ping, SheetTab, UiMessage};

fn replace_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "IBMPlex".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../IBM_Plex_Mono/IBMPlexMono-Regular.ttf"
        ))),
    );

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

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 240.0])
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

            #[cfg(target_os = "macos")]
            {
                let menu = menu::build_menu();
                menu.init_for_nsapp();
                Box::leak(Box::new(menu));
            }

            Ok(Box::new(MyApp {
                worker_chan,
                ui_chan,
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
                filters: HashMap::new(),
            }))
        }),
    )
}
