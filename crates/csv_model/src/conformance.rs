//! Behavioural contract every [`Sheet`] backend must meet, as reusable test
//! code. Two layers:
//!
//! * [`run_all`] -- hand-written scenarios through the trait only.
//! * [`check_ops`] -- model-based: apply the same random sequence of edits to
//!   the backend under test and to [`RefModel`] (an obviously-correct `Vec`)
//!   and require identical contents and return values after every step.
//!
//! A new backend adds one test per layer: `run_all(&make)` and a
//! `proptest!` calling `check_ops(&make, ...)`, where `make` builds the
//! backend from a `SheetVec`.

use csv::StringRecord;
use proptest::prelude::*;

use crate::{InsertedColumn, RemovedColumn, Sheet, SheetRow, SheetVec, SortOrder};

pub type Make = dyn Fn(SheetVec) -> Box<dyn Sheet>;

pub fn rows_of(rows: &[&[&str]]) -> SheetVec {
    rows.iter()
        .enumerate()
        .map(|(i, f)| (i, StringRecord::from(f.to_vec())))
        .collect()
}

pub fn fields(sheet: &dyn Sheet, position: usize) -> Option<Vec<String>> {
    sheet
        .record(position)
        .map(|r| r.iter().map(str::to_string).collect())
}

// ---------------------------------------------------------------- scenarios

/// A backend that is loaded from a file can only be asked to start from
/// rows a file can hold: sequential ids from 0 and at least one field per
/// row (a zero-field row is a blank line on disk, which CSV skips). Edits may
/// still produce such rows later, e.g. by removing a row's only column.
pub fn run_all(make: &Make) {
    run_read_only(make);
    run_edits(make);
}

pub fn run_read_only(make: &Make) {
    empty_sheet(make);
    reads_by_position_and_id(make);
    ragged_rows_read_back_as_they_are(make);
    filter_matches_any_cell(make);
    sort_orders_numbers_by_value_then_text(make);
}

/// The slice of the edit contract that only needs `set_cell`.
pub fn run_cell_edits(make: &Make) {
    set_cell_in_place(make);
    snapshot_is_isolated_from_cell_edits(make);
}

fn set_cell_in_place(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b"], &["c", "d"], &["e"]]));
    assert!(s.set_cell(1, 1, "X"));
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["c".into(), "X".into()]));
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "b".into()]),
        "neighbours untouched"
    );
    assert!(s.set_cell(1, 1, "Y"), "same cell again");
    assert_eq!(s.cell(1, 1).as_deref(), Some("Y"));
    assert!(!s.set_cell(9, 0, "no"), "missing row id");
    assert!(!s.set_cell(2, 1, "no"), "column past the row's end");
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["e".into()]),
        "failed edits change nothing"
    );
    assert!(s.set_cell(0, 0, "multi\nline, \"quoted\""));
    assert_eq!(s.cell(0, 0).as_deref(), Some("multi\nline, \"quoted\""));
    assert!(s.set_cell(0, 0, ""));
    assert_eq!(s.cell(0, 0).as_deref(), Some(""));
    assert_eq!(s.len(), 3);
}

fn snapshot_is_isolated_from_cell_edits(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"]]));
    s.set_cell(0, 0, "first");
    let snap = s.snapshot();
    s.set_cell(0, 0, "second");
    s.set_cell(1, 0, "other");
    assert_eq!(
        snap.cell(0, 0).as_deref(),
        Some("first"),
        "edit made after the snapshot"
    );
    assert_eq!(snap.cell(1, 0).as_deref(), Some("b"));
    assert_eq!(s.cell(0, 0).as_deref(), Some("second"));
}

pub fn run_edits(make: &Make) {
    run_cell_edits(make);
    run_row_edits(make);
    run_column_edits(make);
    snapshot_is_isolated_from_later_edits(make);
}

/// The slice that needs `set_cell`, `remove_row` and `insert_row`.
pub fn run_row_edits(make: &Make) {
    set_cell_by_id(make);
    remove_and_reinsert_row_restores_order_and_id(make);
    insert_row_clamps_to_end(make);
    removing_the_largest_id_lowers_next_row_id(make);
    inserted_rows_can_be_edited_and_removed(make);
    row_edits_keep_filter_and_sort_in_step(make);
    snapshot_is_isolated_from_row_edits(make);
}

