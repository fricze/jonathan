// src/menu.rs
use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

pub const OPEN_FILE_ID: &str = "open_file";

pub fn build_menu() -> Menu {
    let menu = Menu::new();

    // App menu (required for proper macOS behavior)
    let app_menu = Submenu::new("Jonathan", true);
    app_menu.append(&PredefinedMenuItem::about(None, None)).unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu.append(&PredefinedMenuItem::services(None)).unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu.append(&PredefinedMenuItem::hide(None)).unwrap();
    app_menu.append(&PredefinedMenuItem::hide_others(None)).unwrap();
    app_menu.append(&PredefinedMenuItem::show_all(None)).unwrap();
    app_menu.append(&PredefinedMenuItem::separator()).unwrap();
    app_menu.append(&PredefinedMenuItem::quit(None)).unwrap();

    // File menu
    let open_item = MenuItem::with_id(MenuId::new(OPEN_FILE_ID), "Open CSV...", true, None);

    let file_menu = Submenu::new("File", true);
    file_menu.append(&open_item).unwrap();
    file_menu.append(&PredefinedMenuItem::separator()).unwrap();
    file_menu.append(&PredefinedMenuItem::close_window(None)).unwrap();

    // Edit menu (standard macOS copy/paste etc.)
    let edit_menu = Submenu::new("Edit", true);
    edit_menu.append(&PredefinedMenuItem::undo(None)).unwrap();
    edit_menu.append(&PredefinedMenuItem::redo(None)).unwrap();
    edit_menu.append(&PredefinedMenuItem::separator()).unwrap();
    edit_menu.append(&PredefinedMenuItem::cut(None)).unwrap();
    edit_menu.append(&PredefinedMenuItem::copy(None)).unwrap();
    edit_menu.append(&PredefinedMenuItem::paste(None)).unwrap();
    edit_menu.append(&PredefinedMenuItem::select_all(None)).unwrap();

    // Window menu
    let window_menu = Submenu::new("Window", true);
    window_menu.append(&PredefinedMenuItem::minimize(None)).unwrap();
    window_menu.append(&PredefinedMenuItem::maximize(None)).unwrap();
    window_menu.append(&PredefinedMenuItem::fullscreen(None)).unwrap();

    menu.append(&app_menu).unwrap();
    menu.append(&file_menu).unwrap();
    menu.append(&edit_menu).unwrap();
    menu.append(&window_menu).unwrap();

    menu
}
