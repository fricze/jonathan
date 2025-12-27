use egui::{Align, Align2, Id, LayerId, Order, Response, TextStyle};
use egui::{Key, Stroke};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{NodeIndex, SurfaceIndex};

use egui::Color32;
use polars::frame::DataFrame;

use crate::types::{CsvTabViewer, FileHeader, SheetTab, UiMessage};
use eframe::egui;

use std::collections::HashSet;
use std::sync::mpsc::Sender;

use crate::new_table::Table;

fn open_file_dialog(sender: &Sender<UiMessage>, tab: &usize) {
    if let Some(paths) = rfd::FileDialog::new()
        .add_filter("csv", &["csv"])
        .pick_files()
    {
        for path in paths {
            if let Err(e) = sender.send(UiMessage::OpenFile(path.display().to_string(), Some(*tab)))
            {
                eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
            }
        }
    }
}

fn get_last_element_from_path(s: &str) -> Option<&str> {
    s.split('/').last()
}

fn display_headers(ui: &mut egui::Ui, headers: &mut Vec<FileHeader>) {
    ui.collapsing("Show/hide columns", |ui| {
        ui.horizontal_wrapped(|ui| {
            let mut hidden = HashSet::new();

            for (index, file_header) in headers.iter_mut().enumerate() {
                if file_header.visible {
                    hidden.remove(&index);
                } else {
                    hidden.insert(index);
                }

                if ui
                    .checkbox(&mut file_header.visible, &file_header.name)
                    .on_hover_text(format!("Show/hide column {}", file_header.name))
                    .clicked()
                {
                    if file_header.visible {
                        hidden.remove(&index);
                    } else {
                        hidden.insert(index);
                    }
                }
            }
        });
    });
}

fn file_button(ui: &mut egui::Ui, file: &str) -> Response {
    let mut l: Option<Response> = None;
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(4, 8)) // Horizontal 20, Vertical 10 padding
        .show(ui, |ui| {
            l = Some(ui.selectable_label(false, file));
        });

    let mut rect = l.unwrap().rect;

    rect.set_left(rect.left() - 4.0);
    rect.set_top(rect.top() - 4.0);
    rect.set_width(120.0);
    rect.set_height(rect.height() + 8.0);

    let response = ui
        .put(rect, egui::Label::new(""))
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let border_color = if response.hovered() {
        Color32::from_rgb(100, 100, 255)
    } else {
        Color32::GRAY
    };

    let stroke_rect = rect.clone();
    ui.painter().rect_stroke(
        stroke_rect,
        4.0,
        Stroke::new(1.0, border_color),
        egui::StrokeKind::Outside,
    );

    response
}