pub fn run_column_edits(make: &Make) {
    undoing_an_insert_restores_short_rows_exactly(make);
    undoing_an_insert_with_values_and_edited_rows(make);
    token_sizes_are_reported(make);
    column_restore_survives_edits_made_and_undone_in_between(make);
    filter_and_sort_see_the_values_of_added_columns(make);
    column_remove_insert_round_trip(make);
    short_rows_are_padded_so_inserted_columns_line_up(make);
    removing_a_column_past_a_short_rows_end_leaves_it_alone(make);
    column_ops_interleaved_with_row_and_cell_edits(make);
    removing_every_column_leaves_empty_rows(make);
    snapshot_is_isolated_from_column_edits(make);
}

/// A row shorter than the insertion point is treated as padded with empty
/// fields, so the new column lines up with every other row.
/// A column inserted past the end of short rows pads them; undoing the insert
/// must take the padding off again, not just the new field.
fn undoing_an_insert_restores_short_rows_exactly(make: &Make) {
    let original: [&[&str]; 4] = [&["a", "b", "c"], &["d"], &["e", "f"], &["g", "h", "i", "j"]];
    for col in [0usize, 1, 3, 4, 6] {
        let mut s = make(rows_of(&original));
        let tok = s.insert_column(col, &[]);
        for pos in 0..4 {
            assert!(
                s.cell(pos, col).is_some(),
                "col {col}: the new column exists in every row"
            );
        }
        s.undo_insert_column(col, &tok);
        for (pos, want) in original.iter().enumerate() {
            let want: Vec<String> = want.iter().map(|x| x.to_string()).collect();
            assert_eq!(fields(s.as_ref(), pos), Some(want), "col {col}, row {pos}");
        }
    }
}

fn undoing_an_insert_with_values_and_edited_rows(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b"], &["c"], &["d", "e"]]));
    s.set_cell(0, 1, "B"); // a row that is already edited when the column goes in
    let tok = s.insert_column(3, &[(0, "x".to_string()), (1, "y".to_string())]);
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["c".into(), "".into(), "".into(), "y".into()])
    );
    assert!(s.set_cell(2, 0, "D")); // touched after the insert...
    assert!(s.set_cell(2, 0, "d")); // ...and set back by its own undo
    s.undo_insert_column(3, &tok);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into(), "B".into()]));
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["c".into()]));
    assert_eq!(fields(s.as_ref(), 2), Some(vec!["d".into(), "e".into()]));
}

/// Tokens say how much memory they keep alive (the undo history sums these).
fn token_sizes_are_reported(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b", "c"], &["d", "e", "f"]]));
    let ins = s.insert_column(1, &[]);
    let rem = s.remove_column(1);
    assert!(
        ins.size_bytes() > 0 && ins.size_bytes() < 64 * 1024,
        "{}",
        ins.size_bytes()
    );
    assert!(
        rem.size_bytes() > 0 && rem.size_bytes() < 64 * 1024,
        "{}",
        rem.size_bytes()
    );
}

/// Rows touched between the removal and the restore (and put back as they
/// were by the undo stack) must still get their column back.
fn column_restore_survives_edits_made_and_undone_in_between(make: &Make) {
    let mut s = make(rows_of(&[
        &["a", "b", "c"],
        &["d", "e", "f"],
        &["g", "h", "i"],
        &["j"],
    ]));
    s.set_cell(1, 1, "E"); // already edited when the column goes
    let tok = s.remove_column(1);

    assert!(s.set_cell(0, 0, "A")); // touched after the removal...
    assert!(s.set_cell(0, 0, "a")); // ...and set back by its undo
    let gone = s.remove_row(2).unwrap(); // removed after the removal...
    s.insert_row(2, gone); // ...and put back by its undo
    s.insert_row(4, (9, StringRecord::from(vec!["new"]))); // inserted after...
    s.remove_row(4); // ...and removed again by its undo

    s.restore_column(1, &tok);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["d".into(), "E".into(), "f".into()]),
        "its earlier edit survives"
    );
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["g".into(), "h".into(), "i".into()])
    );
    assert_eq!(fields(s.as_ref(), 3), Some(vec!["j".into()]));
    assert_eq!(s.len(), 4);
}

/// Values that exist only in a column added since load are not in any file
/// bytes, but filter and sort must see them.
fn filter_and_sort_see_the_values_of_added_columns(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"], &["c"]]));
    s.insert_column(1, &[(0, "zebra".to_string()), (2, "apple".to_string())]);
    assert_eq!(s.filter("zebra"), vec![0]);
    assert_eq!(s.filter("apple"), vec![2]);
    assert_eq!(
        s.filter("a"),
        vec![0, 2],
        "the stored 'a', and the added values with an 'a' in them"
    );
    assert_eq!(
        s.sort(vec![0, 1, 2], (1, SortOrder::Asc)),
        vec![1, 2, 0],
        "empty, apple, zebra"
    );
}

