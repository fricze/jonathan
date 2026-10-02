use csv::StringRecord;
use egui::Key;
use egui_dock::{DockArea, Style};
use std::path::PathBuf;
use std::thread;

use csv_model::{edit_record, filter_indices, open_csv_file, sort_indices, write_csv};
use crate::menu::OPEN_FILE_ID;
use crate::types::{
    ColumnId, CsvTabViewer, FileHeader, InsertPosition, MyApp, ReplaceScope, SendUiMessage, SheetTab,
    SortOrder, TabId, UiMessage, UndoEntry, View, SheetRow, active_sheet_data, column_position,
};
use crate::ui::drop::preview_files_being_dropped;

#[cfg(target_os = "macos")]
use muda::MenuEvent;

/// Outcome of a successful `apply_undo`/`apply_redo`, enough to describe the
/// change in a toast: which row/column, what it had, what it's now. `row` is
/// None for a column-level change (no single row involved).
struct UndoRedoResult {
    row: Option<usize>,
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
    let title = match (result.row, &result.column) {
        (Some(row), Some(name)) => format!("{action} row {row} · {name}"),
        (Some(row), None) => format!("{action} row {row}"),
        (None, Some(name)) => format!("{action} column {name}"),
        (None, None) => action.to_string(),
    };
    // A batch result has no single before/after value -- `restored` is
    // already a plain summary (e.g. "3 cells") and `overwritten` is empty.
    let body = if result.overwritten.is_empty() {
        result.restored.clone()
    } else {
        format!(
            "\"{}\" → \"{}\"",
            truncate_for_toast(&result.overwritten),
            truncate_for_toast(&result.restored)
        )
    };
    (title, body)
}

/// Bump the request epoch for a (filename, tab_id) key and return the new value.
fn bump_epoch(epochs: &mut std::collections::HashMap<(String, usize), u64>, key: (String, usize)) -> u64 {
    let entry = epochs.entry(key).or_insert(0);
    *entry += 1;
    *entry
}

impl MyApp {
    /// Read the current value of a master cell, addressed by stable row index
    /// and column_id (resolved to a live position internally, so it stays
    /// correct even if a column insert/delete happened since col_id was
    /// recorded).
    fn read_cell(&self, filename: &str, master_row: usize, col_id: ColumnId) -> Option<String> {
        let position = self.column_position(filename, col_id)?;
        self.sheets_data
            .get(filename)
            .and_then(|sheet| sheet.iter().find(|(idx, _)| *idx == master_row))
            .and_then(|(_, record)| record.get(position))
            .map(|s| s.to_string())
    }

    /// Resolve a column_id to its current display position in `filename`'s
    /// headers, from any tab that has the file loaded. `None` if the column
    /// no longer exists (e.g. deleted since an UndoEntry referencing it was
    /// recorded).
    fn column_position(&self, filename: &str, col_id: ColumnId) -> Option<usize> {
        let headers = self.tree.iter_all_tabs().find_map(|(_, tab)| tab.columns.get(filename))?;
        column_position(headers, col_id)
    }

    /// Look up a column's header name for a file from any tab that has it
    /// loaded, by stable column_id.
    fn column_name(&self, filename: &str, col_id: ColumnId) -> Option<String> {
        let headers = self.tree.iter_all_tabs().find_map(|(_, tab)| tab.columns.get(filename))?;
        let position = column_position(headers, col_id)?;
        headers.get(position).map(|h| {
            if h.name.is_empty() && position == 0 {
                "id".to_string()
            } else {
                h.name.clone()
            }
        })
    }

    /// Record a new edit in `filename`'s history (clears its redo branch,
    /// refreshes dirty).
    fn push_undo(&mut self, filename: &str, entry: UndoEntry) {
        self.history.push(&filename.to_string(), entry);
    }

    /// Write `value` into a master cell (by stable row index and column_id,
    /// resolved to a live position internally) and every filtered/sorted
    /// view of that file, across all tabs. No-op if the column no longer
    /// exists.
    fn write_cell(&mut self, filename: &str, master_row: usize, col_id: ColumnId, value: &str) {
        let Some(position) = self.column_position(filename, col_id) else {
            return;
        };
        if let Some(sheet) = self.sheets_data.get_mut(filename) {
            edit_record(sheet, master_row, position, value);
        }
    }

    /// Master row id shown at display row `row_nr` of (filename, tab_id),
    /// resolving through that tab's filtered/sorted view if it has one.
    fn master_row_at(&self, filename: &str, tab_id: TabId, row_nr: u64) -> Option<usize> {
        let master = self.sheets_data.get(filename)?;
        let position = match self.filtered_data.get(&(filename.to_string(), tab_id)) {
            Some(view) => *view.get(row_nr as usize)? as usize,
            None => row_nr as usize,
        };
        master.get(position).map(|(id, _)| *id)
    }

    /// Drop in-flight sort/filter results for `filename`: they were computed
    /// against master positions that a row insert/remove just shifted.
    fn invalidate_pending_views(&mut self, filename: &str) {
        for ((fname, _), epoch) in self.request_epoch.iter_mut() {
            if fname == filename {
                *epoch += 1;
            }
        }
    }

    /// Remove master row `position`, shifting every view of `filename` to
    /// match (views hold master positions).
    fn remove_master_row(&mut self, filename: &str, position: usize) -> Option<SheetRow> {
        let row = {
            let sheet = self.sheets_data.get_mut(filename)?;
            if position >= sheet.len() {
                return None;
            }
            sheet.remove(position)
        };
        self.invalidate_pending_views(filename);
        let p = position as u32;
        for ((fname, _), view) in self.filtered_data.iter_mut() {
            if fname == filename {
                view.retain(|&x| x != p);
                view.iter_mut().filter(|x| **x > p).for_each(|x| *x -= 1);
            }
        }
        Some(row)
    }

    /// Insert `row` at master `position` (clamped), shifting every view of
    /// `filename` to match. The new row is in no view until it is refreshed.
    fn insert_master_row(&mut self, filename: &str, position: usize, row: SheetRow) -> Option<()> {
        let sheet = self.sheets_data.get_mut(filename)?;
        let position = position.min(sheet.len());
        sheet.insert(position, row);
        self.invalidate_pending_views(filename);
        let p = position as u32;
        for ((fname, _), view) in self.filtered_data.iter_mut() {
            if fname == filename {
                view.iter_mut().filter(|x| **x >= p).for_each(|x| *x += 1);
            }
        }
        Some(())
    }

