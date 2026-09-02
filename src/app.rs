use egui::Key;
use egui_dock::{DockArea, Style};
use std::path::PathBuf;
use std::thread;

use crate::data::{edit_record, filter_data, sort_data, write_csv};
use crate::menu::OPEN_FILE_ID;
use crate::read_csv::open_csv_file;
use crate::types::{
    CsvTabViewer, MyApp, SendUiMessage, SheetTab, SortOrder, TabId, UiMessage, UndoEntry, active_sheet_data,
};
use crate::ui::drop::preview_files_being_dropped;

#[cfg(target_os = "macos")]
use muda::MenuEvent;

/// Outcome of a successful `apply_undo`/`apply_redo`, enough to describe the
/// change in a toast: which row/column, what it had, what it's now.
struct UndoRedoResult {
    row: usize,
    column: Option<String>,
    overwritten: String,
    restored: String,
}

/// Truncate a cell value for display in a toast, so a huge field doesn't blow it up.
fn truncate_for_toast(value: &str) -> String {
    const MAX: usize = 24;
    if value.is_empty() {
        "(empty)".to_string()
    } else if value.chars().count() > MAX {
        format!("{}…", value.chars().take(MAX).collect::<String>())
    } else {
        value.to_string()
    }
}

/// Split an undo/redo outcome into a toast (title, body): title names the
/// action and column, body shows the value change.
fn undo_redo_toast(action: &str, result: &UndoRedoResult) -> (String, String) {
    let title = match &result.column {
        Some(name) => format!("{action} row {} · {name}", result.row),
        None => format!("{action} row {}", result.row),
    };
    let body = format!(
        "\"{}\" → \"{}\"",
        truncate_for_toast(&result.overwritten),
        truncate_for_toast(&result.restored)
    );
    (title, body)
}

/// Bump the request epoch for a (filename, tab_id) key and return the new value.
fn bump_epoch(epochs: &mut std::collections::HashMap<(String, usize), u64>, key: (String, usize)) -> u64 {
    let entry = epochs.entry(key).or_insert(0);
    *entry += 1;
    *entry
}

impl MyApp {
    /// Read the current value of a master cell, addressed by stable row index.
    fn read_cell(&self, filename: &str, master_row: usize, col: usize) -> Option<String> {
        self.sheets_data
            .get(filename)
            .and_then(|sheet| sheet.iter().find(|(idx, _)| *idx == master_row))
            .and_then(|(_, record)| record.get(col))
            .map(|s| s.to_string())
    }

    /// Look up a column's header name for a file from any tab that has it loaded.
    fn column_name(&self, filename: &str, col: usize) -> Option<String> {
        self.tree
            .iter_all_tabs()
            .find_map(|(_, tab)| tab.columns.get(filename))
            .and_then(|headers| headers.get(col))
            .map(|h| {
                if h.name.is_empty() && col == 0 {
                    "id".to_string()
                } else {
                    h.name.clone()
                }
            })
    }

    /// Write `value` into a master cell (by stable row index) and every
    /// filtered/sorted view of that file, across all tabs.
    fn write_cell(&mut self, filename: &str, master_row: usize, col: usize, value: &str) {
        if let Some(sheet) = self.sheets_data.get_mut(filename) {
            edit_record(sheet, master_row, col, value);
        }
        for ((fname, _), display_sheet) in self.filtered_data.iter_mut() {
            if fname == filename {
                edit_record(display_sheet, master_row, col, value);
            }
        }
    }

    /// Pop `undo_stack`, write its old value back, and push the value it
    /// overwrote onto `redo_stack`. Returns `None` if the stack was empty or
    /// the target cell no longer exists.
    fn apply_undo(&mut self) -> Option<UndoRedoResult> {
        let entry = self.undo_stack.pop()?;
        let current = self.read_cell(&entry.filename, entry.master_row, entry.col)?;
        self.write_cell(&entry.filename, entry.master_row, entry.col, &entry.old_value);
        self.dirty_files.insert(entry.filename.clone());
        let column = self.column_name(&entry.filename, entry.col);
        let row = entry.master_row;
        let restored = entry.old_value.clone();
        self.redo_stack.push(UndoEntry {
            filename: entry.filename,
            master_row: entry.master_row,
            col: entry.col,
            old_value: current.clone(),
        });
        Some(UndoRedoResult { row, column, restored, overwritten: current })
    }