fn short_rows_are_padded_so_inserted_columns_line_up(make: &Make) {
    let mut s = make(rows_of(&[
        &["a", "b", "c"],
        &["d"],
        &["e", "f"],
        &["g", "h", "i", "j"],
    ]));
    s.insert_column(2, &[(1, "x".to_string()), (2, "y".to_string())]);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "b".into(), "".into(), "c".into()])
    );
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["d".into(), "".into(), "x".into()]),
        "padded up to the column"
    );
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["e".into(), "f".into(), "y".into()]),
        "exactly as long as the column index"
    );
    assert_eq!(
        fields(s.as_ref(), 3),
        Some(vec![
            "g".into(),
            "h".into(),
            "".into(),
            "i".into(),
            "j".into()
        ])
    );
    for pos in 0..4 {
        assert!(
            s.cell(pos, 2).is_some(),
            "every row has the new column at index 2"
        );
    }
}

fn removing_a_column_past_a_short_rows_end_leaves_it_alone(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b", "c"], &["d"], &["e", "f"]]));
    let first = s.remove_column(1);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into(), "c".into()]));
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["d".into()]),
        "the 1-field row has no column 1"
    );
    assert_eq!(fields(s.as_ref(), 2), Some(vec!["e".into()]));
    let past_the_end = s.remove_column(5);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "c".into()]),
        "nothing reached column 5"
    );
    // Undone in reverse order, the sheet is exactly as it was: the short row
    // is not given a field it never had.
    s.restore_column(5, &past_the_end);
    s.restore_column(1, &first);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["d".into()]));
    assert_eq!(fields(s.as_ref(), 2), Some(vec!["e".into(), "f".into()]));
}

fn column_ops_interleaved_with_row_and_cell_edits(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b"], &["c", "d"], &["e", "f"]]));
    s.set_cell(0, 1, "B"); // an edit before the column op
    let gone = s.remove_row(1).unwrap();
    let tok = s.remove_column(0);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["B".into()]),
        "earlier edit followed the column op"
    );
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["f".into()]));

    // More work, then undone in reverse order, like the app's undo stack.
    s.insert_row(1, (7, StringRecord::from(vec!["new"])));
    assert!(s.set_cell(2, 0, "F"));
    assert!(s.set_cell(2, 0, "f")); // the edit's own undo
    s.remove_row(1); // undo of the insert
    s.restore_column(0, &tok);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into(), "B".into()]));
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["e".into(), "f".into()]));
    s.insert_row(1, gone);
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["c".into(), "d".into()]));
    assert_eq!(s.len(), 3);
}

fn removing_every_column_leaves_empty_rows(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"]]));
    let tok = s.remove_column(0);
    assert_eq!(s.len(), 2);
    assert_eq!(fields(s.as_ref(), 0), Some(vec![]));
    assert_eq!(s.cell(0, 0), None);
    assert_eq!(s.filter(""), vec![0, 1]);
    s.restore_column(0, &tok);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into()]));
    assert_eq!(fields(s.as_ref(), 1), Some(vec!["b".into()]));
}

fn snapshot_is_isolated_from_column_edits(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b"], &["c", "d"]]));
    s.cell(0, 0);
    s.cell(1, 1); // warm anything the backend caches
    let snap = s.snapshot();
    s.remove_column(0);
    s.insert_column(1, &[(0, "zz".to_string())]);
    assert_eq!(fields(snap.as_ref(), 0), Some(vec!["a".into(), "b".into()]));
    assert_eq!(fields(snap.as_ref(), 1), Some(vec!["c".into(), "d".into()]));
    assert_eq!(snap.cell(1, 1).as_deref(), Some("d"));
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["b".into(), "zz".into()]));
}

fn removing_the_largest_id_lowers_next_row_id(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"], &["c"]]));
    assert_eq!(s.next_row_id(), 3);
    let last = s.remove_row(2).unwrap();
    assert_eq!(s.next_row_id(), 2);
    s.insert_row(2, last);
    assert_eq!(s.next_row_id(), 3);
    s.remove_row(0);
    assert_eq!(s.next_row_id(), 3, "the largest id is still present");
}