    /// Pop `filename`'s newest undo entry, apply its reverse, and record the
    /// reversal as a redo entry. `None` if the stack was empty or the
    /// entry's target no longer exists. Never touches another file's history.
    fn apply_undo(&mut self, filename: &str) -> Option<UndoRedoResult> {
        let key = filename.to_string();
        let entry = self.history.pop_undo(&key)?;
        let (redo_entry, result) = self.reverse_entry(entry)?;
        self.history.push_redo_entry(&key, redo_entry);
        Some(result)
    }

    /// Mirror of `apply_undo` between redo and undo.
    fn apply_redo(&mut self, filename: &str) -> Option<UndoRedoResult> {
        let key = filename.to_string();
        let entry = self.history.pop_redo(&key)?;
        let (undo_entry, result) = self.reverse_entry(entry)?;
        self.history.push_undo_entry(&key, undo_entry);
        Some(result)
    }

    /// Apply the reverse of `entry` and return (an entry that reverses that
    /// reversal, a description for the toast). Shared by apply_undo/apply_redo
    /// since reversing a reversal is symmetric for every UndoEntry variant.
    fn reverse_entry(&mut self, entry: UndoEntry) -> Option<(UndoEntry, UndoRedoResult)> {
        match entry {
            UndoEntry::CellEdit { filename, master_row, col_id, old_value } => {
                let current = self.read_cell(&filename, master_row, col_id)?;
                self.write_cell(&filename, master_row, col_id, &old_value);
                let column = self.column_name(&filename, col_id);
                let reverse = UndoEntry::CellEdit {
                    filename,
                    master_row,
                    col_id,
                    old_value: current.clone(),
                };
                let result = UndoRedoResult {
                    row: Some(master_row),
                    column,
                    restored: old_value,
                    overwritten: current,
                };
                Some((reverse, result))
            }
            UndoEntry::RowDelete { filename, master_row, position, record } => {
                self.insert_master_row(&filename, position, record)?;
                let reverse = UndoEntry::RowInsert { filename, master_row };
                let result = UndoRedoResult {
                    row: Some(master_row),
                    column: None,
                    restored: "(row restored)".to_string(),
                    overwritten: "(deleted)".to_string(),
                };
                Some((reverse, result))
            }
            UndoEntry::RowInsert { filename, master_row } => {
                let position = self.sheets_data.get(&filename)?.iter().position(|(idx, _)| *idx == master_row)?;
                let record = self.remove_master_row(&filename, position)?;
                let reverse = UndoEntry::RowDelete { filename, master_row, position, record };
                let result = UndoRedoResult {
                    row: Some(master_row),
                    column: None,
                    restored: "(deleted)".to_string(),
                    overwritten: "(row removed)".to_string(),
                };
                Some((reverse, result))
            }
            UndoEntry::ColumnDelete { filename, position, header, values_by_row } => {
                let column_id = header.id;
                let name = header.name.clone();
                self.insert_column_at(&filename, position, header, &values_by_row);
                let reverse = UndoEntry::ColumnInsert { filename, column_id };
                let result = UndoRedoResult {
                    row: None,
                    column: Some(name),
                    restored: "(column restored)".to_string(),
                    overwritten: "(deleted)".to_string(),
                };
                Some((reverse, result))
            }
            UndoEntry::ColumnInsert { filename, column_id } => {
                let headers = self.tree.iter_all_tabs().find_map(|(_, tab)| tab.columns.get(&filename).cloned())?;
                let position = column_position(&headers, column_id)?;
                let header = headers[position].clone();
                let name = header.name.clone();

                let values_by_row = self.remove_column_at(&filename, position, column_id);
                let reverse = UndoEntry::ColumnDelete { filename, position, header, values_by_row };
                let result = UndoRedoResult {
                    row: None,
                    column: Some(name),
                    restored: "(deleted)".to_string(),
                    overwritten: "(column removed)".to_string(),
                };
                Some((reverse, result))
            }
            UndoEntry::Batch(entries) => {
                let count = entries.len();
                // Reverse in reverse order: if entry B in the batch somehow
                // depended on entry A already being applied (not currently
                // possible for CellEdit-only batches, but keeps this correct
                // if a future batch mixes entry types), unwinding must undo
                // B before A.
                let mut reversed = Vec::with_capacity(count);
                for entry in entries.into_iter().rev() {
                    let (reverse, _) = self.reverse_entry(entry)?;
                    reversed.push(reverse);
                }
                let reverse = UndoEntry::Batch(reversed);
                let result = UndoRedoResult {
                    row: None,
                    column: None,
                    restored: format!("{count} cell{}", if count == 1 { "" } else { "s" }),
                    overwritten: "".to_string(),
                };
                Some((reverse, result))
            }
        }
    }

    /// Write a pasted grid into master starting at (anchor_row, anchor_col_id)
    /// in the currently displayed view for (filename, tab_id), growing
    /// right/down through display *positions* from the anchor's current
    /// position (each subsequent column resolved to its own column_id before
    /// writing, so a paste spanning multiple columns still lands correctly
    /// even though only the anchor is identified). Rows/cols beyond the
    /// sheet's current bounds are skipped -- no row or column insertion
    /// happens here. Pushes one UndoEntry::Batch covering every cell written,
    /// so the whole paste undoes/redoes as a single action, and clears
    /// redo_stack if anything was written. Returns the count of cells
    /// actually written.
    fn apply_paste(
        &mut self,
        filename: &str,
        tab_id: TabId,
        anchor_row: u64,
        anchor_col_id: ColumnId,
        rows: &[Vec<String>],
    ) -> usize {
        let headers = self.tree.iter_all_tabs().find_map(|(_, tab)| tab.columns.get(filename).cloned());
        let Some(headers) = headers else {
            return 0;
        };
        let Some(anchor_position) = column_position(&headers, anchor_col_id) else {
            return 0;
        };

        let mut batch: Vec<UndoEntry> = Vec::new();

        for (row_offset, row_values) in rows.iter().enumerate() {
            let display_row = anchor_row + row_offset as u64;

            // Resolve the displayed row to a stable master row index, same
            // fallback EditCell uses.
            let Some(master_row) = self.master_row_at(filename, tab_id, display_row) else {
                break;
            };

            for (col_offset, value) in row_values.iter().enumerate() {
                let position = anchor_position + col_offset;
                let Some(header) = headers.get(position) else {
                    break;
                };
                let col_id = header.id;

                if let Some(old_value) = self.read_cell(filename, master_row, col_id) {
                    batch.push(UndoEntry::CellEdit {
                        filename: filename.to_string(),
                        master_row,
                        col_id,
                        old_value,
                    });
                    self.write_cell(filename, master_row, col_id, value);
                }
            }
        }

        let pasted_count = batch.len();
        if pasted_count > 0 {
            self.push_undo(filename, UndoEntry::Batch(batch));
        }

        pasted_count
    }

