use std::{collections::BTreeMap, sync::mpsc::Sender};

use egui::{Align2, Color32, Context, Id, Margin, NumExt as _, TextFormat};
use polars::frame::DataFrame;

use crate::types::{FileHeader, Filename, SheetVec, SortOrder, TabId, UiMessage};

pub struct Table<'a> {
    pub df: &'a DataFrame,
    pub filtered_df: &'a DataFrame,
    pub view: DataFrame,
    pub data: &'a SheetVec,
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
    pub sender: &'a Sender<UiMessage>,
    pub filename: Filename,
    pub tab_id: TabId,
    pub filter: &'a str,
    pub start: i64,
}

impl<'a> Table<'a> {
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

        // let df_row = self.view.get_row_amortized(idx, row)

        let mut row_idx = row_nr as usize;
        let start = self.start as usize;
        row_idx = row_idx - start;

        let df_row = self.view.get_row(row_idx);

        if let Ok(row) = df_row
            && let Some(val) = row.0.get(col_nr)
        {
            let cell_content = val.str_value();

            let filter = self.filter;

            let label = if filter.is_empty() {
                ui.label(cell_content.clone())
            } else {
                use egui::text::LayoutJob;

                if cell_content.contains(filter) {
                    let cell_content = &cell_content;
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

                        ui.label(job)
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
                            ui.label(job)
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

                            ui.label(job)
                        } else {
                            ui.label(job)
                        }
                    }
                } else {
                    ui.label(cell_content.clone())
                }
            };

            if label.clicked() {
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
        let start = info.visible_rows.start as i64;
        let end = info.visible_rows.end as usize;
        self.start = start;

        // Overscan to reduce thrash when scrolling:
        let overscan = 20i64;

        self.view = if !self.filtered_df.is_empty() {
            let res = self
                .filtered_df
                .slice(start.saturating_sub(overscan), end + 200);

            res
        } else {
            self.df.slice(start.saturating_sub(overscan), end + 200)
        };

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
                                            (group_index.clone(), new_sort.clone()),
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
        let id_salt = Id::new(("table", &self.filename, self.filter));
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
