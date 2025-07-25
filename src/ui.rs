use crate::types::MyApp;
use egui::Key;
use egui_extras::TableBuilder;
use std::ops::{Add, Sub};

pub fn handle_key_nav<'a>(
    app: &mut MyApp,
    ctx: &egui::Context,
    table: TableBuilder<'a>,
) -> TableBuilder<'a> {
    let mut table = table;

    if ctx.input(|i| i.key_pressed(Key::PageUp)) {
        table = table.vertical_scroll_offset(app.scroll_y.sub(app.inner_rect / 2.0).max(0.0));
    }

    if ctx.input(|i| i.key_pressed(Key::PageDown)) {
        table = table.vertical_scroll_offset(app.scroll_y.add(app.inner_rect / 2.0));
    }

    if ctx.input(|i| i.key_pressed(Key::Home)) {
        table = table.vertical_scroll_offset(0.0);
    }

    if ctx.input(|i| i.key_pressed(Key::End)) {
        table = table.vertical_scroll_offset(app.content_height);
    }

    return table;
}
