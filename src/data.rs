use crate::types::{SheetVec, SortOrder};

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