impl egui_dock::TabViewer for CsvTabViewer<'_> {
    type Tab = SheetTab;

    fn add_popup(&mut self, ui: &mut egui::Ui, surface: SurfaceIndex, node: NodeIndex) {
        ui.set_min_width(120.0);
        ui.set_max_width(120.0);

        ui.with_layout(egui::Layout::default().with_cross_align(Align::Min), |ui| {
            ui.set_width(120.0);

            if file_button(ui, "Empty tab").clicked() {
                self.added_nodes.push((surface, node, "".to_string()));
            }

            for path in self.files_list {
                if let Some(file) = get_last_element_from_path(path) {
                    if file_button(ui, file).clicked() {
                        self.added_nodes.push((surface, node, path.to_string()));
                    }
                }
            }
        });
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        let file = get_last_element_from_path(&tab.chosen_file);
        let tab_id = &tab.id;

        if let Some(file) = file {
            if file.is_empty() {
                format!("[tab {tab_id}] Load file").into()
            } else {
                format!("[tab {tab_id}] {file}").into()
            }
        } else {
            format!("[tab {tab_id}] Load file").into()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let tab_id = tab.id;

        ui.add_space(4.0);

        if ui.button("Open file…").clicked() {
            open_file_dialog(&self.sender, &tab_id);
        }

        ui.add_space(4.0);

        if !self.files_list.is_empty() {
            let radio = &tab.chosen_file;
            egui::ComboBox::from_label("Select file")
                .selected_text(get_last_element_from_path(radio).unwrap_or(""))
                .show_ui(ui, |ui| {
                    for file in self.files_list {
                        let filename = file.clone();
                        ui.selectable_value(
                            &mut tab.chosen_file,
                            filename,
                            get_last_element_from_path(file).unwrap_or(""),
                        );
                    }
                });

            ui.add_space(4.0);
        }

        let chosen_file = &tab.chosen_file.clone();

        if !chosen_file.is_empty() {
            if let Some(filter) = self.filters.get_mut(&(chosen_file.clone(), tab_id)) {
                ui.horizontal_wrapped(|ui| {
                    if ui.text_edit_singleline(filter).changed() {
                        if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                            chosen_file.to_string(),
                            filter.to_string(),
                            tab_id,
                            None,
                        )) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }

                    if ui.button("Clear (esc)").clicked() {
                        if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                            chosen_file.to_string(),
                            "".to_string(),
                            tab_id,
                            None,
                        )) {
                            eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                        }
                    }
                });
            }
        }

        if let Some(focused_tab) = self.focused_tab {
            if focused_tab == tab.id {
                if self.ctx.input(|i| i.key_pressed(Key::Escape)) {
                    if let Err(e) = &self.sender.send(UiMessage::FilterSheet(
                        chosen_file.to_string(),
                        "".to_string(),
                        tab_id,
                        None,
                    )) {
                        eprintln!("Worker: Failed to send page data to UI thread: {:?}", e);
                    }
                }
            }
        }

        if let Some(sheet) = self.promised_data.get(chosen_file) {
            if sheet.is_empty() {
                let painter = self
                    .ctx
                    .layer_painter(LayerId::new(Order::Foreground, Id::new("file_drop_target")));

                let screen_rect = self.ctx.content_rect();
                painter.rect_filled(screen_rect, 0.0, Color32::from_black_alpha(192));
                painter.text(
                    screen_rect.center(),
                    Align2::CENTER_CENTER,
                    "Loading…",
                    TextStyle::Heading.resolve(&self.ctx.style()),
                    Color32::WHITE,
                );
            }
        }

        if let Some(columns) = tab.columns.get_mut(chosen_file) {
            display_headers(ui, columns.as_mut());
            ui.add_space(4.0);

            let filter = if !self.global_filter.is_empty() {
                self.global_filter
            } else if let Some(chosen_file) = self.filters.get_mut(&(chosen_file.clone(), tab_id)) {
                chosen_file
            } else {
                &"".to_string()
            };

            let sheet_data = match (
                self.promised_data.get(chosen_file),
                self.filtered_data.get(&(chosen_file.to_string(), tab_id)),
            ) {
                (Some(_), None) if !filter.is_empty() => &vec![],
                (Some(master), None) => master,
                (Some(_), Some(filtered)) => filtered,
                _ => &vec![],
            };

            // let len = sheet_data.len();

            // let first_row = sheet_data.get(0);
            // let num_columns = if let Some(first_row) = first_row {
            //     first_row.len()
            // } else {
            //     0
            // };

            // let col_len = columns.len();

            let len = if !self.filtered_df.is_empty() {
                self.filtered_df.height()
            } else {
                self.df.height()
            };

            let num_columns = if !self.filtered_df.is_empty() {
                self.filtered_df.width()
            } else {
                self.df.width()
            };

            let col_len = columns.len();

            let mut table = Table {
                data: sheet_data,
                num_columns,
                columns: columns.as_mut(),
                num_rows: len as u64,
                num_sticky_cols: if col_len > 0 { 1 } else { 0 },
                default_column: egui_table::Column::new(30.0)
                    .range(10.0..=500.0)
                    .resizable(true),
                auto_size_mode: egui_table::AutoSizeMode::default(),
                top_row_height: 24.0,
                row_height: 18.0,
                is_row_expanded: Default::default(),
                prefetched: vec![],
                sender: self.sender,
                tab_id: tab_id,
                filename: chosen_file.clone(),
                filter: &filter,
                df: self.df,
                filtered_df: &self.filtered_df,
                view: DataFrame::empty(),
                start: 0,
            };

            table.ui(ui);
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if self.tabs_no > 1 {
            OnCloseResponse::Close
        } else {
            OnCloseResponse::Ignore
        }
    }

    // fn on_add(&mut self, surface: SurfaceIndex, node: NodeIndex) {
    //     self.added_nodes.push((surface, node));
    // }
}
