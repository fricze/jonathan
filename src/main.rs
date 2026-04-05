#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use egui_dock::DockState;
use std::collections::HashMap;
use std::sync::mpsc;

mod app;
mod data;
mod menu;
mod new_table;
mod read_csv;
mod tabs;
mod toast;
mod types;
mod ui;

use eframe::egui;
use std::collections::HashSet;
use std::sync::Arc;
use types::{MyApp, Ping, SheetTab, UiMessage};

fn main() -> eframe::Result {
    dioxus_devtools::connect_subsecond();

    subsecond::call(|| {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([640.0, 240.0])
                .with_drag_and_drop(true),
            ..Default::default()
        };

        let title = "CSV Reader.";

        let worker_chan = mpsc::channel::<UiMessage>();
        let ui_chan = mpsc::channel::<Ping>();

        eframe::run_native(
            title,
            options,
            Box::new(|cc| {
                ui::fonts::replace_fonts(&cc.egui_ctx);

                {
                    let ctx = cc.egui_ctx.clone();
                    subsecond::register_handler(Arc::new(move || ctx.request_repaint()));
                }

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
                    dirty_files: HashSet::new(),
                }))
            }),
        )
    })
}