    /// Mirror of `apply_undo` between `redo_stack` and `undo_stack`.
    fn apply_redo(&mut self) -> Option<UndoRedoResult> {
        let entry = self.redo_stack.pop()?;
        let current = self.read_cell(&entry.filename, entry.master_row, entry.col)?;
        self.write_cell(&entry.filename, entry.master_row, entry.col, &entry.old_value);
        self.dirty_files.insert(entry.filename.clone());
        let column = self.column_name(&entry.filename, entry.col);
        let row = entry.master_row;
        let restored = entry.old_value.clone();
        self.undo_stack.push(UndoEntry {
            filename: entry.filename,
            master_row: entry.master_row,
            col: entry.col,
            old_value: current.clone(),
        });
        Some(UndoRedoResult { row, column, restored, overwritten: current })
    }

    /// Write a pasted grid into master starting at (anchor_row, anchor_col) in
    /// the currently displayed view for (filename, tab_id), growing right/down.
    /// Rows/cols beyond the sheet's current bounds are skipped -- no row or
    /// column insertion happens here. Pushes one UndoEntry per cell written
    /// and clears redo_stack if anything was written. Returns the count of
    /// cells actually written.
    fn apply_paste(
        &mut self,
        filename: &str,
        tab_id: TabId,
        anchor_row: u64,
        anchor_col: usize,
        rows: &[Vec<String>],
    ) -> usize {
        let key = (filename.to_string(), tab_id);
        let num_master_rows = self.sheets_data.get(filename).map_or(0, |s| s.len());
        let num_cols = self
            .tree
            .iter_all_tabs()
            .find_map(|(_, tab)| tab.columns.get(filename))
            .map_or(0, |headers| headers.len());

        let mut pasted_count = 0usize;

        for (row_offset, row_values) in rows.iter().enumerate() {
            let display_row = anchor_row + row_offset as u64;

            // Resolve the displayed row to a stable master row index, same
            // fallback EditCell uses.
            let master_row = self
                .filtered_data
                .get(&key)
                .and_then(|sheet| sheet.get(display_row as usize))
                .map(|(idx, _)| *idx)
                .or(Some(display_row as usize));

            let Some(master_row) = master_row else {
                break;
            };
            if master_row >= num_master_rows {
                break;
            }

            for (col_offset, value) in row_values.iter().enumerate() {
                let col = anchor_col + col_offset;
                if col >= num_cols {
                    break;
                }

                if let Some(old_value) = self.read_cell(filename, master_row, col) {
                    self.undo_stack.push(UndoEntry {
                        filename: filename.to_string(),
                        master_row,
                        col,
                        old_value,
                    });
                    self.write_cell(filename, master_row, col, value);
                    pasted_count += 1;
                }
            }
        }

        if pasted_count > 0 {
            self.redo_stack.clear();
            self.dirty_files.insert(filename.to_string());
        }

        pasted_count
    }
}

impl MyApp {
    pub fn load_file(&mut self, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
        self.picked_path = Some(file_name.clone());

        self.files_list.push(file_name.clone());

        let (mut reader, headers) = open_csv_file(&file_name);

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;
            sheet_tab.columns.insert(file_name.clone(), headers.clone());

            if let Some(tab_id) = tab_id {
                if sheet_tab.id == tab_id {
                    sheet_tab.chosen_file = file_name.clone();
                    self.filters
                        .insert((file_name.clone(), tab_id), "".to_string());
                }
            }
        }

        self.loading = true;

        ctx.send_viewport_cmd(egui::ViewportCommand::Title(file_name.clone()));

        let chan = self.worker_chan.0.clone();
        let ctx = ctx.clone();