fn inserted_rows_can_be_edited_and_removed(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"]]));
    s.insert_row(1, (7, StringRecord::from(vec!["new", "row"])));
    assert_eq!(
        (s.row_id(0), s.row_id(1), s.row_id(2)),
        (Some(0), Some(7), Some(1))
    );
    assert_eq!(s.position_of(7), Some(1));
    assert_eq!(s.position_of(1), Some(2));
    assert!(s.set_cell(7, 1, "edited"));
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["new".into(), "edited".into()])
    );
    let gone = s.remove_row(1).unwrap();
    assert_eq!((gone.0, gone.1.get(1)), (7, Some("edited")));
    assert_eq!(s.len(), 2);
    assert_eq!(s.position_of(7), None);
    assert!(!s.set_cell(7, 0, "x"), "a removed row cannot be edited");
    assert_eq!(s.position_of(1), Some(1));
}

fn row_edits_keep_filter_and_sort_in_step(make: &Make) {
    let mut s = make(rows_of(&[&["b"], &["a"], &["c"]]));
    s.remove_row(0);
    s.insert_row(0, (9, StringRecord::from(vec!["zzz"])));
    s.insert_row(3, (10, StringRecord::from(vec!["a2"])));
    // positions now hold: zzz, a, c, a2
    assert_eq!(s.filter("a"), vec![1, 3]);
    assert_eq!(
        s.sort(vec![0, 1, 2, 3], (0, SortOrder::Asc)),
        vec![1, 3, 2, 0]
    );
}

fn snapshot_is_isolated_from_row_edits(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"], &["c"]]));
    let snap = s.snapshot();
    s.remove_row(0);
    s.insert_row(0, (5, StringRecord::from(vec!["new"])));
    s.set_cell(1, 0, "changed");
    assert_eq!(snap.len(), 3);
    assert_eq!(
        (snap.row_id(0), snap.row_id(1), snap.row_id(2)),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!(snap.cell(0, 0).as_deref(), Some("a"));
    assert_eq!(snap.cell(1, 0).as_deref(), Some("b"));
    assert_eq!(snap.position_of(5), None);
}

fn empty_sheet(make: &Make) {
    let s = make(vec![]);
    assert_eq!(s.len(), 0);
    assert!(s.is_empty());
    assert_eq!(s.next_row_id(), 0);
    assert_eq!(s.row_id(0), None);
    assert!(s.record(0).is_none());
    assert!(s.cell(0, 0).is_none());
    assert_eq!(s.position_of(0), None);
    assert!(s.filter("x").is_empty());
}

fn reads_by_position_and_id(make: &Make) {
    let s = make(rows_of(&[&["a", "b"], &["c", "d"], &["e", "f"]]));
    assert_eq!(s.len(), 3);
    assert_eq!(
        (s.row_id(0), s.row_id(1), s.row_id(2)),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!(s.row_id(3), None);
    assert_eq!(s.position_of(2), Some(2));
    assert_eq!(s.position_of(3), None);
    assert_eq!(s.next_row_id(), 3);
    assert_eq!(s.cell(1, 1).as_deref(), Some("d"));
    assert_eq!(s.cell(1, 2), None);
    assert_eq!(s.cell(3, 0), None);
    assert_eq!(fields(s.as_ref(), 2), Some(vec!["e".into(), "f".into()]));
}

fn ragged_rows_read_back_as_they_are(make: &Make) {
    let s = make(rows_of(&[&["a"], &["b", "c", "d"], &[""]]));
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into()]));
    assert_eq!(fields(s.as_ref(), 1).unwrap().len(), 3);
    assert_eq!(fields(s.as_ref(), 2), Some(vec!["".into()]));
    assert_eq!(s.cell(0, 1), None);
}

fn set_cell_by_id(make: &Make) {
    let mut s = make(rows_of(&[&["z"], &["a", "b"], &["c", "d"]]));
    s.remove_row(0); // ids now differ from positions: id 2 sits at position 1
    assert!(s.set_cell(2, 1, "X"));
    assert_eq!(s.cell(1, 1).as_deref(), Some("X"));
    assert_eq!(s.cell(0, 1).as_deref(), Some("b"));
    assert!(!s.set_cell(5, 0, "no"), "missing row id");
    assert!(!s.set_cell(0, 0, "no"), "removed row id");
    assert!(!s.set_cell(2, 2, "no"), "column past the row's end");
    assert!(s.set_cell(1, 0, "multi\nline, \"quoted\""));
    assert_eq!(s.cell(0, 0).as_deref(), Some("multi\nline, \"quoted\""));
    assert!(s.set_cell(1, 0, ""));
    assert_eq!(s.cell(0, 0).as_deref(), Some(""));
}

