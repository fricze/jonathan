//! Pure CSV data model for Jonathan: the row/column types, and the
//! edit/sort/filter/read/write operations on them. No UI dependency --
//! every function here is plain data in, plain data out, independently
//! testable without egui or any app state.

use csv::StringRecord;

pub type ColumnId = usize;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Clone, Default)]
pub struct FileHeader {
    /// Stable identity for this column, assigned once when the file loads
    /// and never reassigned -- unlike its position in the columns Vec, which
    /// shifts on column insert/delete. Mirrors master_row's role for rows.
    pub id: ColumnId,
    pub name: String,
    pub visible: bool,
    pub sort: Option<SortOrder>,
}

/// (master row index, record). The master index is assigned once when a file
/// is loaded and carried through sort/filter so edits to a filtered or sorted
/// view can always be written back to the correct row in master data.
pub type SheetRow = (usize, StringRecord);
pub type SheetVec = Vec<SheetRow>;

/// Update a single cell in a sheet, addressed by master row index. Returns
/// `true` if a row with that master index (and the column) existed.
pub fn edit_record(sheet: &mut SheetVec, master_row: usize, col: usize, value: &str) -> bool {
    if let Some((_, record)) = sheet.iter_mut().find(|(idx, _)| *idx == master_row) {
        if col < record.len() {
            *record = record
                .iter()
                .enumerate()
                .map(|(i, f)| if i == col { value } else { f })
                .collect();
            return true;
        }
    }
    false
}

/// Wrap a CSV field value in double-quotes if it contains a comma, double-quote, or newline.
/// Internal double-quotes are escaped by doubling them.
pub fn csv_quote(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn write_csv(path: &str, headers: &[FileHeader], data: &SheetVec) -> Result<(), csv::Error> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record(headers.iter().map(|h| h.name.as_str()))?;
    for (_, record) in data {
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(())
}

/// A filtered and/or sorted view of a file: positions into the master
/// `SheetVec`, in display order. Positions (not rows) so a view costs 4
/// bytes/row and never goes stale on cell edits; callers must fix them up
/// when master rows are inserted or removed.
pub type View = Vec<u32>;

/// Positions of master rows with any cell containing `filter`.
pub fn filter_indices(master: &SheetVec, filter: &str) -> View {
    master
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| r.iter().any(|c| c.contains(filter)))
        .map(|(i, _)| i as u32)
        .collect()
}

/// `view` sorted by column `sort_by.0` of the rows it points at (stable).
pub fn sort_indices(master: &SheetVec, mut view: View, sort_by: (usize, SortOrder)) -> View {
    let cell = |i: &u32| master[*i as usize].1.get(sort_by.0).unwrap_or_default();
    view.sort_by(|a, b| match sort_by.1 {
        SortOrder::Asc => cell(a).cmp(cell(b)),
        SortOrder::Dsc => cell(b).cmp(cell(a)),
    });
    view
}

pub fn iterate_csv(path: &str) -> csv::Result<(csv::Reader<std::fs::File>, StringRecord)> {
    let mut rdr = csv::Reader::from_path(path)?;
    let headers = rdr.headers()?.clone();
    Ok((rdr, headers))
}

pub fn open_csv_file(path: &str) -> (csv::Reader<std::fs::File>, Vec<FileHeader>) {
    match iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .enumerate()
                .map(|(id, name)| FileHeader {
                    id,
                    name: name.to_string(),
                    visible: true,
                    ..FileHeader::default()
                })
                .collect::<Vec<_>>();
            (csv_reader, headers)
        }
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(idx: usize, fields: &[&str]) -> (usize, StringRecord) {
        (idx, StringRecord::from(fields.to_vec()))
    }

    #[test]
    fn edit_record_updates_by_master_row_not_position() {
        let mut sheet: SheetVec = vec![row(5, &["a", "b"]), row(2, &["c", "d"])];
        assert!(edit_record(&mut sheet, 2, 1, "z"));
        assert_eq!(sheet[1].1.get(1), Some("z"));
        assert_eq!(sheet[0].1.get(1), Some("b"));
    }

    #[test]
    fn edit_record_missing_master_row_returns_false() {
        let mut sheet: SheetVec = vec![row(0, &["a"])];
        assert!(!edit_record(&mut sheet, 99, 0, "z"));
    }

    #[test]
    fn edit_record_out_of_range_col_returns_false() {
        let mut sheet: SheetVec = vec![row(0, &["a"])];
        assert!(!edit_record(&mut sheet, 0, 5, "z"));
    }

    #[test]
    fn csv_quote_wraps_when_special_chars_present() {
        assert_eq!(csv_quote("plain"), "plain");
        assert_eq!(csv_quote("a,b"), "\"a,b\"");
        assert_eq!(csv_quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_quote("a\nb"), "\"a\nb\"");
    }

    #[test]
    fn sort_indices_orders_by_column_and_keeps_master_rows() {
        let sheet: SheetVec = vec![row(0, &["3"]), row(1, &["1"]), row(2, &["2"])];
        let all: View = vec![0, 1, 2];
        assert_eq!(sort_indices(&sheet, all.clone(), (0, SortOrder::Asc)), vec![1, 2, 0]);
        assert_eq!(sort_indices(&sheet, all, (0, SortOrder::Dsc)), vec![0, 2, 1]);
    }

    #[test]
    fn sort_indices_only_reorders_the_given_view() {
        let sheet: SheetVec = vec![row(0, &["3"]), row(1, &["1"]), row(2, &["2"])];
        assert_eq!(sort_indices(&sheet, vec![2, 0], (0, SortOrder::Asc)), vec![2, 0]);
    }

    #[test]
    fn filter_indices_returns_matching_positions() {
        let sheet: SheetVec = vec![row(7, &["apple"]), row(8, &["banana"]), row(9, &["grape"])];
        assert_eq!(filter_indices(&sheet, "an"), vec![1]);
    }

    #[test]
    fn filter_indices_empty_filter_matches_everything() {
        let sheet: SheetVec = vec![row(0, &["a"]), row(1, &["b"])];
        assert_eq!(filter_indices(&sheet, ""), vec![0, 1]);
    }
}