        thread::spawn(move || {
            let master_data = reader
                .records()
                .filter_map(|record| record.ok())
                .enumerate()
                .collect::<Vec<_>>();

            chan.send_msg(UiMessage::SetMaster(master_data, file_name.clone()));

            ctx.request_repaint();
        });
    }

    fn sort_current_sheet(
        &mut self,
        ctx: &egui::Context,
        filename: String,
        sort_order: (usize, SortOrder),
        tab_id: usize,
    ) {
        let chan = self.worker_chan.0.clone();

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;

            if sheet_tab.id == tab_id {
                let filter_active = self
                    .filters
                    .get(&(filename.to_string(), tab_id))
                    .map_or(false, |f| !f.is_empty());

                let sheet_data = active_sheet_data(
                    &self.sheets_data,
                    &self.filtered_data,
                    &filename,
                    tab_id,
                    filter_active,
                );

                if !sheet_data.is_empty() {
                    let master_clone = sheet_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();
                    let epoch = bump_epoch(&mut self.request_epoch, (filename.clone(), tab_id));

                    thread::spawn(move || {
                        let sorted = sort_data(master_clone, sort_order);

                        chan.send_msg(UiMessage::SetDisplayData(sorted, filename, tab_id, epoch));

                        ctx.request_repaint();
                    });
                }
            };
        }
    }

    fn filter_current_sheet(
        &mut self,
        ctx: &egui::Context,
        filename: String,
        filter: String,
        tab_id: usize,
    ) {
        let chan = self.worker_chan.0.clone();

        for tab in self.tree.iter_all_tabs_mut() {
            let sheet_tab = tab.1;
            let filter = filter.clone();

            if sheet_tab.id == tab_id {
                if let Some(master_data) = self.sheets_data.get(&filename) {
                    let master_clone = master_data.clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();
                    let epoch = bump_epoch(&mut self.request_epoch, (filename.clone(), tab_id));

                    thread::spawn(move || {
                        let filtered = filter_data(master_clone, filter);

                        chan.send_msg(UiMessage::SetDisplayData(filtered, filename, tab_id, epoch));

                        ctx.request_repaint();
                    });
                }
            };
        }
    }
}

impl MyApp {
    fn subsecond_fn(&mut self, ctx: &egui::Context) {
        subsecond::call(|| {
            self.update_inner(ctx);
        });
    }

    fn update_inner(&mut self, ctx: &egui::Context) {
        // Handle macOS menu events
        #[cfg(target_os = "macos")]
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id.as_ref() == OPEN_FILE_ID {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .pick_file()
                {
                    if let Some(path_str) = path.to_str() {
                        self.worker_chan
                            .0
                            .send_msg(UiMessage::OpenFile(path_str.to_string(), None));
                    }
                }
            }
        }

        while let Ok(ui_ping) = self.ui_chan.1.try_recv() {
            if ui_ping {
                eprintln!("requested ui ping");

                ctx.request_repaint();
            }
        }