fn remove_and_reinsert_row_restores_order_and_id(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"], &["c"]]));
    let removed: SheetRow = s.remove_row(1).unwrap();
    assert_eq!(removed.0, 1);
    assert_eq!(removed.1.get(0), Some("b"));
    assert_eq!(s.len(), 2);
    assert_eq!((s.row_id(0), s.row_id(1)), (Some(0), Some(2)));
    assert_eq!(s.position_of(2), Some(1));
    assert_eq!(s.position_of(1), None);
    assert!(s.remove_row(2).is_none(), "out of range");
    s.insert_row(1, removed);
    assert_eq!(
        (s.row_id(0), s.row_id(1), s.row_id(2)),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!(s.cell(1, 0).as_deref(), Some("b"));
}

fn insert_row_clamps_to_end(make: &Make) {
    let mut s = make(rows_of(&[&["a"]]));
    s.insert_row(99, (5, StringRecord::from(vec!["z"])));
    assert_eq!(s.len(), 2);
    assert_eq!(s.row_id(1), Some(5));
    assert_eq!(s.next_row_id(), 6);
    s.insert_row(0, (6, StringRecord::from(vec!["first"])));
    assert_eq!(s.cell(0, 0).as_deref(), Some("first"));
    assert_eq!(s.position_of(0), Some(1));
}

fn column_remove_insert_round_trip(make: &Make) {
    let mut s = make(rows_of(&[&["a", "b", "c"], &["d", "e", "f"], &["g"]]));
    let tok = s.remove_column(1);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into(), "c".into()]));
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["g".into()]),
        "short row untouched"
    );
    s.restore_column(1, &tok);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["d".into(), "e".into(), "f".into()])
    );
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["g".into()]),
        "exact inverse: still one field"
    );
    s.insert_column(0, &[]);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["".into(), "a".into(), "b".into(), "c".into()])
    );
    s.insert_column(4, &[(0, "end".to_string())]); // one past the last column
    assert_eq!(s.cell(0, 4).as_deref(), Some("end"));
    assert_eq!(s.cell(1, 4).as_deref(), Some(""));
}

fn snapshot_is_isolated_from_later_edits(make: &Make) {
    let mut s = make(rows_of(&[&["a"], &["b"]]));
    let snap = s.snapshot();
    s.set_cell(0, 0, "changed");
    s.remove_row(1);
    s.insert_column(0, &[]);
    assert_eq!(snap.len(), 2);
    assert_eq!(snap.cell(0, 0).as_deref(), Some("a"));
    assert_eq!(snap.cell(1, 0).as_deref(), Some("b"));
}

fn filter_matches_any_cell(make: &Make) {
    let s = make(rows_of(&[
        &["apple", "x"],
        &["y", "grape"],
        &["pear"],
        &[""],
    ]));
    assert_eq!(s.filter("ap"), vec![0, 1]);
    assert_eq!(s.filter("pear"), vec![2]);
    assert_eq!(s.filter("zzz"), Vec::<u32>::new());
    assert_eq!(
        s.filter(""),
        vec![0, 1, 2, 3],
        "an empty needle matches every row, even one with no cells"
    );
}

fn sort_orders_numbers_by_value_then_text(make: &Make) {
    let s = make(rows_of(&[
        &["10"],
        &["abc"],
        &["2"],
        &["-1.5"],
        &[""],
        &["3"],
    ]));
    let all: Vec<u32> = (0..6).collect();
    assert_eq!(
        s.sort(all.clone(), (0, SortOrder::Asc)),
        vec![3, 2, 5, 0, 4, 1]
    );
    assert_eq!(s.sort(all, (0, SortOrder::Dsc)), vec![1, 4, 0, 5, 2, 3]);
    assert_eq!(
        s.sort(vec![2, 0], (0, SortOrder::Asc)),
        vec![2, 0],
        "only the given view"
    );
}

// ------------------------------------------------------------ model-based

/// The simplest thing that could possibly be correct.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefModel {
    pub rows: Vec<(usize, Vec<String>)>,
}

impl RefModel {
    pub fn from_rows(rows: &SheetVec) -> Self {
        RefModel {
            rows: rows
                .iter()
                .map(|(i, r)| (*i, r.iter().map(str::to_string).collect()))
                .collect(),
        }
    }

