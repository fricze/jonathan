// src/menu.rs
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use std::sync::atomic::{AtomicBool, Ordering};

pub const OPEN_FILE_ID: &str = "open_file";
pub const COPY_AS_CSV_ID: &str = "copy_as_csv";

/// Set by the menu handler, consumed by the table that owns the selection.
static COPY_AS_CSV_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_copy_as_csv() {
    COPY_AS_CSV_REQUESTED.store(true, Ordering::Relaxed);
}

/// True once per "Copy as CSV" request.
pub fn take_copy_as_csv_request() -> bool {
    COPY_AS_CSV_REQUESTED.swap(false, Ordering::Relaxed)
}

pub fn build_menu() -> Menu {
    let menu = Menu::new();

    // App menu (required for proper macOS behavior)
    let app_menu = Submenu::new("Jonathan", true);
    app_menu
        .append(&PredefinedMenuItem::about(None, None))
        .unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu
        .append(&PredefinedMenuItem::services(None))
        .unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu.append(&PredefinedMenuItem::hide(None)).unwrap();
    app_menu
        .append(&PredefinedMenuItem::hide_others(None))
        .unwrap();
    app_menu
        .append(&PredefinedMenuItem::show_all(None))
        .unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu.append(&PredefinedMenuItem::quit(None)).unwrap();

    // File menu
    let open_item = MenuItem::with_id(MenuId::new(OPEN_FILE_ID), "Open CSV...", true, None);

    let file_menu = Submenu::new("File", true);
    file_menu.append(&open_item).unwrap();
    file_menu.append(&PredefinedMenuItem::separator()).unwrap();
    file_menu
        .append(&PredefinedMenuItem::close_window(None))
        .unwrap();

    // Edit menu
    let copy_csv_item = MenuItem::with_id(
        MenuId::new(COPY_AS_CSV_ID),
        "Copy as CSV",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyC)),
    );
    let edit_menu = Submenu::new("Edit", true);
    edit_menu.append(&copy_csv_item).unwrap();

    // Window menu
    let window_menu = Submenu::new("Window", true);
    window_menu
        .append(&PredefinedMenuItem::minimize(None))
        .unwrap();
    window_menu
        .append(&PredefinedMenuItem::maximize(None))
        .unwrap();
    window_menu
        .append(&PredefinedMenuItem::fullscreen(None))
        .unwrap();

    menu.append(&app_menu).unwrap();
    menu.append(&file_menu).unwrap();
    menu.append(&edit_menu).unwrap();
    menu.append(&window_menu).unwrap();

    menu
}