        while let Ok(message) = self.worker_chan.1.try_recv() {
            match message {
                UiMessage::SetMaster(master, file_name) => {
                    self.sheets_data.insert(file_name, master);
                }
                UiMessage::SetDisplayData(sorted, file_name, tab_id, epoch) => {
                    let key = (file_name, tab_id);
                    let current_epoch = self.request_epoch.get(&key).copied().unwrap_or(0);
                    if epoch == current_epoch {
                        self.filtered_data.insert(key, sorted);
                    }
                }
                UiMessage::FilterGlobal(filter) => {
                    self.global_filter = filter;
                }
                UiMessage::FilterSheet(filename, filter, tab_id, _column) => {
                    self.filters
                        .insert((filename.clone(), tab_id), filter.clone());
                    self.filter_current_sheet(ctx, filename, filter, tab_id);
                }
                UiMessage::SortSheet(filename, sort_order, tab_id) => {
                    self.sort_current_sheet(ctx, filename, sort_order, tab_id);
                }
                UiMessage::OpenFile(file, tab) => self.load_file(ctx, file, tab),
                UiMessage::EditCell(filename, tab_id, row_nr, actual_col, new_value) => {
                    // row_nr is an index into the currently displayed (filtered/sorted)
                    // view. Resolve it to a stable master row index first, then always
                    // write through to master -- the filtered view is just a cache.
                    let key = (filename.clone(), tab_id);
                    let master_row = self
                        .filtered_data
                        .get(&key)
                        .and_then(|sheet| sheet.get(row_nr as usize))
                        .map(|(idx, _)| *idx)
                        .or(Some(row_nr as usize));

                    if let Some(master_row) = master_row {
                        if let Some(old_value) = self.read_cell(&filename, master_row, actual_col) {
                            self.undo_stack.push(UndoEntry {
                                filename: filename.clone(),
                                master_row,
                                col: actual_col,
                                old_value,
                            });
                            self.redo_stack.clear();
                        }

                        self.write_cell(&filename, master_row, actual_col, &new_value);
                    }

                    self.dirty_files.insert(filename);
                }
                UiMessage::PasteCells(filename, tab_id, anchor_row, anchor_col, rows) => {
                    let pasted_count = self.apply_paste(&filename, tab_id, anchor_row, anchor_col, &rows);
                    if pasted_count > 0 {
                        let plural = if pasted_count == 1 { "" } else { "s" };
                        crate::toast::show(ctx, format!("Pasted {pasted_count} cell{plural}"));
                    }
                }
                UiMessage::Undo => {
                    if let Some(result) = self.apply_undo() {
                        let (title, body) = undo_redo_toast("Undo", &result);
                        crate::toast::show_titled(ctx, title, body);
                    }
                }
                UiMessage::Redo => {
                    if let Some(result) = self.apply_redo() {
                        let (title, body) = undo_redo_toast("Redo", &result);
                        crate::toast::show_titled(ctx, title, body);
                    }
                }
                UiMessage::SaveFile(filename) => {
                    if let Some(data) = self.sheets_data.get(&filename) {
                        let headers = self
                            .tree
                            .iter_all_tabs()
                            .find_map(|(_, tab)| tab.columns.get(&filename))
                            .cloned()
                            .unwrap_or_default();
                        if let Err(e) = write_csv(&filename, &headers, data) {
                            eprintln!("Failed to save {}: {:?}", filename, e);
                        } else {
                            self.dirty_files.remove(&filename);
                            let short_name =
                                filename.split('/').last().unwrap_or(&filename).to_string();
                            crate::toast::show(ctx, format!("Saved: {short_name}"));
                        }
                    }
                }
            }
        }

        let mut added_nodes = Vec::new();

        let tabs_no = self.tree.iter_all_tabs().count();
        let focused_tab = self.tree.find_active_focused().map(|(_, tab)| tab.id);

        let save_file = ctx
            .input(|i| i.modifiers.command && i.key_pressed(Key::S))
            .then(|| {
                self.tree.find_active_focused().and_then(|(_, tab)| {
                    let f = tab.chosen_file.clone();
                    if f.is_empty() { None } else { Some(f) }
                })
            })
            .flatten();

        if let Some(filename) = save_file {
            self.worker_chan.0.send_msg(UiMessage::SaveFile(filename));
        }

        // Ignore undo/redo shortcuts while a cell is actively being edited so
        // they don't clobber an in-progress edit buffer.
        let any_cell_editing = self
            .tree
            .iter_all_tabs()
            .any(|(_, tab)| tab.editing_cell.is_some());

        if !any_cell_editing {
            let (undo, redo) = ctx.input(|i| {
                let cmd_z = i.modifiers.command && i.key_pressed(Key::Z);
                (cmd_z && !i.modifiers.shift, cmd_z && i.modifiers.shift)
            });

            if undo {
                self.worker_chan.0.send_msg(UiMessage::Undo);
            } else if redo {
                self.worker_chan.0.send_msg(UiMessage::Redo);
            }
        }

        crate::toast::render(ctx);

        egui::TopBottomPanel::top("top_panel").show(ctx, |_ui| {
            ctx.input(|input| {
                if input.key_pressed(Key::X) {
                    self.worker_chan
                        .0
                        .send_msg(UiMessage::FilterGlobal("".to_string()));
                }
            });
        });