    fn next_row_id(&self) -> usize {
        self.rows.iter().map(|(i, _)| *i).max().map_or(0, |m| m + 1)
    }

    fn set_cell(&mut self, id: usize, col: usize, v: &str) -> bool {
        match self.rows.iter_mut().find(|(i, _)| *i == id) {
            Some((_, f)) if col < f.len() => {
                f[col] = v.to_string();
                true
            }
            _ => false,
        }
    }

    fn remove_row(&mut self, pos: usize) -> Option<(usize, Vec<String>)> {
        (pos < self.rows.len()).then(|| self.rows.remove(pos))
    }

    fn insert_row(&mut self, pos: usize, row: (usize, Vec<String>)) {
        self.rows.insert(pos.min(self.rows.len()), row);
    }

    fn remove_column(&mut self, col: usize) -> Vec<(usize, String)> {
        let mut out = vec![];
        for (id, f) in self.rows.iter_mut() {
            if col < f.len() {
                out.push((*id, f.remove(col)));
            }
        }
        out
    }

    /// Exact inverse of `remove_column`: only rows that had a value get one back.
    fn restore_column(&mut self, col: usize, values: &[(usize, String)]) {
        for (id, f) in self.rows.iter_mut() {
            if let Some((_, v)) = values.iter().find(|(i, _)| i == id) {
                while f.len() < col {
                    f.push(String::new());
                }
                f.insert(col, v.clone());
            }
        }
    }

    /// Returns the old length of every row that was padded (shorter than `col`).
    fn insert_column(&mut self, col: usize, values: &[(usize, String)]) -> Vec<(usize, usize)> {
        let mut padded = vec![];
        for (id, f) in self.rows.iter_mut() {
            let v = values
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            if f.len() < col {
                padded.push((*id, f.len()));
            }
            while f.len() < col {
                f.push(String::new());
            }
            f.insert(col, v);
        }
        padded
    }

    fn undo_insert_column(&mut self, col: usize, padded: &[(usize, usize)]) {
        for (id, f) in self.rows.iter_mut() {
            if col < f.len() {
                f.remove(col);
            }
            if let Some((_, old_len)) = padded.iter().find(|(i, _)| i == id) {
                f.truncate(*old_len);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    SetCell {
        pick: usize,
        col: usize,
        value: String,
    },
    SetCellMissingRow {
        col: usize,
    },
    RemoveRow {
        position: usize,
    },
    InsertRow {
        position: usize,
        fields: Vec<String>,
    },
    ReinsertRemovedRow {
        position: usize,
    },
    RemoveColumn {
        col: usize,
    },
    InsertColumn {
        col: usize,
        with_values: bool,
    },
    /// Insert a column and immediately undo it: from any state this must leave
    /// the sheet exactly as it was.
    InsertThenUndo {
        col: usize,
        with_values: bool,
    },
    /// Remove a column and immediately restore it: likewise a no-op overall.
    RemoveThenRestore {
        col: usize,
    },
    /// Undo the most recent `RemoveColumn` or `InsertColumn`; only applied when
    /// nothing but snapshots happened since (the undo-stack precondition).
    RestoreColumn,
    Snapshot,
}

fn text() -> impl Strategy<Value = String> {
    "[a-c0-9,\" \\n]{0,4}"
}

pub fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        3 => (any::<usize>(), 0usize..5, text()).prop_map(|(pick, col, value)| Op::SetCell { pick, col, value }),
        1 => (0usize..5).prop_map(|col| Op::SetCellMissingRow { col }),
        2 => (0usize..8).prop_map(|position| Op::RemoveRow { position }),
        2 => (0usize..8, prop::collection::vec(text(), 0..4))
            .prop_map(|(position, fields)| Op::InsertRow { position, fields }),
        2 => (0usize..8).prop_map(|position| Op::ReinsertRemovedRow { position }),
        2 => (0usize..5).prop_map(|col| Op::RemoveColumn { col }),
        2 => (0usize..6, any::<bool>()).prop_map(|(col, with_values)| Op::InsertColumn { col, with_values }),
        2 => Just(Op::RestoreColumn),
        2 => (0usize..7, any::<bool>()).prop_map(|(col, with_values)| Op::InsertThenUndo { col, with_values }),
        2 => (0usize..7).prop_map(|col| Op::RemoveThenRestore { col }),
        1 => Just(Op::Snapshot),
    ];
    prop::collection::vec(op, 0..40)
}

/// Only the ops a backend with just `set_cell` can do.
pub fn cell_edit_ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        4 => (any::<usize>(), 0usize..5, text()).prop_map(|(pick, col, value)| Op::SetCell { pick, col, value }),
        1 => (0usize..5).prop_map(|col| Op::SetCellMissingRow { col }),
        1 => Just(Op::Snapshot),
    ];
    prop::collection::vec(op, 0..40)
}