    /// Replace every occurrence of `find` with `replace` across the rows
    /// currently displayed for (filename, tab_id) -- the filtered/sorted view
    /// if one exists, otherwise all of master -- within `scope`. Empty `find`
    /// matches nothing (avoids replacing every cell boundary). Pushes one
    /// UndoEntry::Batch covering every changed cell, so the whole replace
    /// undoes/redoes as a single action. Returns the count of cells changed.
    fn apply_replace_all(
        &mut self,
        filename: &str,
        tab_id: TabId,
        find: &str,
        replace: &str,
        scope: ReplaceScope,
    ) -> usize {
        if find.is_empty() {
            return 0;
        }

        let master_rows: Vec<usize> = match (
            self.sheets_data.get(filename),
            self.filtered_data.get(&(filename.to_string(), tab_id)),
        ) {
            (Some(master), Some(view)) => view.iter().map(|&i| master[i as usize].0).collect(),
            (Some(master), None) => master.iter().map(|(idx, _)| *idx).collect(),
            _ => vec![],
        };

        let all_col_ids: Vec<ColumnId> = self
            .tree
            .iter_all_tabs()
            .find_map(|(_, tab)| tab.columns.get(filename))
            .map_or(vec![], |headers| headers.iter().map(|h| h.id).collect());

        let col_ids: Vec<ColumnId> = match scope {
            ReplaceScope::CurrentColumn(col_id) => vec![col_id],
            ReplaceScope::AllColumns => all_col_ids,
        };

        let mut batch: Vec<UndoEntry> = Vec::new();

        for master_row in master_rows {
            for &col_id in &col_ids {
                let Some(old_value) = self.read_cell(filename, master_row, col_id) else {
                    continue;
                };
                if !old_value.contains(find) {
                    continue;
                }
                let new_value = old_value.replace(find, replace);
                if new_value == old_value {
                    continue;
                }

                batch.push(UndoEntry::CellEdit {
                    filename: filename.to_string(),
                    master_row,
                    col_id,
                    old_value,
                });
                self.write_cell(filename, master_row, col_id, &new_value);
            }
        }

        let changed_count = batch.len();
        if changed_count > 0 {
            self.push_undo(filename, UndoEntry::Batch(batch));
        }

        changed_count
    }

    /// Delete the row at `row_nr` in the currently displayed view for
    /// (filename, tab_id). Resolves to a stable master_row the same way
    /// EditCell does, then removes it from master and every filtered/sorted
    /// view of that file. Pushes an UndoEntry::RowDelete capturing enough to
    /// restore it. Returns `true` if a row was actually removed.
    fn apply_delete_row(&mut self, filename: &str, tab_id: TabId, row_nr: u64) -> bool {
        let Some(master_row) = self.master_row_at(filename, tab_id, row_nr) else {
            return false;
        };
        let Some(position) = self
            .sheets_data
            .get(filename)
            .and_then(|sheet| sheet.iter().position(|(idx, _)| *idx == master_row))
        else {
            return false;
        };
        let Some(record) = self.remove_master_row(filename, position) else {
            return false;
        };

        self.push_undo(
            filename,
            UndoEntry::RowDelete { filename: filename.to_string(), master_row, position, record },
        );
        true
    }

    /// Insert a new empty row relative to `anchor` in the currently displayed
    /// view for (filename, tab_id): Some((row_nr, Before|After)) inserts
    /// immediately before/after that displayed row, None appends at the end
    /// of master. The new row gets a fresh master_row id from next_row_id
    /// and is only added to master -- filtered/sorted views naturally
    /// exclude it until refreshed, same as any other master change. Pushes
    /// an UndoEntry::RowInsert. Returns the new row's master_row id, or None
    /// if the file isn't loaded.
    fn apply_insert_row(
        &mut self,
        filename: &str,
        tab_id: TabId,
        anchor: Option<(u64, InsertPosition)>,
    ) -> Option<usize> {
        let num_cols = self
            .tree
            .iter_all_tabs()
            .find_map(|(_, tab)| tab.columns.get(filename))
            .map_or(0, |headers| headers.len());

        let master_row = *self.next_row_id.get(filename).unwrap_or(&0);
        self.next_row_id.insert(filename.to_string(), master_row + 1);

        let empty_record: StringRecord = std::iter::repeat("").take(num_cols).collect();

        let len = self.sheets_data.get(filename)?.len();
        let position = match anchor {
            Some((row_nr, insert_position)) => {
                let anchor_position = match self.filtered_data.get(&(filename.to_string(), tab_id)) {
                    Some(view) => view.get(row_nr as usize).map(|&p| p as usize),
                    None => Some(row_nr as usize),
                }
                .filter(|&p| p < len);
                match (anchor_position, insert_position) {
                    (Some(p), InsertPosition::Before) => p,
                    (Some(p), InsertPosition::After) => p + 1,
                    (None, _) => len,
                }
            }
            None => len,
        };
        self.insert_master_row(filename, position, (master_row, empty_record))?;

        self.push_undo(filename, UndoEntry::RowInsert { filename: filename.to_string(), master_row });
        Some(master_row)
    }

