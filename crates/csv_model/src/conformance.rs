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

use crate::{Sheet, SheetRow, SheetVec, SortOrder};

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
    set_cell_by_id(make);
    remove_and_reinsert_row_restores_order_and_id(make);
    insert_row_clamps_to_end(make);
    column_remove_insert_round_trip(make);
    snapshot_is_isolated_from_later_edits(make);
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
    let removed = s.remove_column(1);
    assert_eq!(removed, vec![(0, "b".to_string()), (1, "e".to_string())]);
    assert_eq!(fields(s.as_ref(), 0), Some(vec!["a".into(), "c".into()]));
    assert_eq!(
        fields(s.as_ref(), 2),
        Some(vec!["g".into()]),
        "short row untouched"
    );
    s.insert_column(1, &removed);
    assert_eq!(
        fields(s.as_ref(), 1),
        Some(vec!["d".into(), "e".into(), "f".into()])
    );
    s.insert_column(0, &[]);
    assert_eq!(
        fields(s.as_ref(), 0),
        Some(vec!["".into(), "a".into(), "b".into(), "c".into()])
    );
    s.insert_column(99, &[(0, "end".to_string())]);
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

    fn insert_column(&mut self, col: usize, values: &[(usize, String)]) {
        for (id, f) in self.rows.iter_mut() {
            let v = values
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            f.insert(col.min(f.len()), v);
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
        restore: bool,
    },
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
        2 => (0usize..6, any::<bool>()).prop_map(|(col, restore)| Op::InsertColumn { col, restore }),
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

/// Apply `ops` to a backend built from `initial` and to a [`RefModel`],
/// requiring agreement on contents and on every return value.
pub fn check_ops(make: &Make, initial: &[Vec<String>], ops: &[Op]) {
    let mut sheet = make(to_sheet_vec(initial));
    let mut model = RefModel::from_rows(&to_sheet_vec(initial));
    let mut removed_rows: Vec<(usize, Vec<String>)> = vec![];
    let mut removed_col: Vec<(usize, String)> = vec![];
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
                let got = sheet.remove_column(*col);
                let want = model.remove_column(*col);
                assert_eq!(got, want, "{ctx}");
                removed_col = want;
            }
            Op::InsertColumn { col, restore } => {
                let values: &[(usize, String)] = if *restore { &removed_col } else { &[] };
                sheet.insert_column(*col, values);
                model.insert_column(*col, values);
            }
            Op::Snapshot => snapshots.push((sheet.snapshot(), model.clone())),
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