/// Cell edits plus row insert/remove/reinsert.
pub fn row_ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        3 => (any::<usize>(), 0usize..5, text()).prop_map(|(pick, col, value)| Op::SetCell { pick, col, value }),
        1 => (0usize..5).prop_map(|col| Op::SetCellMissingRow { col }),
        3 => (0usize..8).prop_map(|position| Op::RemoveRow { position }),
        3 => (0usize..8, prop::collection::vec(text(), 1..4))
            .prop_map(|(position, fields)| Op::InsertRow { position, fields }),
        3 => (0usize..8).prop_map(|position| Op::ReinsertRemovedRow { position }),
        1 => Just(Op::Snapshot),
    ];
    prop::collection::vec(op, 0..50)
}

pub fn initial_rows() -> impl Strategy<Value = Vec<Vec<String>>> {
    prop::collection::vec(prop::collection::vec(text(), 1..4), 0..6)
}

pub fn to_sheet_vec(rows: &[Vec<String>]) -> SheetVec {
    rows.iter()
        .enumerate()
        .map(|(i, f)| (i, f.iter().map(String::as_str).collect()))
        .collect()
}

/// Panics unless `sheet` and `model` hold identical contents.
pub fn assert_same(sheet: &dyn Sheet, model: &RefModel, ctx: &str) {
    assert_eq!(sheet.len(), model.rows.len(), "len after {ctx}");
    assert_eq!(
        sheet.next_row_id(),
        model.next_row_id(),
        "next_row_id after {ctx}"
    );
    for (pos, (id, f)) in model.rows.iter().enumerate() {
        assert_eq!(sheet.row_id(pos), Some(*id), "row_id({pos}) after {ctx}");
        assert_eq!(
            sheet.position_of(*id),
            Some(pos),
            "position_of({id}) after {ctx}"
        );
        assert_eq!(
            fields(sheet, pos).as_ref(),
            Some(f),
            "record({pos}) after {ctx}"
        );
        for c in 0..f.len() + 2 {
            assert_eq!(
                sheet.cell(pos, c).as_deref(),
                f.get(c).map(String::as_str),
                "cell({pos},{c}) after {ctx}"
            );
        }
    }
    assert_eq!(
        sheet.row_id(model.rows.len()),
        None,
        "row_id past end after {ctx}"
    );
    assert_eq!(
        sheet.position_of(model.next_row_id() + 100),
        None,
        "position_of missing after {ctx}"
    );
}

/// The last column operation, while it can still be undone exactly.
enum Undoable {
    Removed(usize, RemovedColumn, Vec<(usize, String)>),
    Inserted(usize, InsertedColumn, Vec<(usize, usize)>),
}

