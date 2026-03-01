use std::{collections::BTreeMap, sync::mpsc::Sender};

use egui::{Align2, Color32, Context, Id, Margin, NumExt as _, Sense, TextFormat};

use crate::types::{FileHeader, Filename, SheetVec, SortOrder, TabId, UiMessage};

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
}

impl<'a> Table<'a> {
    // fn was_row_prefetched(&self, row_nr: u64) -> bool {
    //     self.prefetched
    //         .iter()
    //         .any(|info| info.visible_rows.contains(&row_nr))
    // }

    fn cell_content_ui(&mut self, row_nr: u64, col_nr: usize, ui: &mut egui::Ui) {
        // Map visible column index to actual data column index
        let actual_col = self.visible_col_indices.get(col_nr).copied().unwrap_or(col_nr);

        // --- Edit mode ---
        if *self.editing_cell == Some((row_nr, col_nr)) {
            let edit_id = Id::new(("cell_edit", row_nr, col_nr, self.tab_id));
            let response = ui.add(
                egui::TextEdit::singleline(self.edit_buffer)
                    .id(edit_id)
                    .desired_width(f32::INFINITY),
            );
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if response.lost_focus() {
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
                response.request_focus();
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
                    ui.add(egui::Label::new(cell_content).sense(Sense::click()))
                } else {
                    use egui::text::LayoutJob;

                    if cell_content.contains(filter) {
                        let mut job = LayoutJob::default();

                        if cell_content == filter {
                            job.append(
                                cell_content,
                                0.0,
                                TextFormat {
                                    color: Color32::DARK_BLUE,
                                    ..Default::default()
                                },
                            );

                            ui.add(egui::Label::new(job).sense(Sense::click()))
                        } else {
                            let text: Vec<&str> = cell_content.split(&filter).collect();

                            if text.len() == 1 {
                                job.append(
                                    &filter,
                                    0.0,
                                    TextFormat {
                                        color: Color32::DARK_BLUE,
                                        ..Default::default()
                                    },
                                );
                                job.append(text[0], 0.0, TextFormat::default());
                                ui.add(egui::Label::new(job).sense(Sense::click()))
                            } else if text.len() == 2 {
                                job.append(text[0], 0.0, TextFormat::default());
                                job.append(
                                    &filter,
                                    0.0,
                                    TextFormat {
                                        color: Color32::DARK_BLUE,
                                        ..Default::default()
                                    },
                                );
                                job.append(text[1], 0.0, TextFormat::default());

                                ui.add(egui::Label::new(job).sense(Sense::click()))
                            } else {
                                ui.add(egui::Label::new(job).sense(Sense::click()))
                            }
                        }
                    } else {
                        ui.add(egui::Label::new(cell_content).sense(Sense::click()))
                    }
                };

                if label.double_clicked() {
                    *self.editing_cell = Some((row_nr, col_nr));
                    *self.edit_buffer = cell_content.to_string();
                } else if label.clicked() {
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

        // ui.vertical(|ui| {
        //     ui.horizontal(|ui| {
        //         ui.label(format!("({row_nr}, {col_nr})"));

        //         if (row_nr + col_nr as u64) % 27 == 0 {
        //             if !ui.is_sizing_pass() {
        //                 // During a sizing pass we don't truncate!
        //                 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        //             }
        //             ui.label("Extra long cell!");
        //         }
        //     });
        // });
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
        let actual_col_index = self.visible_col_indices.get(*group_index).copied().unwrap_or(*group_index);

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

        egui::Frame::NONE
            .inner_margin(Margin::symmetric(4, 0))
            .show(ui, |ui| {
                self.cell_content_ui(row_nr, col_nr, ui);
            });
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

impl<'a> Table<'a> {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let id_salt = Id::new("table_demo");
        let state_id = egui_table::Table::new().id_salt(id_salt).get_id(ui); // Note: must be here (in the correct outer `ui` scope) to be correct.

        let table = egui_table::Table::new()
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

        table.show(ui, self);
    }
}
