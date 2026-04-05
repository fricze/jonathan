use std::{collections::BTreeMap, sync::mpsc::Sender};

use egui::{Align2, Color32, Context, Id, Margin, NumExt as _, Sense, TextFormat};

use crate::data::csv_quote;
use crate::types::{FileHeader, Filename, SelectionState, SheetVec, SortOrder, TabId, UiMessage};

pub struct Table<'a> {
    pub data: &'a SheetVec,
    pub num_columns: usize,
    pub columns: &'a mut Vec<FileHeader>,
    /// Maps visible column index to actual data column index
    pub visible_col_indices: Vec<usize>,
    pub num_rows: u64,
    pub num_sticky_cols: usize,
    pub default_column: egui_table::Column,
    pub auto_size_mode: egui_table::AutoSizeMode,
    pub top_row_height: f32,
    pub row_height: f32,
    pub is_row_expanded: BTreeMap<u64, bool>,
    pub prefetched: Vec<egui_table::PrefetchInfo>,
    pub sender: &'a Sender<UiMessage>,
    pub filename: Filename,
    pub tab_id: TabId,
    pub filter: &'a str,
    pub editing_cell: &'a mut Option<(u64, usize)>,
    pub edit_buffer: &'a mut String,
    pub selection: &'a mut SelectionState,
    pub last_visible_rows: &'a mut Option<std::ops::Range<u64>>,
}