    /// Remove the field at `position` from every row's StringRecord in
    /// master and every filtered/sorted view of `filename` (O(rows)), and
    /// remove `header_id`'s FileHeader from every open tab's columns Vec for
    /// that file. Returns each row's value at that position before removal
    /// (keyed by master_row), for undo to restore.
    fn remove_column_at(&mut self, filename: &str, position: usize, header_id: ColumnId) -> Vec<(usize, String)> {
        let values_by_row: Vec<(usize, String)> = self
            .sheets_data
            .get(filename)
            .map(|sheet| {
                sheet
                    .iter()
                    .filter_map(|(idx, record)| record.get(position).map(|v| (*idx, v.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(sheet) = self.sheets_data.get_mut(filename) {
            for (_, record) in sheet.iter_mut() {
                *record = record.iter().enumerate().filter(|(i, _)| *i != position).map(|(_, f)| f).collect();
            }
        }
        for tab in self.tree.iter_all_tabs_mut() {
            if let Some(headers) = tab.1.columns.get_mut(filename) {
                headers.retain(|h| h.id != header_id);
            }
        }

        values_by_row
    }

    /// Insert `header` at `position` into every open tab's columns Vec for
    /// `filename`, and insert a field into every row's StringRecord in
    /// master and every filtered/sorted view at that position -- taken from
    /// `values_by_row` where a value exists (keyed by master_row), empty
    /// otherwise. Inverse of `remove_column_at`.
    fn insert_column_at(
        &mut self,
        filename: &str,
        position: usize,
        header: FileHeader,
        values_by_row: &[(usize, String)],
    ) {
        let values: std::collections::HashMap<usize, &str> =
            values_by_row.iter().map(|(idx, v)| (*idx, v.as_str())).collect();

        if let Some(sheet) = self.sheets_data.get_mut(filename) {
            for (idx, record) in sheet.iter_mut() {
                let mut fields: Vec<&str> = record.iter().collect();
                fields.insert(position.min(fields.len()), values.get(idx).copied().unwrap_or(""));
                *record = fields.into_iter().collect();
            }
        }
        for tab in self.tree.iter_all_tabs_mut() {
            if let Some(headers) = tab.1.columns.get_mut(filename) {
                let insert_at = position.min(headers.len());
                headers.insert(insert_at, header.clone());
            }
        }
    }

    /// Delete the column with `column_id` from `filename`. Unlike row
    /// delete, this touches every row's StringRecord (O(rows)) via
    /// `remove_column_at`. Pushes an UndoEntry::ColumnDelete capturing
    /// enough to restore it. Returns `true` if the column was found and
    /// removed.
    fn apply_delete_column(&mut self, filename: &str, column_id: ColumnId) -> bool {
        let Some(headers) = self
            .tree
            .iter_all_tabs()
            .find_map(|(_, tab)| tab.columns.get(filename).cloned())
        else {
            return false;
        };
        let Some(position) = column_position(&headers, column_id) else {
            return false;
        };
        let header = headers[position].clone();

        let values_by_row = self.remove_column_at(filename, position, column_id);

        self.push_undo(
            filename,
            UndoEntry::ColumnDelete { filename: filename.to_string(), position, header, values_by_row },
        );
        true
    }

    /// Insert a new empty column after `after_column_id` (None = append at
    /// the end) into `filename`, named `name`. Mirrors apply_delete_column
    /// via `insert_column_at`. Returns the new column's id, or None if the
    /// file isn't loaded.
    fn apply_insert_column(
        &mut self,
        filename: &str,
        after_column_id: Option<ColumnId>,
        name: &str,
    ) -> Option<ColumnId> {
        let headers = self.tree.iter_all_tabs().find_map(|(_, tab)| tab.columns.get(filename).cloned())?;

        let position = match after_column_id {
            Some(after) => column_position(&headers, after).map_or(headers.len(), |p| p + 1),
            None => headers.len(),
        };

        let column_id = *self.next_col_id.get(filename).unwrap_or(&0);
        self.next_col_id.insert(filename.to_string(), column_id + 1);

        let header = FileHeader { id: column_id, name: name.to_string(), visible: true, sort: None };
        self.insert_column_at(filename, position, header, &[]);

        self.push_undo(filename, UndoEntry::ColumnInsert { filename: filename.to_string(), column_id });
        Some(column_id)
    }
}

impl MyApp {
    /// The tab a newly opened file should display in: the one asked for,
    /// else the focused tab, else the first tab. Menu and drag-and-drop opens
    /// pass `None`, which would otherwise load the file without showing it.
    fn resolve_target_tab(&mut self, tab_id: Option<usize>) -> Option<usize> {
        tab_id
            .or_else(|| self.tree.find_active_focused().map(|(_, tab)| tab.id))
            .or_else(|| self.tree.iter_all_tabs().next().map(|(_, tab)| tab.id))
    }

    pub fn load_file(&mut self, ctx: &egui::Context, file_name: String, tab_id: Option<usize>) {
        let tab_id = self.resolve_target_tab(tab_id);
        self.picked_path = Some(file_name.clone());

        self.files_list.push(file_name.clone());

        let (mut reader, headers) = open_csv_file(&file_name);

        let next_col_id = headers.iter().map(|h| h.id).max().map_or(0, |m| m + 1);
        self.next_col_id.insert(file_name.clone(), next_col_id);

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

                let key = (filename.clone(), tab_id);
                let rows = active_sheet_data(&self.sheets_data, &self.filtered_data, &filename, tab_id, filter_active);

                if rows.len() > 0 {
                    // Sort the tab's current view, or every master row if it has none.
                    let view: View = match self.filtered_data.get(&key) {
                        Some(v) => v.clone(),
                        None => (0..rows.len() as u32).collect(),
                    };
                    // ponytail: clones master for the worker thread; Arc'd master if this shows up in profiles
                    let master_clone = self.sheets_data[&filename].clone();
                    let chan = chan.clone();
                    let ctx = ctx.clone();
                    let filename = filename.clone();
                    let epoch = bump_epoch(&mut self.request_epoch, (filename.clone(), tab_id));

                    thread::spawn(move || {
                        let sorted = sort_indices(&master_clone, view, sort_order);

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
                        let filtered = filter_indices(&master_clone, &filter);

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
        self.agent_bridge.poll(ctx);

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
                    let next_id = master.iter().map(|(idx, _)| *idx).max().map_or(0, |m| m + 1);
                    self.next_row_id.insert(file_name.clone(), next_id);
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
                UiMessage::EditCell(filename, tab_id, row_nr, col_id, new_value) => {
                    // row_nr is an index into the currently displayed (filtered/sorted)
                    // view. Resolve it to a stable master row index first, then always
                    // write through to master -- the filtered view is just a cache.
                    if let Some(master_row) = self.master_row_at(&filename, tab_id, row_nr) {
                        if let Some(old_value) = self.read_cell(&filename, master_row, col_id) {
                            self.write_cell(&filename, master_row, col_id, &new_value);
                            self.push_undo(
                                &filename,
                                UndoEntry::CellEdit { filename: filename.clone(), master_row, col_id, old_value },
                            );
                        }
                    }
                }
                UiMessage::PasteCells(filename, tab_id, anchor_row, anchor_col, rows) => {
                    let pasted_count = self.apply_paste(&filename, tab_id, anchor_row, anchor_col, &rows);
                    if pasted_count > 0 {
                        let plural = if pasted_count == 1 { "" } else { "s" };
                        crate::toast::show(ctx, format!("Pasted {pasted_count} cell{plural}"));
                    }
                }
                UiMessage::ReplaceAll(filename, tab_id, find, replace, scope) => {
                    let changed = self.apply_replace_all(&filename, tab_id, &find, &replace, scope);
                    let plural = if changed == 1 { "" } else { "s" };
                    crate::toast::show(ctx, format!("Replaced in {changed} cell{plural}"));
                }
                UiMessage::DeleteRow(filename, tab_id, row_nr) => {
                    if self.apply_delete_row(&filename, tab_id, row_nr) {
                        crate::toast::show(ctx, "Row deleted");
                    }
                }
                UiMessage::InsertRow(filename, tab_id, row_nr) => {
                    if self.apply_insert_row(&filename, tab_id, row_nr).is_some() {
                        crate::toast::show(ctx, "Row inserted");
                    }
                }
                UiMessage::DeleteColumn(filename, column_id) => {
                    if self.apply_delete_column(&filename, column_id) {
                        crate::toast::show(ctx, "Column deleted");
                    }
                }
                UiMessage::InsertColumn(filename, after_column_id, name) => {
                    if self.apply_insert_column(&filename, after_column_id, &name).is_some() {
                        crate::toast::show(ctx, "Column inserted");
                    }
                }
                UiMessage::Undo(filename) => {
                    if let Some(result) = self.apply_undo(&filename) {
                        let (title, body) = undo_redo_toast("Undo", &result);
                        crate::toast::show_titled(ctx, title, body);
                    }
                }
                UiMessage::Redo(filename) => {
                    if let Some(result) = self.apply_redo(&filename) {
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
                            self.history.mark_clean(&filename);
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

            if undo || redo {
                let focused_file = self
                    .tree
                    .find_active_focused()
                    .and_then(|(_, tab)| {
                        let f = tab.chosen_file.clone();
                        if f.is_empty() { None } else { Some(f) }
                    });

                if let Some(filename) = focused_file {
                    if undo {
                        self.worker_chan.0.send_msg(UiMessage::Undo(filename));
                    } else {
                        self.worker_chan.0.send_msg(UiMessage::Redo(filename));
                    }
                }
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
                    history: &self.history,
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
            request_epoch: HashMap::new(),
            history: Default::default(),
            next_row_id: HashMap::new(),
            next_col_id: HashMap::new(),
            agent_bridge: agent_bridge::AgentBridge::new(),
        }
    }

    fn row(idx: usize, fields: &[&str]) -> (usize, StringRecord) {
        (idx, StringRecord::from(fields.to_vec()))
    }

    fn set_columns(app: &mut MyApp, filename: &str, count: usize) {
        let headers = (0..count)
            .map(|i| crate::types::FileHeader {
                id: i,
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

    fn push_undo_entry(app: &mut MyApp, filename: &str, entry: UndoEntry) {
        app.history.push_undo_entry(&filename.to_string(), entry);
    }

    fn push_redo_entry(app: &mut MyApp, filename: &str, entry: UndoEntry) {
        app.history.push_redo_entry(&filename.to_string(), entry);
    }

    fn undo_len(app: &MyApp, filename: &str) -> usize {
        app.history.undo_len(&filename.to_string())
    }

    fn redo_len(app: &MyApp, filename: &str) -> usize {
        app.history.redo_len(&filename.to_string())
    }

    #[test]
    fn open_without_tab_targets_a_tab_instead_of_none() {
        let mut app = test_app();
        assert_eq!(app.resolve_target_tab(Some(7)), Some(7));
        assert_eq!(app.resolve_target_tab(None), Some(1));
    }

    #[test]
    fn undo_restores_previous_value_and_pushes_redo() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"])],
        );
        set_columns(&mut app, "f.csv", 1);
        push_undo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "original".to_string(),
        });

        let result = app.apply_undo("f.csv").expect("undo should apply");
        assert_eq!(result.restored, "original");
        assert_eq!(result.overwritten, "a");
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("original".to_string()));
        assert_eq!(redo_len(&app, "f.csv"), 1);
        match app.history.peek_redo(&"f.csv".to_string()).unwrap() {
            UndoEntry::CellEdit { old_value, .. } => assert_eq!(old_value, "a"),
            other => panic!("expected CellEdit, got a different UndoEntry variant: {:?}", std::mem::discriminant(other)),
        }
        // The undo_stack entry pushed directly above (simulating a prior
        // edit) is back to empty after this undo, matching clean_marker's
        // default of 0 -- so the file reads as clean, not dirty. See
        // undo_back_to_clean_marker_is_not_dirty for the full edit-then-undo
        // scenario via write_cell + push_undo.
        assert!(!app.history.is_dirty(&"f.csv".to_string()));
    }

    /// Regression test for the reported bug: editing a cell then undoing
    /// back to the original value must NOT leave the file marked modified.
    #[test]
    fn undo_back_to_clean_marker_is_not_dirty() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);
        // File just loaded: clean_marker defaults to 0, undo_stack is empty.
        assert!(!app.history.is_dirty(&"f.csv".to_string()));

        // Simulate a real edit: write the new value, then push_undo (the
        // same path UiMessage::EditCell takes).
        app.write_cell("f.csv", 0, 0, "b");
        app.push_undo(
            "f.csv",
            UndoEntry::CellEdit {
                filename: "f.csv".to_string(),
                master_row: 0,
                col_id: 0,
                old_value: "a".to_string(),
            },
        );
        assert!(app.history.is_dirty(&"f.csv".to_string()));

        // Undo back to the original value: stack length returns to 0,
        // matching clean_marker, so the file must read as clean again.
        app.apply_undo("f.csv");
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
        assert!(!app.history.is_dirty(&"f.csv".to_string()));

        // Redo re-applies the edit: dirty again.
        app.apply_redo("f.csv");
        assert!(app.history.is_dirty(&"f.csv".to_string()));
    }

    /// A file can be dirty even at undo_stack.len() == 0 relative to a
    /// nonzero baseline: save while dirty, edit again, undo back past the
    /// point where it was saved -- it's dirty relative to what's on disk
    /// even though the stack is shorter than it was mid-edit.
    #[test]
    fn dirty_tracked_relative_to_save_point_not_zero() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);

        app.write_cell("f.csv", 0, 0, "b");
        app.push_undo(
            "f.csv",
            UndoEntry::CellEdit {
                filename: "f.csv".to_string(),
                master_row: 0,
                col_id: 0,
                old_value: "a".to_string(),
            },
        );
        // Simulate a save: mark this stack depth (1) as the new clean point.
        app.history.mark_clean(&"f.csv".to_string());
        assert!(!app.history.is_dirty(&"f.csv".to_string()));

        // Undo below the save point: now dirty relative to what's on disk,
        // even though the stack is shorter than its mid-edit peak.
        app.apply_undo("f.csv");
        assert!(app.history.is_dirty(&"f.csv".to_string()));
    }

    #[test]
    fn undo_then_redo_round_trips() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["current"])]);
        set_columns(&mut app, "f.csv", 1);
        push_undo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "before".to_string(),
        });

        app.apply_undo("f.csv");
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("before".to_string()));

        app.apply_redo("f.csv");
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("current".to_string()));
        // redo is itself undoable, so it lands back on undo_stack.
        assert_eq!(undo_len(&app, "f.csv"), 1);
        assert_eq!(redo_len(&app, "f.csv"), 0);
    }

    #[test]
    fn multiple_undos_restore_in_reverse_order() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["v3"])]);
        set_columns(&mut app, "f.csv", 1);
        // Simulate two edits: v1 -> v2 -> v3, each push recording the prior value.
        push_undo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "v1".to_string(),
        });
        push_undo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "v2".to_string(),
        });

        assert!(app.apply_undo("f.csv").is_some());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("v2".to_string()));
        assert!(app.apply_undo("f.csv").is_some());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("v1".to_string()));
    }

    #[test]
    fn undo_on_empty_stack_is_noop() {
        let mut app = test_app();
        assert!(app.apply_undo("f.csv").is_none());
        assert!(!app.history.any_dirty());
    }

    #[test]
    fn redo_on_empty_stack_is_noop() {
        let mut app = test_app();
        assert!(app.apply_redo("f.csv").is_none());
        assert!(!app.history.any_dirty());
    }

    #[test]
    fn undo_missing_target_cell_is_noop_and_drops_entry() {
        let mut app = test_app();
        // File was closed/removed since the edit was made.
        push_undo_entry(&mut app, "gone.csv", UndoEntry::CellEdit {
            filename: "gone.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "x".to_string(),
        });

        assert!(app.apply_undo("gone.csv").is_none());
        assert_eq!(undo_len(&app, "gone.csv"), 0);
        assert_eq!(redo_len(&app, "gone.csv"), 0);
    }

    #[test]
    fn new_edit_clears_redo_stack() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["b"])]);
        set_columns(&mut app, "f.csv", 1);
        push_redo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 0,
            old_value: "stale".to_string(),
        });

        if let Some(old_value) = app.read_cell("f.csv", 0, 0) {
            app.push_undo("f.csv", UndoEntry::CellEdit {
                filename: "f.csv".to_string(),
                master_row: 0,
                col_id: 0,
                old_value,
            });
        }
        app.write_cell("f.csv", 0, 0, "c");

        assert_eq!(redo_len(&app, "f.csv"), 0);
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
        assert!(app.history.is_dirty(&"f.csv".to_string()));
        // The whole paste is one undoable action, not four separate entries.
        assert_eq!(undo_len(&app, "f.csv"), 1);
        match app.history.peek_undo(&"f.csv".to_string()).unwrap() {
            UndoEntry::Batch(entries) => assert_eq!(entries.len(), 4),
            other => panic!("expected a Batch, got {:?}", std::mem::discriminant(other)),
        }
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
        assert!(!app.history.any_dirty());
    }

    #[test]
    fn replace_all_columns_across_all_rows_when_no_filter() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["foo", "bar"]), row(1, &["foobar", "baz"])],
        );
        set_columns(&mut app, "f.csv", 2);

        let count = app.apply_replace_all("f.csv", 1, "foo", "X", ReplaceScope::AllColumns);

        assert_eq!(count, 2);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("X".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("bar".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("Xbar".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("baz".to_string()));
        // The whole replace-all is one undoable action.
        assert_eq!(undo_len(&app, "f.csv"), 1);
        match app.history.peek_undo(&"f.csv".to_string()).unwrap() {
            UndoEntry::Batch(entries) => assert_eq!(entries.len(), 2),
            other => panic!("expected a Batch, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn replace_current_column_only() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["foo", "foo"])],
        );
        set_columns(&mut app, "f.csv", 2);

        let count = app.apply_replace_all("f.csv", 1, "foo", "X", ReplaceScope::CurrentColumn(1));

        assert_eq!(count, 1);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("foo".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("X".to_string()));
    }

    #[test]
    fn replace_all_only_touches_filtered_rows_when_filter_active() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["foo"]), row(1, &["foo"]), row(2, &["foo"])],
        );
        set_columns(&mut app, "f.csv", 1);
        // Simulate an active filter that only surfaced row 1 (master index 1).
        app.filtered_data.insert(
            ("f.csv".to_string(), 1),
            vec![1u32],
        );

        let count = app.apply_replace_all("f.csv", 1, "foo", "X", ReplaceScope::AllColumns);

        assert_eq!(count, 1);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("foo".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("X".to_string()));
        assert_eq!(app.read_cell("f.csv", 2, 0), Some("foo".to_string()));
    }

    #[test]
    fn replace_all_no_match_changes_nothing() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["bar"])]);
        set_columns(&mut app, "f.csv", 1);

        let count = app.apply_replace_all("f.csv", 1, "foo", "X", ReplaceScope::AllColumns);

        assert_eq!(count, 0);
        assert!(!app.history.any_dirty());
        assert_eq!(undo_len(&app, "f.csv"), 0);
    }

    #[test]
    fn replace_all_empty_find_is_noop() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["bar"])]);
        set_columns(&mut app, "f.csv", 1);

        let count = app.apply_replace_all("f.csv", 1, "", "X", ReplaceScope::AllColumns);

        assert_eq!(count, 0);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("bar".to_string()));
    }

    #[test]
    fn delete_row_removes_by_stable_master_row() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"]), row(2, &["c"])],
        );

        assert!(app.apply_delete_row("f.csv", 1, 1));

        let remaining: Vec<usize> = app.sheets_data["f.csv"].iter().map(|(idx, _)| *idx).collect();
        assert_eq!(remaining, vec![0, 2]);
        assert!(app.history.is_dirty(&"f.csv".to_string()));
        assert_eq!(undo_len(&app, "f.csv"), 1);
    }

    #[test]
    fn delete_row_also_removes_from_filtered_views() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"])],
        );
        app.filtered_data.insert(
            ("f.csv".to_string(), 1),
            vec![0u32, 1],
        );

        assert!(app.apply_delete_row("f.csv", 1, 0));

        // 'b' (master row 1) shifted from position 1 to 0.
        assert_eq!(app.filtered_data[&("f.csv".to_string(), 1)], vec![0u32]);
        assert_eq!(app.sheets_data["f.csv"][0].0, 1);
    }

    #[test]
    fn insert_row_shifts_views_so_they_keep_pointing_at_the_same_rows() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"]), row(1, &["b"])]);
        app.next_row_id.insert("f.csv".to_string(), 2);
        set_columns(&mut app, "f.csv", 1);
        app.filtered_data.insert(("f.csv".to_string(), 1), vec![1u32]); // view shows only 'b'

        app.apply_insert_row("f.csv", 1, Some((0, InsertPosition::Before)));

        let view = &app.filtered_data[&("f.csv".to_string(), 1)];
        assert_eq!(app.sheets_data["f.csv"][view[0] as usize].0, 1);
    }

    #[test]
    fn delete_row_missing_row_is_noop() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);

        assert!(!app.apply_delete_row("f.csv", 1, 5));
        assert!(!app.history.any_dirty());
    }

    #[test]
    fn insert_row_assigns_fresh_master_row_id() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"])],
        );
        app.next_row_id.insert("f.csv".to_string(), 2);
        set_columns(&mut app, "f.csv", 1);

        let new_id = app.apply_insert_row("f.csv", 1, None).expect("insert should apply");

        assert_eq!(new_id, 2);
        assert_eq!(app.sheets_data["f.csv"].len(), 3);
        assert_eq!(app.read_cell("f.csv", 2, 0), Some("".to_string()));
        assert_eq!(app.next_row_id["f.csv"], 3);
        assert!(app.history.is_dirty(&"f.csv".to_string()));
    }

    #[test]
    fn insert_row_after_anchor_lands_immediately_below() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"]), row(2, &["c"])],
        );
        app.next_row_id.insert("f.csv".to_string(), 3);
        set_columns(&mut app, "f.csv", 1);

        // Insert after displayed row 0 ("a") -- should land between "a" and "b".
        let new_id = app
            .apply_insert_row("f.csv", 1, Some((0, InsertPosition::After)))
            .expect("insert should apply");

        let order: Vec<usize> = app.sheets_data["f.csv"].iter().map(|(idx, _)| *idx).collect();
        assert_eq!(order, vec![0, new_id, 1, 2]);
    }

    #[test]
    fn insert_row_before_anchor_lands_immediately_above() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"]), row(2, &["c"])],
        );
        app.next_row_id.insert("f.csv".to_string(), 3);
        set_columns(&mut app, "f.csv", 1);

        // Insert before displayed row 0 ("a") -- should land at the very top,
        // the case that was previously unrepresentable.
        let new_id = app
            .apply_insert_row("f.csv", 1, Some((0, InsertPosition::Before)))
            .expect("insert should apply");

        let order: Vec<usize> = app.sheets_data["f.csv"].iter().map(|(idx, _)| *idx).collect();
        assert_eq!(order, vec![new_id, 0, 1, 2]);
    }

    #[test]
    fn delete_then_undo_restores_row_at_original_position() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"]), row(2, &["c"])],
        );
        set_columns(&mut app, "f.csv", 1);

        app.apply_delete_row("f.csv", 1, 1);
        assert_eq!(app.sheets_data["f.csv"].len(), 2);

        let result = app.apply_undo("f.csv").expect("undo should restore the row");
        assert_eq!(result.row, Some(1));
        assert_eq!(app.sheets_data["f.csv"].len(), 3);
        let restored: Vec<usize> = app.sheets_data["f.csv"].iter().map(|(idx, _)| *idx).collect();
        assert_eq!(restored, vec![0, 1, 2]);
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("b".to_string()));
    }

    #[test]
    fn insert_then_undo_removes_the_row() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        app.next_row_id.insert("f.csv".to_string(), 1);
        set_columns(&mut app, "f.csv", 1);

        app.apply_insert_row("f.csv", 1, None);
        assert_eq!(app.sheets_data["f.csv"].len(), 2);

        app.apply_undo("f.csv");
        assert_eq!(app.sheets_data["f.csv"].len(), 1);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
    }

    #[test]
    fn delete_undo_redo_round_trips() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a"]), row(1, &["b"])],
        );

        app.apply_delete_row("f.csv", 1, 1);
        app.apply_undo("f.csv");
        app.apply_redo("f.csv");

        assert_eq!(app.sheets_data["f.csv"].len(), 1);
        let remaining: Vec<usize> = app.sheets_data["f.csv"].iter().map(|(idx, _)| *idx).collect();
        assert_eq!(remaining, vec![0]);
    }

    /// Regression test for the bug jon-42i's design identified and jon-r7f
    /// fixes: an UndoEntry recorded for a column, followed by a column
    /// shift (simulating what a future column delete would do), must still
    /// undo to the SAME column -- not whatever column now sits at the old
    /// position.
    #[test]
    fn undo_survives_a_column_position_shift() {
        let mut app = test_app();
        // Three columns: id 0 "name", id 1 "dupa", id 2 "age".
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["alice", "x", "30"])],
        );
        let headers = vec![
            crate::types::FileHeader { id: 0, name: "name".to_string(), visible: true, sort: None },
            crate::types::FileHeader { id: 1, name: "dupa".to_string(), visible: true, sort: None },
            crate::types::FileHeader { id: 2, name: "age".to_string(), visible: true, sort: None },
        ];
        for tab in app.tree.iter_all_tabs_mut() {
            tab.1.columns.insert("f.csv".to_string(), headers);
            break;
        }

        // Edit column id 1 ("dupa"): "x" -> "y". Records an UndoEntry with
        // col_id: 1, old_value: "x".
        push_undo_entry(&mut app, "f.csv", UndoEntry::CellEdit {
            filename: "f.csv".to_string(),
            master_row: 0,
            col_id: 1,
            old_value: "x".to_string(),
        });
        app.write_cell("f.csv", 0, 1, "y");
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("y".to_string()));

        // Simulate a column delete: "name" (id 0) is removed, shifting
        // "dupa" from position 1 to position 0 and "age" from 2 to 1.
        for tab in app.tree.iter_all_tabs_mut() {
            if let Some(headers) = tab.1.columns.get_mut("f.csv") {
                headers.retain(|h| h.id != 0);
            }
        }
        for (_, record) in app.sheets_data.get_mut("f.csv").unwrap() {
            *record = record.iter().skip(1).collect();
        }

        // "dupa" (id 1) is now at position 0, "age" (id 2) at position 1.
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("y".to_string())); // still id 1
        assert_eq!(app.read_cell("f.csv", 0, 2), Some("30".to_string())); // age untouched

        // Undo the edit made before the shift. It must restore "dupa" (id 1,
        // now at position 0) back to "x" -- NOT write into whatever is now
        // at the old position-1 slot (which would be "age").
        let result = app.apply_undo("f.csv").expect("undo should apply");
        assert_eq!(result.column, Some("dupa".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("x".to_string()));
        // "age" (id 2) must be untouched by the undo.
        assert_eq!(app.read_cell("f.csv", 0, 2), Some("30".to_string()));
    }

    #[test]
    fn delete_column_removes_field_and_header_by_id() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b", "c"]), row(1, &["d", "e", "f"])],
        );
        set_columns(&mut app, "f.csv", 3); // ids 0, 1, 2

        assert!(app.apply_delete_column("f.csv", 1));

        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("d".to_string()));
        // Column 2 ("c"/"f") is now at position 1 after the delete.
        assert_eq!(app.read_cell("f.csv", 0, 2), Some("c".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 2), Some("f".to_string()));

        for tab in app.tree.iter_all_tabs() {
            let headers = tab.1.columns.get("f.csv").unwrap();
            assert_eq!(headers.len(), 2);
            assert!(headers.iter().all(|h| h.id != 1));
        }
        assert!(app.history.is_dirty(&"f.csv".to_string()));
    }

    #[test]
    fn delete_column_missing_id_is_noop() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);

        assert!(!app.apply_delete_column("f.csv", 99));
        assert!(!app.history.any_dirty());
    }

    #[test]
    fn insert_column_appends_empty_field_with_fresh_id() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b"])],
        );
        set_columns(&mut app, "f.csv", 2); // ids 0, 1
        app.next_col_id.insert("f.csv".to_string(), 2);

        let new_id = app.apply_insert_column("f.csv", None, "new").expect("insert should apply");

        assert_eq!(new_id, 2);
        assert_eq!(app.read_cell("f.csv", 0, 2), Some("".to_string()));
        assert_eq!(app.next_col_id["f.csv"], 3);

        let headers = app.tree.iter_all_tabs().next().unwrap().1.columns.get("f.csv").unwrap();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[2].name, "new");
    }

    #[test]
    fn insert_column_after_specific_id_lands_in_position() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a", "b"])]);
        set_columns(&mut app, "f.csv", 2); // ids 0, 1
        app.next_col_id.insert("f.csv".to_string(), 2);

        app.apply_insert_column("f.csv", Some(0), "mid");

        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 2), Some("".to_string())); // new col, id 2
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("b".to_string())); // untouched, id 1

        let headers = app.tree.iter_all_tabs().next().unwrap().1.columns.get("f.csv").unwrap();
        assert_eq!(headers[1].name, "mid");
    }

    #[test]
    fn delete_column_then_undo_restores_header_position_and_values() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b", "c"]), row(1, &["d", "e", "f"])],
        );
        set_columns(&mut app, "f.csv", 3);

        app.apply_delete_column("f.csv", 1);
        let result = app.apply_undo("f.csv").expect("undo should restore the column");
        assert_eq!(result.column, Some("col1".to_string()));

        assert_eq!(app.read_cell("f.csv", 0, 1), Some("b".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("e".to_string()));

        let headers = app.tree.iter_all_tabs().next().unwrap().1.columns.get("f.csv").unwrap();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[1].id, 1);
    }

    #[test]
    fn insert_column_then_undo_removes_it() {
        let mut app = test_app();
        app.sheets_data.insert("f.csv".to_string(), vec![row(0, &["a"])]);
        set_columns(&mut app, "f.csv", 1);
        app.next_col_id.insert("f.csv".to_string(), 1);

        app.apply_insert_column("f.csv", None, "new");
        assert_eq!(app.sheets_data["f.csv"][0].1.len(), 2);

        app.apply_undo("f.csv");
        assert_eq!(app.sheets_data["f.csv"][0].1.len(), 1);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));

        let headers = app.tree.iter_all_tabs().next().unwrap().1.columns.get("f.csv").unwrap();
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn delete_column_undo_redo_round_trips() {
        let mut app = test_app();
        app.sheets_data.insert(
            "f.csv".to_string(),
            vec![row(0, &["a", "b"])],
        );
        set_columns(&mut app, "f.csv", 2);

        app.apply_delete_column("f.csv", 0);
        app.apply_undo("f.csv");
        app.apply_redo("f.csv");

        assert_eq!(app.sheets_data["f.csv"][0].1.len(), 1);
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("b".to_string()));
        let headers = app.tree.iter_all_tabs().next().unwrap().1.columns.get("f.csv").unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].id, 1);
    }

    /// Regression test: pasting a multi-cell block must undo/redo as ONE
    /// action, not one Cmd+Z per cell.
    #[test]
    fn paste_undo_restores_all_cells_in_one_action() {
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
        app.apply_paste("f.csv", 1, 0, 0, &rows);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("x".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("w".to_string()));

        // A single undo must restore every pasted cell.
        assert!(app.apply_undo("f.csv").is_some());
        assert_eq!(undo_len(&app, "f.csv"), 0);
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("a".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("b".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("c".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("d".to_string()));

        // A second undo must be a no-op (nothing left to undo) -- proves the
        // whole paste was ONE undo item, not four.
        assert!(app.apply_undo("f.csv").is_none());

        // A single redo must re-apply every pasted cell.
        assert!(app.apply_redo("f.csv").is_some());
        assert_eq!(app.read_cell("f.csv", 0, 0), Some("x".to_string()));
        assert_eq!(app.read_cell("f.csv", 0, 1), Some("y".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 0), Some("z".to_string()));
        assert_eq!(app.read_cell("f.csv", 1, 1), Some("w".to_string()));
        assert!(app.apply_redo("f.csv").is_none());
    }
}
