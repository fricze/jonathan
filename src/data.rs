use crate::types::{FileHeader, SheetVec, SortOrder};

/// Update a single cell in a sheet. Returns `true` if the row and column existed.
pub fn edit_record(sheet: &mut SheetVec, row: usize, col: usize, value: &str) -> bool {
    if let Some(record) = sheet.get_mut(row) {
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
    for record in data {
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn sort_data(mut sheet_clone: SheetVec, sort_by: (usize, SortOrder)) -> SheetVec {
    sheet_clone.sort_by(|a, b| -> std::cmp::Ordering {
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
        .filter(|r| r.iter().any(|c| c.contains(&filter)))
        .map(|r| r.clone())
        .collect::<Vec<_>>()
}