impl<'a> Table<'a> {
    fn cell_content_ui(&mut self, row_nr: u64, col_nr: usize, ui: &mut egui::Ui) {
        // Map visible column index to actual data column index
        let actual_col = self
            .visible_col_indices
            .get(col_nr)
            .copied()
            .unwrap_or(col_nr);

        // --- Edit mode ---
        if *self.editing_cell == Some((row_nr, col_nr)) {
            let _edit_id = Id::new(("cell_edit", row_nr, col_nr, self.tab_id));
            let output = egui::TextEdit::singleline(self.edit_buffer)
                .margin(egui::Margin::ZERO)
                .frame(false)
                .show(ui)
                .response;
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if output.lost_focus() {
                if enter {
                    if let Err(e) = self.sender.send(UiMessage::EditCell(
                        self.filename.clone(),
                        self.tab_id,
                        row_nr,
                        actual_col,
                        self.edit_buffer.clone(),
                    )) {
                        eprintln!("Failed to send EditCell: {:?}", e);
                    }
                }
                // Enter → commit, Escape/click-away → revert (buffer is discarded)
                *self.editing_cell = None;
            } else {
                output.request_focus();
            }
            return;
        }

        // --- Display mode ---
        let row = self.data.get(row_nr as usize);
        if let Some(row) = row {
            let cell = row.get(actual_col);
            if let Some(cell_content) = cell {
                let filter = self.filter;

                let label = if filter.is_empty() {
                    ui.add(
                        egui::Label::new(cell_content)
                            .sense(Sense::click())
                            .extend(),
                    )
                } else {
                    use egui::text::LayoutJob;

                    let highlight = TextFormat {
                        color: Color32::DARK_BLUE,
                        ..Default::default()
                    };
                    let mut job = LayoutJob::default();
                    let mut parts = cell_content.split(filter).peekable();
                    while let Some(part) = parts.next() {
                        if !part.is_empty() {
                            job.append(part, 0.0, TextFormat::default());
                        }
                        if parts.peek().is_some() {
                            job.append(filter, 0.0, highlight.clone());
                        }
                    }
                    ui.add(egui::Label::new(job).sense(Sense::click()).extend())
                };

                if label.clicked() && ui.ctx().input(|i| i.modifiers.command) {
                    if let Err(e) = self.sender.send(UiMessage::FilterSheet(
                        self.filename.to_string(),
                        cell_content.to_string(),
                        self.tab_id,
                        None,
                    )) {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }
            }
        }
    }

    fn handle_clipboard_copy(&self, ui: &egui::Ui) {
        let copy_requested = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if !copy_requested || self.selection.selected_cells.is_empty() {
            return;
        }

        let min_row = self
            .selection
            .selected_cells
            .iter()
            .map(|&(r, _)| r)
            .min()
            .unwrap_or(0);
        let max_row = self
            .selection
            .selected_cells
            .iter()
            .map(|&(r, _)| r)
            .max()
            .unwrap_or(0);
        let min_col = self
            .selection
            .selected_cells
            .iter()
            .map(|&(_, c)| c)
            .min()
            .unwrap_or(0);
        let max_col = self
            .selection
            .selected_cells
            .iter()
            .map(|&(_, c)| c)
            .max()
            .unwrap_or(0);

        let mut csv_rows: Vec<String> = Vec::new();
        for r in min_row..=max_row {
            let mut row_fields: Vec<String> = Vec::new();
            for c in min_col..=max_col {
                if self.selection.selected_cells.contains(&(r, c)) {
                    let actual_col = self.visible_col_indices.get(c).copied().unwrap_or(c);
                    let value = self
                        .data
                        .get(r as usize)
                        .and_then(|row| row.get(actual_col))
                        .unwrap_or("");
                    row_fields.push(csv_quote(value));
                } else {
                    row_fields.push(String::new());
                }
            }
            csv_rows.push(row_fields.join(","));
        }

        ui.ctx().copy_text(csv_rows.join("\n"));
        crate::toast::show(ui.ctx(), "Copied to clipboard");
    }

    fn handle_keyboard_navigation(&mut self, ui: &egui::Ui) -> Option<u64> {
        if self.editing_cell.is_some() {
            return None;
        }
        let (anchor_row, anchor_col) = self.selection.anchor_cell?;

        let shift = ui.input(|i| i.modifiers.shift);
        let pressed_up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
        let pressed_down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
        let pressed_left = ui.input(|i| i.key_pressed(egui::Key::ArrowLeft));
        let pressed_right = ui.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let pressed_pgup = ui.input(|i| i.key_pressed(egui::Key::PageUp));
        let pressed_pgdown = ui.input(|i| i.key_pressed(egui::Key::PageDown));

        let (cur_row, cur_col) = self.selection.cursor().unwrap_or((anchor_row, anchor_col));

        let new_pos: Option<(u64, usize)> = if pressed_up && cur_row > 0 {
            Some((cur_row - 1, cur_col))
        } else if pressed_down && cur_row + 1 < self.num_rows {
            Some((cur_row + 1, cur_col))
        } else if pressed_pgup {
            Some((cur_row.saturating_sub(20), cur_col))
        } else if pressed_pgdown {
            Some(((cur_row + 20).min(self.num_rows - 1), cur_col))
        } else if pressed_left && cur_col > 0 {
            Some((cur_row, cur_col - 1))
        } else if pressed_right && cur_col + 1 < self.num_columns {
            Some((cur_row, cur_col + 1))
        } else {
            None
        };

        let (new_row, new_col) = new_pos?;
        if shift {
            self.selection.extend_to(new_row, new_col);
        } else {
            self.selection.select_single(new_row, new_col);
        }
        Some(new_row)
    }

    fn handle_drag_autoscroll(&mut self, ui: &egui::Ui) -> Option<u64> {
        if !self.selection.is_dragging() || !ui.input(|i| i.pointer.is_decidedly_dragging()) {
            return None;
        }

        let pos = ui.input(|i| i.pointer.latest_pos())?;
        let vis = self.last_visible_rows.clone()?;

        let rect = ui.clip_rect();
        let edge_zone = 40.0;
        let end_col = self.selection.cursor().map(|(_, c)| c).unwrap_or(0);

        let target_row = if pos.y < rect.min.y + edge_zone && vis.start > 0 {
            Some(vis.start - 1)
        } else if pos.y > rect.max.y - edge_zone && vis.end < self.num_rows {
            Some(vis.end)
        } else {
            None
        }?;

        let target_row = target_row.min(self.num_rows - 1);
        self.selection.extend_to(target_row, end_col);
        ui.ctx().request_repaint();
        Some(target_row)
    }

    fn draw_selection_border(&self, ui: &egui::Ui, row_nr: u64, col_nr: usize, r: egui::Rect) {
        let is_editing = *self.editing_cell == Some((row_nr, col_nr));
        if !is_editing {
            ui.painter()
                .rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(0, 200, 80, 15));
        }

        let stroke = egui::Stroke::new(2.0, Color32::GREEN);
        let dash = 4.0;
        let gap = 3.0;
        let painter = ui.painter();

        if !(row_nr > 0 && self.selection.contains(row_nr - 1, col_nr)) {
            painter.extend(egui::Shape::dashed_line(
                &[r.left_top(), r.right_top()],
                stroke,
                dash,
                gap,
            ));
        }
        if !self.selection.contains(row_nr, col_nr + 1) {
            painter.extend(egui::Shape::dashed_line(
                &[r.right_top(), r.right_bottom()],
                stroke,
                dash,
                gap,
            ));
        }
        if !self.selection.contains(row_nr + 1, col_nr) {
            painter.extend(egui::Shape::dashed_line(
                &[r.right_bottom(), r.left_bottom()],
                stroke,
                dash,
                gap,
            ));
        }
        if !(col_nr > 0 && self.selection.contains(row_nr, col_nr - 1)) {
            painter.extend(egui::Shape::dashed_line(
                &[r.left_bottom(), r.left_top()],
                stroke,
                dash,
                gap,
            ));
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if ui.input(|i| i.pointer.any_released()) {
            self.selection.end_drag();
        }

        self.handle_clipboard_copy(ui);

        if self.editing_cell.is_none() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some((row_nr, col_nr)) = self.selection.cursor().or(self.selection.anchor_cell) {
                let actual_col = self
                    .visible_col_indices
                    .get(col_nr)
                    .copied()
                    .unwrap_or(col_nr);
                if let Some(content) = self
                    .data
                    .get(row_nr as usize)
                    .and_then(|r| r.get(actual_col))
                {
                    *self.editing_cell = Some((row_nr, col_nr));
                    *self.edit_buffer = content.to_string();
                }
            }
        }

        let nav_scroll = self.handle_keyboard_navigation(ui);
        let drag_scroll = self.handle_drag_autoscroll(ui);
        let scroll_to_row = drag_scroll.or(nav_scroll);

        let id_salt = Id::new("table_demo");
        let _state_id = egui_table::Table::new().id_salt(id_salt).get_id(ui);

        let mut table = egui_table::Table::new()
            .id_salt(id_salt)
            .num_rows(self.num_rows)
            .columns(vec![self.default_column; self.num_columns])
            .num_sticky_cols(self.num_sticky_cols)
            .headers([
                egui_table::HeaderRow {
                    height: self.top_row_height,
                    groups: if self.num_columns > 0 {
                        vec![0..self.num_columns]
                    } else {
                        vec![]
                    },
                },
                egui_table::HeaderRow::new(self.top_row_height),
            ])
            .auto_size_mode(self.auto_size_mode);

        if let Some(row) = scroll_to_row {
            table = table.scroll_to_row(row, None);
        }

        table.show(ui, self);
    }
}

