//! Shared test infrastructure for the budget tests (see
//! docs/design/large-file-engine.md, sections 3 and 7).
//!
//! * [`CountingAlloc`] - a global allocator that counts bytes, so memory
//!   budgets are asserted in exact bytes and do not depend on the machine.
//! * [`Instrumented`] - a `ByteSource` that counts reads and can inject
//!   failures, so I/O budgets ("a window costs <= 2 reads, independent of file
//!   size") are exact too.
//! * [`csv_bytes`] - a deterministic synthetic CSV generator.
//! * [`check_le`]/[`check_ge`] - assert a budget and print a table line.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

use csv_model::ByteSource;

// ------------------------------------------------------------- allocation

pub struct CountingAlloc;

// Process-wide counters, and per-thread ones. `measure` uses the per-thread
// counters so that other threads (the test harness, parallel tests) cannot
// disturb a measurement; `measure_all_threads` uses the global ones for code
// that fans out to worker threads. Thread-locals with const init never
// allocate, which a global allocator requires.
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

// ------------------------------------------------------------------- I/O

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoStats {
    pub calls: u64,
    pub bytes: u64,
}

/// In-memory file that counts what is read from it and can start failing.
pub struct Instrumented {
    data: Vec<u8>,
    calls: AtomicU64,
    bytes: AtomicU64,
    fail_from: AtomicU64,
}

impl Instrumented {
    pub fn new(data: Vec<u8>) -> Self {
        Instrumented {
            data,
            calls: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            fail_from: AtomicU64::new(u64::MAX),
        }
    }

    pub fn stats(&self) -> IoStats {
        IoStats {
            calls: self.calls.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
        }
    }

    pub fn reset(&self) {
        self.calls.store(0, Relaxed);
        self.bytes.store(0, Relaxed);
    }

    /// Reads at or after `offset` fail from now on (a file that shrank, a
    /// disk that went away).
    pub fn fail_reads_from(&self, offset: u64) {
        self.fail_from.store(offset, Relaxed);
    }

    pub fn data_len(&self) -> u64 {
        self.data.len() as u64
    }
}

impl ByteSource for Instrumented {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.calls.fetch_add(1, Relaxed);
        if offset >= self.fail_from.load(Relaxed) {
            return Err(io::Error::other("injected read failure"));
        }
        let start = (offset as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.bytes.fetch_add(n as u64, Relaxed);
        Ok(n)
    }
}

// -------------------------------------------------------------- generator

/// Deterministic synthetic CSV, ~76 bytes/row, 8 fields, no header:
/// `id,user<id>,city<n>,<amount>,<date>,<0|1>,"note, with comma <id>",x<n>`.
/// Column 3 is numeric (a "money" value), column 1 is text with a shared
/// prefix. Same shape as the measurement spikes, so results compare.
pub fn csv_bytes(rows: usize) -> Vec<u8> {
    use std::fmt::Write;
    let mut s = String::with_capacity(rows * 80);
    for i in 0..rows {
        write!(
            s,
            "{i},user{i},city{},{}.{:02},2024-{:02}-{:02},{},\"note, with comma {i}\",x{}\n",
            i % 97,
            i % 1000,
            i % 100,
            1 + i % 12,
            1 + i % 28,
            i % 2,
            i % 7
        )
        .unwrap();
    }
    s.into_bytes()
}

// ---------------------------------------------------------------- budgets

/// Assert `measured <= limit`, printing a table line either way.
pub fn check_le(name: &str, measured: f64, limit: f64, unit: &str) {
    let ok = measured <= limit;
    eprintln!(
        "BUDGET {name:<58} {measured:>14.3} {unit:<10} limit <= {limit:<12.3} {}",
        if ok { "PASS" } else { "FAIL" }
    );
    assert!(
        ok,
        "{name}: {measured:.3} {unit} exceeds the budget of {limit:.3} {unit}"
    );
}

/// Assert `measured >= limit`, printing a table line either way.
pub fn check_ge(name: &str, measured: f64, limit: f64, unit: &str) {
    let ok = measured >= limit;
    eprintln!(
        "BUDGET {name:<58} {measured:>14.3} {unit:<10} limit >= {limit:<12.3} {}",
        if ok { "PASS" } else { "FAIL" }
    );
    assert!(
        ok,
        "{name}: {measured:.3} {unit} is below the budget of {limit:.3} {unit}"
    );
}

/// Fastest of `runs` timings of `f`, in seconds. Wall-clock timings on a
/// shared machine are noisy (the same code ranged 3x between runs in the
/// cloud sandbox); the minimum is the best estimate of what the code costs.
pub fn best_secs(runs: usize, mut f: impl FnMut()) -> f64 {
    (0..runs)
        .map(|_| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min)
}

/// How fast this machine is compared with the reference machine the
/// throughput budgets were set on (7.6 GB/s raw search). Slower machines get
/// proportionally relaxed budgets; faster ones are not made stricter.
pub fn perf_scale() -> f64 {
    let buf = csv_bytes(300_000);
    let finder = memchr::memmem::Finder::new("zzzzzzzz");
    let _ = finder.find_iter(&buf).count(); // warm
    let mut n = 0;
    let secs = best_secs(7, || n += finder.find_iter(&buf).count());
    let gbps = buf.len() as f64 / 1e9 / secs;
    assert_eq!(n, 0);
    (gbps / 7.6).min(1.0)
}
