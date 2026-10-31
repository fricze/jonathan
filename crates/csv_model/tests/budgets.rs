//! Budget tests for the large-file engine (docs/design/large-file-engine.md,
//! sections 3 and 7).
//!
//! Three kinds of test live here:
//!
//! * plain `#[test]` - must always pass. Harness self-tests, and budgets the
//!   current code already meets (regression guards).
//! * `#[ignore = "pending: ..."]` - a budget the design requires and the
//!   current code does **not** meet. They are red on purpose; the reason says
//!   what must land first. When it lands, remove the `ignore` in that commit.
//! * `#[ignore = "perf: ..."]` - machine-speed budgets; run in release mode.
//!
//! Run everything and see the table of measured values against limits:
//!
//!     scripts/budgets.sh
//!
//! or, by hand:
//!
//!     cargo test -p csv_model --release --test budgets -- --ignored --nocapture

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::*;
use csv_model::{ByteSource, FileSheet, RowIndex, Sheet, SortOrder};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn open(bytes: Vec<u8>) -> (FileSheet, Arc<Instrumented>) {
    let src = Arc::new(Instrumented::new(bytes));
    let sheet = FileSheet::new(src.clone(), b',', false).unwrap();
    (sheet, src)
}

// ------------------------------------------------- harness self-tests

#[test]
fn generator_is_deterministic_and_has_the_documented_shape() {
    let _g = serial();
    let a = csv_bytes(1000);
    assert_eq!(a, csv_bytes(1000));
    let rows: Vec<csv::StringRecord> =
        csv::ReaderBuilder::new().has_headers(false).from_reader(&a[..]).records().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 1000);
    assert!(rows.iter().all(|r| r.len() == 8));
    assert_eq!(&rows[7][1], "user7");
    assert_eq!(&rows[7][6], "note, with comma 7");
    let avg = a.len() as f64 / 1000.0;
    assert!((60.0..95.0).contains(&avg), "about 76 B/row, got {avg}");
}

#[test]
fn counting_allocator_sees_known_allocations_exactly() {
    let _g = serial();
    let (v, m) = measure(|| vec![0u8; 1 << 20]);
    assert_eq!(m.retained, 1 << 20);
    assert!(m.allocated >= 1 << 20 && m.allocated < (1 << 20) + 4096);
    drop(v);
    let (_, m) = measure(|| drop(vec![1u8; 1 << 20]));
    assert_eq!(m.retained, 0, "freed memory is not retained");
    assert!(m.peak >= 1 << 20, "peak records the transient");
    let (_, m) = measure(|| {});
    assert_eq!((m.retained, m.allocated), (0, 0));
}

#[test]
fn counting_allocator_follows_growing_vectors() {
    let _g = serial();
    let (v, m) = measure(|| {
        let mut v = Vec::new();
        for i in 0..100_000u32 {
            v.push(i);
        }
        v
    });
    assert_eq!(m.retained as usize, v.capacity() * 4, "realloc growth is tracked, not double counted");
}

#[test]
fn instrumented_source_counts_reads_and_injects_failures() {
    let _g = serial();
    let src = Instrumented::new(vec![7u8; 100]);
    let mut buf = [0u8; 40];
    assert_eq!(src.read_at(0, &mut buf).unwrap(), 40);
    assert_eq!(src.read_at(80, &mut buf).unwrap(), 20, "short read at the end");
    assert_eq!(src.stats(), IoStats { calls: 2, bytes: 60 });
    src.reset();
    assert_eq!(src.stats(), IoStats { calls: 0, bytes: 0 });
    src.fail_reads_from(50);
    assert!(src.read_at(0, &mut buf).is_ok());
    assert!(src.read_at(50, &mut buf).is_err());
    assert_eq!(src.stats().calls, 2, "failed reads are still calls");
}

#[test]
fn a_filesheet_over_the_instrumented_source_reads_what_it_should() {
    let _g = serial();
    let (sheet, src) = open(csv_bytes(1000));
    assert_eq!(sheet.len(), 1000);
    assert_eq!(src.stats().bytes, src.data_len(), "indexing reads every byte once");
    src.reset();
    assert_eq!(sheet.cell(7, 1).as_deref(), Some("user7"));
    assert!(src.stats().calls >= 1);
}

// ------------------------------------------------ memory (exact bytes)

#[test]
fn memory_edits_cost_is_proportional_to_edits_not_file_size() {
    let _g = serial();
    let mut growth = Vec::new();
    for n in [30_000usize, 120_000] {
        let (mut sheet, _src) = open(csv_bytes(n));
        let (_, m) = measure(|| {
            for k in 0..1000 {
                assert!(sheet.set_cell(k * (n / 1000), 1, "edited text"));
            }
        });
        check_le(&format!("1000 cell edits, retained bytes (file {n} rows)"), m.retained as f64, 1000.0 * 1024.0, "bytes");
        growth.push(m.retained);
    }
    check_le("edit memory: 4x larger file costs <= 25% more", growth[1] as f64, growth[0] as f64 * 1.25 + 65536.0, "bytes");
}