/// Apply `ops` to a backend built from `initial` and to a [`RefModel`],
/// requiring agreement on contents and on every return value.
pub fn check_ops(make: &Make, initial: &[Vec<String>], ops: &[Op]) {
    let mut sheet = make(to_sheet_vec(initial));
    let mut model = RefModel::from_rows(&to_sheet_vec(initial));
    let mut removed_rows: Vec<(usize, Vec<String>)> = vec![];
    // The most recent column removal, as the model saw it (for arbitrary
    // `insert_column` values), and whether it can still be undone exactly.
    let mut last_values: Vec<(usize, String)> = vec![];
    let mut undoable: Option<Undoable> = None;
    let mut snapshots: Vec<(Box<dyn Sheet>, RefModel)> = vec![];
    assert_same(sheet.as_ref(), &model, "load");

    for (n, op) in ops.iter().enumerate() {
        let ctx = format!("op #{n} {op:?}");
        match op {
            Op::SetCell { pick, col, value } => {
                let Some(id) =
                    (!model.rows.is_empty()).then(|| model.rows[pick % model.rows.len()].0)
                else {
                    continue;
                };
                assert_eq!(
                    sheet.set_cell(id, *col, value),
                    model.set_cell(id, *col, value),
                    "{ctx}"
                );
            }
            Op::SetCellMissingRow { col } => {
                let id = model.next_row_id() + 50;
                assert!(!sheet.set_cell(id, *col, "x"), "{ctx}");
            }
            Op::RemoveRow { position } => {
                let got = sheet
                    .remove_row(*position)
                    .map(|(i, r)| (i, r.iter().map(str::to_string).collect::<Vec<_>>()));
                let want = model.remove_row(*position);
                assert_eq!(got, want, "{ctx}");
                removed_rows.extend(want);
            }
            Op::InsertRow { position, fields } => {
                let id = model.next_row_id();
                let rec: StringRecord = fields.iter().map(String::as_str).collect();
                sheet.insert_row(*position, (id, rec));
                model.insert_row(*position, (id, fields.clone()));
            }
            Op::ReinsertRemovedRow { position } => {
                let Some((id, f)) = removed_rows.pop() else {
                    continue;
                };
                if model.rows.iter().any(|(i, _)| *i == id) {
                    continue; // id was handed out again; an undo can't hit this
                }
                let rec: StringRecord = f.iter().map(String::as_str).collect();
                sheet.insert_row(*position, (id, rec));
                model.insert_row(*position, (id, f));
            }
            Op::RemoveColumn { col } => {
                let token = sheet.remove_column(*col);
                let values = model.remove_column(*col);
                last_values = values.clone();
                undoable = Some(Undoable::Removed(*col, token, values));
            }
            Op::InsertColumn { col, with_values } => {
                let values: &[(usize, String)] = if *with_values { &last_values } else { &[] };
                let token = sheet.insert_column(*col, values);
                let padded = model.insert_column(*col, values);
                undoable = Some(Undoable::Inserted(*col, token, padded));
            }
            Op::InsertThenUndo { col, with_values } => {
                let values: &[(usize, String)] = if *with_values { &last_values } else { &[] };
                let before = model.clone();
                let token = sheet.insert_column(*col, values);
                let padded = model.insert_column(*col, values);
                sheet.undo_insert_column(*col, &token);
                model.undo_insert_column(*col, &padded);
                assert_eq!(
                    model, before,
                    "the model's own insert/undo is an exact inverse: {ctx}"
                );
            }
            Op::RemoveThenRestore { col } => {
                let before = model.clone();
                let token = sheet.remove_column(*col);
                let values = model.remove_column(*col);
                sheet.restore_column(*col, &token);
                model.restore_column(*col, &values);
                assert_eq!(
                    model, before,
                    "the model's own remove/restore is an exact inverse: {ctx}"
                );
            }
            Op::RestoreColumn => match undoable.take() {
                Some(Undoable::Removed(col, token, values)) => {
                    sheet.restore_column(col, &token);
                    model.restore_column(col, &values);
                }
                Some(Undoable::Inserted(col, token, padded)) => {
                    sheet.undo_insert_column(col, &token);
                    model.undo_insert_column(col, &padded);
                }
                None => {}
            },
            Op::Snapshot => snapshots.push((sheet.snapshot(), model.clone())),
        }
        if !matches!(
            op,
            Op::Snapshot | Op::RemoveColumn { .. } | Op::RestoreColumn
        ) {
            undoable = None; // something else changed the sheet: no longer an exact undo
        }
        assert_same(sheet.as_ref(), &model, &ctx);
    }
    for (i, (snap, frozen)) in snapshots.iter().enumerate() {
        assert_same(
            snap.as_ref(),
            frozen,
            &format!("snapshot #{i} checked at the end"),
        );
    }
}

/// Filter and sort must give exactly what [`crate::MemorySheet`] gives on the
/// same rows. `needle` is taken from the data half the time so it matches.
pub fn check_filter_sort(
    make: &Make,
    initial: &[Vec<String>],
    needle: &str,
    col: usize,
    dsc: bool,
) {
    let reference = crate::MemorySheet::new(to_sheet_vec(initial));
    let sheet = make(to_sheet_vec(initial));
    assert_eq!(
        sheet.filter(needle),
        reference.filter(needle),
        "filter({needle:?})"
    );
    let order = if dsc { SortOrder::Dsc } else { SortOrder::Asc };
    let all: Vec<u32> = (0..initial.len() as u32).collect();
    let subset: Vec<u32> = all.iter().copied().filter(|i| i % 2 == 0).collect();
    for view in [all, subset] {
        assert_eq!(
            sheet.sort(view.clone(), (col, order)),
            reference.sort(view.clone(), (col, order)),
            "sort col {col} {order:?} of {view:?}"
        );
    }
}