        DockArea::new(&mut self.tree)
            .style(Style::from_egui(ctx.style().as_ref()))
            .show_add_buttons(true)
            .show_add_popup(true)
            .show(
                ctx,
                &mut CsvTabViewer {
                    added_nodes: &mut added_nodes,
                    promised_data: &self.sheets_data,
                    filtered_data: &self.filtered_data,
                    ctx: &ctx,
                    sender: &self.worker_chan.0,
                    files_list: &self.files_list,
                    tabs_no,
                    focused_tab,
                    global_filter: &self.global_filter,
                    filters: &mut self.filters,
                    dirty_files: &self.dirty_files,
                },
            );

        added_nodes.drain(..).for_each(|(surface, node, filename)| {
            self.tree.set_focused_node_and_surface((surface, node));

            let columns = self
                .tree
                .iter_all_tabs()
                .last()
                .map(|(_, tab)| tab.columns.clone())
                .unwrap_or_default();

            self.tree.push_to_focused_leaf(SheetTab {
                id: self.counter,
                columns,
                chosen_file: filename,
                ..Default::default()
            });

            self.counter += 1;
        });

        preview_files_being_dropped(ctx);

        ctx.input(|i| {
            if !i.raw.dropped_files.is_empty() {
                let files = &i.raw.dropped_files;

                let default_path = PathBuf::default();

                for file in files {
                    let path = file
                        .path
                        .as_ref()
                        .unwrap_or(&default_path)
                        .to_str()
                        .unwrap_or("");

                    self.worker_chan
                        .0
                        .send_msg(UiMessage::OpenFile(path.to_string(), None));
                }
            }
        });
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        subsecond::call(|| {
            self.subsecond_fn(ctx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UiMessage;
    use csv::StringRecord;
    use egui_dock::DockState;
    use std::collections::{HashMap, HashSet};
    use std::sync::mpsc;

    fn test_app() -> MyApp {
        MyApp {
            picked_path: None,
            loading: false,
            worker_chan: mpsc::channel::<UiMessage>(),
            ui_chan: mpsc::channel::<crate::types::Ping>(),
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
            request_epoch: HashMap::new(),
            undo_stack: vec![],
            redo_stack: vec![],
        }
    }

    fn row(idx: usize, fields: &[&str]) -> (usize, StringRecord) {
        (idx, StringRecord::from(fields.to_vec()))
    }

    fn set_columns(app: &mut MyApp, filename: &str, count: usize) {
        let headers = (0..count)
            .map(|i| crate::types::FileHeader {
                name: format!("col{i}"),
                visible: true,
                sort: None,
            })
            .collect();
        for tab in app.tree.iter_all_tabs_mut() {
            tab.1.columns.insert(filename.to_string(), headers);
            return;
        }
    }

    #[test]
    fn undo_restores_previous_value_and_pushes_redo() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"])],
        );
        app.undo_stack.push(UndoEntry {
            filename: "f.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "original".to_string(),
        });

        let result = app.apply_undo().expect("undo should apply");
        assert_eq!(result.restored, "original");
        assert_eq!(result.overwritten, "a");
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("original".to_string()));
        assert_eq!(app.redo_stack.len(), 1);
        assert_eq!(app.redo_stack[0].old_value, "a");
        assert!(app.dirty_files.contains("f.csv"));
    }