#[test]
fn memory_filter_result_is_four_bytes_per_match() {
    let _g = serial();
    let (sheet, _src) = open(csv_bytes(100_000));
    let (view, m) = measure(|| sheet.filter("city9"));
    assert!(view.len() > 1000);
    check_le("filter view bytes per matching row", m.retained as f64 / view.len() as f64, 8.0, "B/match");
    check_le("filter view bytes per matching row (capacity)", (view.capacity() * 4) as f64 / view.len() as f64, 8.0, "B/match");
}

#[test]
fn memory_row_cache_never_exceeds_its_byte_cap() {
    let _g = serial();
    let (sheet, _src) = open(csv_bytes(100_000));
    let (_, m) = measure(|| {
        for p in (0..100_000).step_by(3) {
            let _ = sheet.cell(p, 1);
        }
    });
    check_le("row cache after reading 33k distinct rows", m.retained as f64, 32.0 * 1024.0 * 1024.0, "bytes");
}

#[test]
#[ignore = "pending: sparse index (design 4.1)"]
fn memory_index_is_at_most_a_quarter_byte_per_row() {
    let _g = serial();
    for n in [100_000usize, 500_000] {
        let bytes = csv_bytes(n);
        let (idx, m) = measure(|| RowIndex::build(&bytes, b',', false));
        assert_eq!(idx.len(), n);
        check_le(&format!("index retained bytes per row ({n} rows)"), (m.retained as f64 - 4096.0) / n as f64, 0.25, "B/row");
    }
}

#[test]
#[ignore = "pending: persistent piece table (design 4.2)"]
fn memory_row_insert_and_remove_do_not_scale_with_file_size() {
    let _g = serial();
    let mut cost = Vec::new();
    for n in [30_000usize, 120_000] {
        let (mut sheet, _src) = open(csv_bytes(n));
        let (_, m) = measure(|| {
            for k in 0..100 {
                let row = sheet.remove_row(n / 2 + k).unwrap();
                sheet.insert_row(k, (row.0, row.1));
            }
        });
        check_le(&format!("100 row remove+insert, retained bytes (file {n} rows)"), m.retained as f64, 256.0 * 1024.0, "bytes");
        cost.push(m.retained);
    }
    check_le("row-edit memory: 4x larger file costs <= 25% more", cost[1] as f64, cost[0] as f64 * 1.25 + 65536.0, "bytes");
}

#[test]
#[ignore = "pending: persistent overlay map (design 4.3/4.9)"]
fn memory_snapshot_cost_does_not_grow_with_the_number_of_edits() {
    let _g = serial();
    let mut allocated = Vec::new();
    for edits in [10usize, 10_000] {
        let (mut sheet, _src) = open(csv_bytes(50_000));
        for k in 0..edits {
            sheet.set_cell(k, 1, "x");
        }
        let (snap, m) = measure(|| sheet.snapshot());
        drop(snap);
        check_le(&format!("snapshot() bytes allocated ({edits} edits)"), m.allocated as f64, 4096.0, "bytes");
        allocated.push(m.allocated);
    }
    assert!(allocated[1] <= allocated[0] + 1024);
}

// ------------------------------------------ I/O complexity (exact reads)

#[test]
fn io_indexing_reads_every_byte_exactly_once() {
    let _g = serial();
    let (_sheet, src) = open(csv_bytes(100_000));
    check_le("index build: bytes read / file length", src.stats().bytes as f64 / src.data_len() as f64, 1.001, "ratio");
}

#[test]
fn io_a_cell_edit_reads_at_most_the_one_row() {
    let _g = serial();
    for n in [20_000usize, 100_000] {
        let (mut sheet, src) = open(csv_bytes(n));
        src.reset();
        assert!(sheet.set_cell(n / 2, 1, "new"));
        check_le(&format!("cell edit: read calls (file {n} rows)"), src.stats().calls as f64, 2.0, "calls");
        check_le(&format!("cell edit: bytes read (file {n} rows)"), src.stats().bytes as f64, 64.0 * 1024.0, "bytes");
    }
}

#[test]
fn io_random_row_access_reads_at_most_two_blocks() {
    let _g = serial();
    for n in [20_000usize, 100_000] {
        let (sheet, src) = open(csv_bytes(n));
        for p in [3, n / 3, n / 2 + 5, n - 2] {
            src.reset();
            let _ = sheet.cell(p, 1);
            let s = src.stats();
            check_le(&format!("random row {p}: read calls (file {n} rows)"), s.calls as f64, 2.0, "calls");
            check_le(&format!("random row {p}: bytes read (file {n} rows)"), s.bytes as f64, 2.0 * 1024.0 * 1024.0, "bytes");
        }
    }
}