impl<'a> egui_table::TableDelegate for Table<'a> {
    fn prepare(&mut self, info: &egui_table::PrefetchInfo) {
        assert!(
            info.visible_rows.end <= self.num_rows,
            "Was asked to prefetch rows {:?}, but we only have {} rows. This is a bug in egui_table.",
            info.visible_rows,
            self.num_rows
        );
        *self.last_visible_rows = Some(info.visible_rows.clone());
        self.prefetched.push(info.clone());
    }

    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell_inf: &egui_table::HeaderCellInfo) {
        let egui_table::HeaderCellInfo {
            group_index,
            col_range,
            row_nr,
            ..
        } = cell_inf;

        // Map visible column index to actual column index
        let actual_col_index = self
            .visible_col_indices
            .get(*group_index)
            .copied()
            .unwrap_or(*group_index);

        let margin = 4;

        egui::Frame::NONE
            .inner_margin(Margin::symmetric(margin, 0))
            .show(ui, |ui| {
                if *row_nr == 0 {
                    if 0 < col_range.start {
                        // Our special grouped column.
                        let sticky = true;
                        let text = format!("This is group {group_index}");
                        if sticky {
                            let font_id = egui::TextStyle::Heading.resolve(ui.style());
                            let text_color = ui.visuals().text_color();
                            let galley =
                                ui.painter()
                                    .layout(text, font_id, text_color, f32::INFINITY);

                            // Put the text leftmost in the clip rect (so it is always visible)
                            let mut pos = Align2::LEFT_CENTER
                                .anchor_size(
                                    ui.clip_rect().shrink(margin as _).left_center(),
                                    galley.size(),
                                )
                                .min;

                            // … but not so far to the right that it doesn't fit.
                            pos.x = pos.x.at_most(ui.max_rect().right() - galley.size().x);

                            ui.put(
                                egui::Rect::from_min_size(pos, galley.size()),
                                egui::Label::new(galley),
                            );
                        } else {
                            ui.heading(text);
                        }
                    }
                } else {
                    let header = self.columns.get(actual_col_index);
                    if let Some(header) = header {
                        let name = if header.name.is_empty() && actual_col_index == 0 {
                            "id"
                        } else {
                            &header.name
                        };
                        ui.heading(name);

                        let button_symbol = if let Some(sort) = header.sort {
                            if sort == SortOrder::Asc { "⬆" } else { "⬇" }
                        } else {
                            "→"
                        };

                        if ui.button(button_symbol).clicked() {
                            self.columns
                                .iter_mut()
                                .enumerate()
                                .for_each(|(idx, header)| {
                                    if idx == actual_col_index {
                                        let new_sort = if let Some(sort) = header.sort {
                                            if sort == SortOrder::Asc {
                                                SortOrder::Dsc
                                            } else {
                                                SortOrder::Asc
                                            }
                                        } else {
                                            SortOrder::Asc
                                        };

                                        header.sort = Some(new_sort);

                                        if let Err(e) = self.sender.send(UiMessage::SortSheet(
                                            self.filename.clone(),
                                            (actual_col_index, new_sort.clone()),
                                            self.tab_id,
                                        )) {
                                            println!("{:?}", e)
                                        };

                                        ()
                                    } else {
                                        header.sort = None
                                    }
                                });
                        }
                    }
                }
            });
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell_info: &egui_table::CellInfo) {
        let egui_table::CellInfo { row_nr, col_nr, .. } = *cell_info;

        if row_nr % 2 == 1 {
            ui.painter()
                .rect_filled(ui.max_rect(), 0.0, ui.visuals().faint_bg_color);
        }

        let cell_rect = ui.max_rect();

        if *self.editing_cell == Some((row_nr, col_nr)) {
            ui.painter().rect_filled(cell_rect, 0.0, Color32::WHITE);
        }

        egui::Frame::NONE
            .inner_margin(Margin::symmetric(4, 0))
            .show(ui, |ui| {
                self.cell_content_ui(row_nr, col_nr, ui);
            });

        let cell_response = ui.interact(
            cell_rect,
            Id::new(("cell", row_nr, col_nr, self.tab_id)),
            Sense::click_and_drag(),
        );

        if cell_response.drag_started() {
            self.selection.start_drag(row_nr, col_nr);
        } else if self.selection.is_dragging() && ui.input(|i| i.pointer.is_decidedly_dragging()) {
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                if cell_rect.contains(pos) {
                    self.selection.update_drag(row_nr, col_nr);
                }
            }
        }

        if cell_response.double_clicked() {
            let actual_col = self
                .visible_col_indices
                .get(col_nr)
                .copied()
                .unwrap_or(col_nr);
            if let Some(content) = self
                .data
                .get(row_nr as usize)
                .and_then(|r| r.get(actual_col))
            {
                *self.editing_cell = Some((row_nr, col_nr));
                *self.edit_buffer = content.to_string();
            }
        } else if cell_response.clicked() {
            let modifiers = ui.ctx().input(|i| i.modifiers);
            if modifiers.command {
                self.selection.toggle(row_nr, col_nr);
            } else if modifiers.shift {
                self.selection.extend_to(row_nr, col_nr);
            } else {
                self.selection.select_single(row_nr, col_nr);
            }
        }

        if self.selection.contains(row_nr, col_nr) {
            self.draw_selection_border(ui, row_nr, col_nr, cell_rect);
        }
    }

    fn row_top_offset(&self, ctx: &Context, _table_id: Id, row_nr: u64) -> f32 {
        let fully_expanded_row_height = 48.0;

        self.is_row_expanded
            .range(0..row_nr)
            .map(|(expanded_row_nr, expanded)| {
                let how_expanded = ctx.animate_bool(Id::new(expanded_row_nr), *expanded);
                how_expanded * fully_expanded_row_height
            })
            .sum::<f32>()
            + row_nr as f32 * self.row_height
    }
}
