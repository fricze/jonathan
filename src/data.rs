use crate::types::{FileHeader, SheetVec, SortOrder};

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

pub fn sort_data(mut sheet_clone: SheetVec, sort_by: (usize, SortOrder)) -> SheetVec {
    sheet_clone.sort_by(|(_, a), (_, b)| -> std::cmp::Ordering {
        let val_a = a.get(sort_by.0).unwrap_or_default();
        let val_b = b.get(sort_by.0).unwrap_or_default();

        if sort_by.1 == SortOrder::Asc {
            val_a.cmp(val_b)
        } else {
            val_b.cmp(val_a)
        }
    });

    sheet_clone
}

pub fn filter_data(master_data: SheetVec, filter: String) -> SheetVec {
    master_data
        .iter()
        .filter(|(_, r)| r.iter().any(|c| c.contains(&filter)))
        .cloned()
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use csv::StringRecord;

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
    fn sort_data_preserves_master_row_index() {
        let sheet: SheetVec = vec![row(0, &["3"]), row(1, &["1"]), row(2, &["2"])];
        let sorted = sort_data(sheet, (0, SortOrder::Asc));
        let values: Vec<&str> = sorted.iter().map(|(_, r)| r.get(0).unwrap()).collect();
        assert_eq!(values, vec!["1", "2", "3"]);
        let indices: Vec<usize> = sorted.iter().map(|(idx, _)| *idx).collect();
        assert_eq!(indices, vec![1, 2, 0]);
    }

    #[test]
    fn sort_data_descending() {
        let sheet: SheetVec = vec![row(0, &["1"]), row(1, &["3"]), row(2, &["2"])];
        let sorted = sort_data(sheet, (0, SortOrder::Dsc));
        let values: Vec<&str> = sorted.iter().map(|(_, r)| r.get(0).unwrap()).collect();
        assert_eq!(values, vec!["3", "2", "1"]);
    }

    #[test]
    fn filter_data_keeps_matching_rows_and_their_master_index() {
        let sheet: SheetVec = vec![row(0, &["apple"]), row(1, &["banana"]), row(2, &["grape"])];
        let filtered = filter_data(sheet, "an".to_string());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, 1);
        assert_eq!(filtered[0].1.get(0), Some("banana"));
    }

    #[test]
    fn filter_data_empty_filter_matches_everything() {
        let sheet: SheetVec = vec![row(0, &["a"]), row(1, &["b"])];
        let filtered = filter_data(sheet, "".to_string());
        assert_eq!(filtered.len(), 2);
    }
}