#[test]
#[ignore = "pending: block reads (design 4.5)"]
fn io_a_visible_window_costs_a_couple_of_reads_independent_of_file_size() {
    let _g = serial();
    let mut calls = Vec::new();
    for n in [20_000usize, 100_000] {
        let (sheet, src) = open(csv_bytes(n));
        src.reset();
        for p in n / 2..n / 2 + 60 {
            for c in 0..8 {
                let _ = sheet.cell(p, c);
            }
        }
        let s = src.stats();
        check_le(&format!("60-row window: read calls (file {n} rows)"), s.calls as f64, 2.0, "calls");
        check_le(&format!("60-row window: bytes read (file {n} rows)"), s.bytes as f64, 2.0 * 1024.0 * 1024.0, "bytes");
        calls.push(s.calls);
    }
    assert_eq!(calls[0], calls[1], "same cost on a 10x larger file");
}

#[test]
#[ignore = "pending: column map + undo token (design 4.4)"]
fn io_deleting_a_column_never_touches_the_file() {
    let _g = serial();
    for n in [20_000usize, 100_000] {
        let (mut sheet, src) = open(csv_bytes(n));
        src.reset();
        let _ = sheet.remove_column(2);
        check_le(&format!("column delete: read calls (file {n} rows)"), src.stats().calls as f64, 0.0, "calls");
    }
}

#[test]
#[ignore = "pending: block reads + scanner (design 4.5)"]
fn io_a_no_match_filter_reads_each_byte_once_in_large_blocks() {
    let _g = serial();
    let (sheet, src) = open(csv_bytes(100_000));
    let len = src.data_len() as f64;
    src.reset();
    assert!(sheet.filter("zzzzzzzz").is_empty());
    let s = src.stats();
    check_le("no-match filter: bytes read / file length", s.bytes as f64 / len, 1.01, "ratio");
    check_le("no-match filter: read calls", s.calls as f64, len / (256.0 * 1024.0) + 4.0, "calls");
}

// ------------------------------------------------ performance (release)
//
// Timings use the best of several runs and are scaled by `perf_scale()`.

#[test]
#[ignore = "perf: run in release mode"]
fn perf_index_build_throughput() {
    let _g = serial();
    let scale = perf_scale();
    let bytes = csv_bytes(600_000);
    let mut rows = 0;
    let secs = best_secs(5, || rows = RowIndex::build(&bytes, b',', false).len());
    assert_eq!(rows, 600_000);
    check_ge("index build throughput", bytes.len() as f64 / 1e6 / secs, 400.0 * scale, "MB/s");
}

#[test]
#[ignore = "perf: run in release mode"]
fn perf_random_row_read() {
    let _g = serial();
    let scale = perf_scale();
    let n = 10_000;
    let (sheet, _src) = open(csv_bytes(300_000));
    let secs = best_secs(3, || {
        for k in 0..n {
            let _ = sheet.cell(k * 29 % 300_000, 1);
        }
    });
    check_le("random row read", secs * 1e6 / n as f64, 50.0 / scale, "us/row");
}

#[test]
#[ignore = "pending: lean parser + scanner (design 4.5)"]
fn perf_filter_parse_scan_rate() {
    let _g = serial();
    let scale = perf_scale();
    let n = 150_000;
    let (sheet, _src) = open(csv_bytes(n));
    let mut hits = 0;
    // no match, but every field of every row must be looked at
    let secs = best_secs(3, || hits = sheet.filter("user29999999").len());
    assert_eq!(hits, 0);
    check_ge("filter, general (parse) rate, 1 thread", n as f64 / 1e6 / secs, 5.0 * scale, "M rows/s");
}

#[test]
#[ignore = "pending: raw-buffer search prefilter (design 4.5)"]
fn perf_filter_raw_search_rate() {
    let _g = serial();
    let scale = perf_scale();
    let bytes = csv_bytes(300_000);
    let len = bytes.len() as f64;
    let (sheet, _src) = open(bytes);
    let mut hits = 0;
    let secs = best_secs(3, || hits = sheet.filter("zzzzzzzz").len());
    assert_eq!(hits, 0);
    check_ge("filter, needle without delimiter/quote, 1 thread", len / 1e9 / secs, 3.0 * scale, "GB/s");
}

#[test]
#[ignore = "pending: lean parser + scanner (design 4.5)"]
fn perf_sort_numeric_column() {
    let _g = serial();
    let scale = perf_scale();
    let n = 150_000;
    let (sheet, _src) = open(csv_bytes(n));
    let view: Vec<u32> = (0..n as u32).collect();
    let mut sorted = 0;
    let secs = best_secs(3, || sorted = sheet.sort(view.clone(), (3, SortOrder::Asc)).len());
    assert_eq!(sorted, n);
    check_ge("sort by numeric column, end to end, 1 thread", n as f64 / 1e6 / secs, 3.0 * scale, "M rows/s");
}

#[test]
#[ignore = "pending: lean parser (design 4.5)"]
fn perf_first_touch_of_a_visible_window() {
    let _g = serial();
    let scale = perf_scale();
    let bytes = csv_bytes(300_000);
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        // a fresh sheet each time, so nothing is cached
        let (sheet, _src) = open(bytes.clone());
        let t = Instant::now();
        for p in 150_000..150_060 {
            for c in 0..8 {
                let _ = sheet.cell(p, c);
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    check_le("60 rows x 8 cells, first touch", best * 1e3, 1.0 / scale, "ms");
}
