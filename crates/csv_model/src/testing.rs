//! Support for tests and benchmarks, here so the app's tests and this crate's
//! integration tests can share it: a virtual CSV file of any size, and an
//! allocation counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use crate::ByteSource;

// ----------------------------------------------------------- virtual file

/// Every row of a [`SyntheticSource`] is exactly this many bytes.
pub const SYNTHETIC_ROW_BYTES: u64 = 80;

/// A CSV file that exists only as a formula: row `i` is computed when its bytes
/// are read. Lets a test open a 5 GB file with no disk and no generation time.
/// Rows are fixed-width (80 bytes, 8 fields, one quoted field containing a
/// comma, no header): the same shape as the measurement spikes.
pub struct SyntheticSource {
    rows: u64,
}

impl SyntheticSource {
    pub fn new(rows: u64) -> Self {
        SyntheticSource { rows }
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The text of row `i`, including its newline.
    pub fn row_text(i: u64) -> [u8; 80] {
        use std::io::Write;
        let mut row = [0u8; 80];
        let mut out = &mut row[..];
        write!(
            out,
            "{i:010},user{i:010},city{:03},{:010.2},2024-{:02}-{:02},{},\"note, {i:010}\",x{}\n",
            i % 97,
            (i % 1_000_000) as f64 / 100.0,
            1 + i % 12,
            1 + i % 28,
            i % 2,
            i % 7
        )
        .expect("a row fits in 80 bytes");
        assert!(out.is_empty(), "row {i} is not exactly 80 bytes");
        row
    }
}

impl ByteSource for SyntheticSource {
    fn len(&self) -> u64 {
        self.rows * SYNTHETIC_ROW_BYTES
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let (len, mut at, mut done) = (self.len(), offset, 0);
        while done < buf.len() && at < len {
            let (row, within) = (
                at / SYNTHETIC_ROW_BYTES,
                (at % SYNTHETIC_ROW_BYTES) as usize,
            );
            let text = Self::row_text(row);
            let n = (80 - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&text[within..within + n]);
            done += n;
            at += n as u64;
        }
        Ok(done)
    }
}

// ------------------------------------------------------ counting allocator

/// A global allocator that counts bytes, so memory can be asserted exactly
/// instead of read off the OS. Declare it in the test binary:
/// `#[global_allocator] static A: csv_model::testing::CountingAlloc = ...;`
pub struct CountingAlloc;

// Process-wide counters, and per-thread ones. `measure` uses the per-thread
// counters so other threads (the test harness, parallel tests) cannot disturb
// a measurement; `measure_all_threads` uses the global ones for code that fans
// out to worker threads. Thread-locals with const init never allocate, which a
// global allocator requires.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static T_LIVE: Cell<isize> = const { Cell::new(0) };
    static T_TOTAL: Cell<usize> = const { Cell::new(0) };
    static T_PEAK: Cell<isize> = const { Cell::new(0) };
}

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    TOTAL.fetch_add(by, Relaxed);
    PEAK.fetch_max(live, Relaxed);
    let _ = T_LIVE.try_with(|l| {
        l.set(l.get() + by as isize);
        let _ = T_PEAK.try_with(|p| p.set(p.get().max(l.get())));
    });
    let _ = T_TOTAL.try_with(|t| t.set(t.get() + by));
}

fn shrank(by: usize) {
    LIVE.fetch_sub(by, Relaxed);
    let _ = T_LIVE.try_with(|l| l.set(l.get() - by as isize));
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grew(l.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grew(l.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        shrank(l.size());
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new_size) };
        if !q.is_null() {
            if new_size >= l.size() {
                grew(new_size - l.size());
            } else {
                shrank(l.size() - new_size);
            }
        }
        q
    }
}

/// Tests that time things or measure the whole process must not overlap with
/// each other. Hold this for the whole test.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Copy)]
pub struct Mem {
    /// Bytes still allocated after `f` returned, minus before (includes the
    /// returned value).
    pub retained: i64,
    /// Total bytes requested from the allocator during `f`.
    pub allocated: usize,
    /// Highest live bytes reached during `f`, above the starting level.
    pub peak: usize,
}

/// Run `f` and report the allocation behaviour of the *calling thread*.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Mem) {
    let live0 = T_LIVE.with(|c| c.get());
    let total0 = T_TOTAL.with(|c| c.get());
    T_PEAK.with(|p| p.set(live0));
    let r = f();
    let mem = Mem {
        retained: (T_LIVE.with(|c| c.get()) - live0) as i64,
        allocated: T_TOTAL.with(|c| c.get()) - total0,
        peak: (T_PEAK.with(|c| c.get()) - live0).max(0) as usize,
    };
    (r, mem)
}

/// Like [`measure`], but counts every thread. Hold [`serial`] and expect a
/// little noise from the test harness.
pub fn measure_all_threads<R>(f: impl FnOnce() -> R) -> (R, Mem) {
    let live0 = LIVE.load(Relaxed);
    let total0 = TOTAL.load(Relaxed);
    PEAK.store(live0, Relaxed);
    let r = f();
    let mem = Mem {
        retained: LIVE.load(Relaxed) as i64 - live0 as i64,
        allocated: TOTAL.load(Relaxed) - total0,
        peak: PEAK.load(Relaxed).saturating_sub(live0),
    };
    (r, mem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_80_bytes_and_parses_to_the_documented_fields() {
        for i in [
            0u64,
            1,
            96,
            97,
            999_999,
            1_000_000,
            12_345_678,
            62_499_999,
            9_999_999_999,
        ] {
            let text = SyntheticSource::row_text(i);
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(&text[..]);
            let rec = rdr.records().next().unwrap().unwrap();
            assert_eq!(rec.len(), 8, "row {i}");
            assert_eq!(rec[0].parse::<u64>().unwrap(), i);
            assert_eq!(&rec[1], format!("user{i:010}"));
            assert_eq!(
                &rec[6],
                format!("note, {i:010}"),
                "the quoted field contains a comma"
            );
        }
    }

    #[test]
    fn reads_at_any_offset_match_the_concatenated_rows() {
        let src = SyntheticSource::new(50);
        let mut whole = Vec::new();
        for i in 0..50 {
            whole.extend_from_slice(&SyntheticSource::row_text(i));
        }
        assert_eq!(src.len() as usize, whole.len());
        for (offset, len) in [
            (0u64, 80usize),
            (79, 3),
            (1, 4000),
            (3999, 100),
            (3990, 500),
            (4000, 10),
        ] {
            let mut buf = vec![0u8; len];
            let n = src.read_at(offset, &mut buf).unwrap();
            let want = &whole
                [(offset as usize).min(whole.len())..(offset as usize + len).min(whole.len())];
            assert_eq!(&buf[..n], want, "offset {offset} len {len}");
        }
    }

    #[test]
    fn a_five_gigabyte_file_has_the_expected_length_without_any_memory() {
        let src = SyntheticSource::new(62_500_000);
        assert_eq!(src.len(), 5_000_000_000);
        let mut buf = [0u8; 160];
        assert_eq!(
            src.read_at(src.len() - 80, &mut buf).unwrap(),
            80,
            "short read at the end"
        );
        assert_eq!(src.read_at(src.len(), &mut buf).unwrap(), 0);
    }
}