    #[test]
    fn undo_then_redo_round_trips() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["current"])]);
        app.undo_stack.push(UndoEntry {
            filename: "f.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "before".to_string(),
        });

        app.apply_undo();
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("before".to_string()));

        app.apply_redo();
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("current".to_string()));
        // redo is itself undoable, so it lands back on undo_stack.
        assert_eq!(app.undo_stack.len(), 1);
        assert!(app.redo_stack.is_empty());
    }

    #[test]
    fn multiple_undos_restore_in_reverse_order() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["v3"])]);
        // Simulate two edits: v1 -> v2 -> v3, each push recording the prior value.
        app.undo_stack.push(UndoEntry {
            filename: "f.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "v1".to_string(),
        });
        app.undo_stack.push(UndoEntry {
            filename: "f.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "v2".to_string(),
        });

        assert!(app.apply_undo().is_some());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("v2".to_string()));
        assert!(app.apply_undo().is_some());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("v1".to_string()));
    }

    #[test]
    fn undo_on_empty_stack_is_noop() {
        let mut app = test_app();
        assert!(app.apply_undo().is_none());
        assert!(app.dirty_files.is_empty());
    }

    #[test]
    fn redo_on_empty_stack_is_noop() {
        let mut app = test_app();
        assert!(app.apply_redo().is_none());
        assert!(app.dirty_files.is_empty());
    }

    #[test]
    fn undo_missing_target_cell_is_noop_and_drops_entry() {
        let mut app = test_app();
        // File was closed/removed since the edit was made.
        app.undo_stack.push(UndoEntry {
            filename: "gone.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "x".to_string(),
        });

        assert!(app.apply_undo().is_none());
        assert!(app.undo_stack.is_empty());
        assert!(app.redo_stack.is_empty());
    }

    #[test]
    fn new_edit_clears_redo_stack() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["b"])]);
        app.redo_stack.push(UndoEntry {
            filename: "f.csv".to_string(),
            master_row: 0,
            col: 0,
            old_value: "stale".to_string(),
        });

        if let Some(old_value) = app.read_cell("f.csv", 0, 0) {
            app.undo_stack.push(UndoEntry {
                filename: "f.csv".to_string(),
                master_row: 0,
                col: 0,
                old_value,
            });
            app.redo_stack.clear();
        }
        app.write_cell("f.csv", 0, 0, "c");

        assert!(app.redo_stack.is_empty());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("c".to_string()));
    }

    #[test]
    fn paste_writes_grid_starting_at_anchor() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b"]), row(1, &["c", "d"])],
        );
        set_columns(&mut app, "f.csv", 2);

        let rows = vec![
            vec!["x".to_string(), "y".to_string()],
            vec!["z".to_string(), "w".to_string()],
        ];
        let count = app.apply_paste("f.csv", 1, 0, 0, &rows);

        assert_eq!(count, 4);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("x".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("y".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("z".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("w".to_string()));
        assert!(app.dirty_files.contains("f.csv"));
        assert_eq!(app.undo_stack.len(), 4);
    }

    #[test]
    fn paste_skips_rows_beyond_sheet_bounds() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);

        // Anchor at row 0 with 3 pasted rows, but only 1 row exists.
        let rows = vec![
            vec!["x".to_string()],
            vec!["y".to_string()],
            vec!["z".to_string()],
        ];
        let count = app.apply_paste("f.csv", 1, 0, 0, &rows);

        assert_eq!(count, 1);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("x".to_string()));
    }

    #[test]
    fn paste_skips_columns_beyond_header_bounds() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a", "b"])]);
        set_columns(&mut app, "f.csv", 2);

        // Pasted row has 3 values but only 2 columns exist.
        let rows = vec![vec!["x".to_string(), "y".to_string(), "z".to_string()]];
        let count = app.apply_paste("f.csv", 1, 0, 0, &rows);

        assert_eq!(count, 2);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("x".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("y".to_string()));
    }

    #[test]
    fn paste_at_nonzero_anchor_offsets_correctly() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b", "c"]), row(1, &["d", "e", "f"])],
        );
        set_columns(&mut app, "f.csv", 3);

        let rows = vec![vec!["x".to_string()]];
        let count = app.apply_paste("f.csv", 1, 1, 2, &rows);

        assert_eq!(count, 1);
        assert_eq!(app.read_cell("f.csv", 1, 2), Some("x".to_string()));
        // Untouched cells stay as they were.
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
    }

    #[test]
    fn paste_empty_rows_is_noop() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);

        let count = app.apply_paste("f.csv", 1, 0, 0, &[]);

        assert_eq!(count, 0);
        assert!(app.dirty_files.is_empty());
    }
}
