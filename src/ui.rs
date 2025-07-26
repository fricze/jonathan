use crate::types::SheetTab;
use egui::Key;
use egui_extras::TableBuilder;
use std::ops::{Add, Sub};

pub fn handle_key_nav<'a>(
    tab: &mut SheetTab,
    ctx: &egui::Context,
    table: TableBuilder<'a>,
) -> TableBuilder<'a> {
    let mut table = table;

    if ctx.input(|i| i.key_pressed(Key::PageUp)) {
        table = table.vertical_scroll_offset(tab.scroll_y.sub(tab.inner_rect / 2.0).max(0.0));
    }

    if ctx.input(|i| i.key_pressed(Key::PageDown)) {
        table = table.vertical_scroll_offset(tab.scroll_y.add(tab.inner_rect / 2.0));
    }

    if ctx.input(|i| i.key_pressed(Key::Home)) {
        table = table.vertical_scroll_offset(0.0);
    }

    if ctx.input(|i| i.key_pressed(Key::End)) {
        table = table.vertical_scroll_offset(tab.content_height);
    }

    return table;
}
