use crate::types::{SheetVec, SortOrder};

/// Wrap a CSV field value in double-quotes if it contains a comma, double-quote, or newline.
/// Internal double-quotes are escaped by doubling them.
pub fn csv_quote(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub fn sort_data(mut sheet_clone: SheetVec, sort_by: (usize, SortOrder)) -> SheetVec {
    sheet_clone.sort_by(|a, b| -> std::cmp::Ordering {
        let val_a = a.get(sort_by.0).unwrap_or_default();
        let val_b = b.get(sort_by.0).unwrap_or_default();

        if sort_by.1 == SortOrder::Dsc {
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
