use std::{collections::BTreeMap, sync::Arc};

use csv::StringRecord;
use egui::{Align2, Context, Id, Margin, NumExt as _, Sense, Vec2};

use crate::types::{FileHeader, SortOrder};

// #[derive(serde::Deserialize, serde::Serialize)]
pub struct TableDemo<'a> {
    pub data: Vec<Arc<StringRecord>>,
    pub num_columns: usize,
    pub columns: &'a mut Vec<FileHeader>,
    pub num_rows: u64,
    pub num_sticky_cols: usize,
    pub default_column: egui_table::Column,
    pub auto_size_mode: egui_table::AutoSizeMode,
    pub top_row_height: f32,
    pub row_height: f32,
    pub is_row_expanded: BTreeMap<u64, bool>,
    pub prefetched: Vec<egui_table::PrefetchInfo>,
}

// impl<'a> Default for TableDemo<'a> {
//     fn default() -> Self {
//         Self {
//             num_columns: 20,
//             num_rows: 10_000,
//             num_sticky_cols: 1,
//             default_column: egui_table::Column::new(100.0)
//                 .range(10.0..=500.0)
//                 .resizable(true),
//             auto_size_mode: egui_table::AutoSizeMode::default(),
//             top_row_height: 24.0,
//             row_height: 18.0,
//             is_row_expanded: Default::default(),
//             prefetched: vec![],
//             data: None,
//             columns: &mut vec![],
//         }
//     }
// }

impl<'a> TableDemo<'a> {
    // fn was_row_prefetched(&self, row_nr: u64) -> bool {
    //     self.prefetched
    //         .iter()
    //         .any(|info| info.visible_rows.contains(&row_nr))
    // }

    fn cell_content_ui(&mut self, row_nr: u64, col_nr: usize, ui: &mut egui::Ui) {
        // assert!(
        //     self.was_row_prefetched(row_nr),
        //     "Was asked to show row {row_nr} which was not prefetched! This is a bug in egui_table."
        // );

        // let is_expanded = self
        //     .is_row_expanded
        //     .get(&row_nr)
        //     .copied()
        //     .unwrap_or_default();
        // let expandedness = ui.ctx().animate_bool(Id::new(row_nr), is_expanded);
        //

        let row = self.data.get(row_nr as usize);
        if let Some(row) = row {
            let cell = row.get(col_nr as usize);
            if let Some(cell) = cell {
                ui.label(cell);
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

impl<'a> egui_table::TableDelegate for TableDemo<'a> {
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
                    let header = self.columns.get(group_index.clone());
                    if let Some(header) = header {
                        let name = &header.name;

                        if col_range.start == 0 && name.is_empty() {
                            ui.heading("ID");
                        } else {
                            ui.heading(&header.name);
                        }

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
                                    if idx == group_index.clone() {
                                        if let Some(sort) = header.sort {
                                            if sort == SortOrder::Asc {
                                                header.sort = Some(SortOrder::Dsc)
                                            } else {
                                                header.sort = Some(SortOrder::Asc)
                                            }
                                        } else {
                                            header.sort = Some(SortOrder::Asc)
                                        }
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

impl<'a> TableDemo<'a> {
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
