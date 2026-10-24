# Design: large-file engine for Jonathan

Status: proposal for review. Supersedes the step list in
`docs/plans/file-backed-sheet.md` (that plan was written test-first but without
budgets; see "What the existing code is and isn't").

## 1. Requirements

Jonathan is a CSV viewer/editor whose promise is *huge files, fast and
responsive*, with *high-quality editing* (cells stay exact text; unedited
bytes are saved untouched).

**Envelope (D2 decided).** Required: files up to **10 GB** (~130M rows at
~76 B/row) on a 16 GB / 4-core laptop with an SSD. Future size is unknown, so
the design must degrade gracefully, not fall over, up to ~50 GB (~650M rows).
Below ~256 MB the simple in-memory backend is fine and stays.

What that means at 10 GB (131M rows), from the measurements in section 2:

| Structure | Cost at 10 GB |
|---|---|
| Rows in memory as `StringRecord` (today's `MemorySheet`) | ~37 GB - impossible |
| Dense row index (8 B/row), current `FileSheet` | 1.05 GB |
| Order `Vec` after the first row insert/remove, current | +1.05 GB, ~100+ ms memmove per row edit |
| **Sparse index (1 per 64 rows)** | **16 MB** |
| **Piece table** | proportional to edits (KBs) |
| Full index build, one thread, warm | ~19 s (progressive, cancellable) |
| Lean full scan, one thread, warm | ~14 s; raw-buffer search ~1.3 s |
| Filter result view if every row matches (4 B/row) | 524 MB |
| Sort keys (16 B/row, transient) | 2.1 GB |
| Cold-cache read of the file | ~4 s at 2.5 GB/s (NVMe), ~70 s at 150 MB/s (HDD) |

**What the user must be able to do, and how it must feel:**

| # | Interaction | Requirement |
|---|---|---|
| U1 | Open a file of any size | First screen of rows visible in < 300 ms. App stays interactive while the rest is indexed. Row count and a progress indicator update live. |
| U2 | Scroll / jump anywhere in the indexed part | Every frame (16 ms) reads only the visible rows. Jumping to row N is instant. |
| U3 | Type a filter | Typing never blocks. First matches appear < 100 ms; total grows live; typing another character cancels the previous scan. |
| U4 | Sort by a column | Starts immediately, shows progress, cancellable, never freezes the UI. Numbers sort by value. |
| U5 | Edit a cell, insert/delete a row or a column | Instant (< 1 ms) at any file size. Undo/redo instant. |
| U6 | Save | Never loses data: a failure leaves the original intact. Progress shown. Unedited rows are byte-identical to the input. |
| U7 | File changes on disk while open, or disk errors | Detected and shown to the user. Never silently shown as empty rows. |
| U8 | Memory | Resident memory grows with *edits and views*, not with file size. |

## 2. Measurements this design rests on

One run each, release build, synthetic 227 MB / 3M rows / 76 B per row /
8 fields (one quoted with a comma), data already in RAM (warm), 4 vCPU
sandbox. Treat as order-of-magnitude, not benchmarks. Script:
`docs/design/spike.rs` (kept outside the build).

| Primitive | Result |
|---|---|
| Index scan with `csv_core` (single thread) | 544 MB/s = 0.14 us/row (10 GB ~ 19 s) |
| Current `FileSheet` row read (new `csv::Reader` per row) | **19.2 us/row** |
| Lean parse: one reused `csv_core` reader, no allocation | **0.11 us/row** (9M rows/s/thread) |
| Whole-buffer substring search (`memchr::memmem`) | 7.6 GB/s (227 MB in 30 ms) |
| `Vec<StringRecord>` in memory | **272 B/row = 3.7x the file text** |
| Dense row index (current) | 8 B/row |

**Not measured, and must be before the budgets are trusted:** cold-cache disk
reads, positional reads vs mmap, parallel scaling of scans, sort cost, save
throughput. Those are the first tasks in section 8.

Consequences:
* The current read path is ~170x slower than necessary. That is a design
  flaw, not a tuning detail.
* In-memory rows cost 3.7x the file: a 4 GB file cannot be opened on a 16 GB
  machine with the current backend, and a 10 GB file is hopeless.
* Searching raw bytes is ~70x faster than parsing rows. Filtering should
  search the buffer, then verify only candidate rows.

## 3. Budgets

Derived from section 1 and 2. Each is enforced by a test (section 7).

| Budget | Target | Why |
|---|---|---|
| Index memory | <= 0.25 B/row (sparse index, 1 sample per 64 rows) | 500M rows -> ~60 MB, vs 4 GB dense |
| Row access (random) | <= 50 us (sample + <= 63 rows lean-parsed ~ 7 us) | U2 |
| Visible window read (60 rows) | <= 1 ms, and *bytes read independent of file size* | U2 |
| Index build | >= 400 MB/s warm; progressive; cancellable | U1 |
| Filter, needle with no delimiter/quote | >= 3 GB/s (single pass of the buffer) | U3 |
| Filter, general | >= 5M rows/s/thread | U3 |
| Sort, one column | O(rows) time; 16 B/row transient; 10M rows < 10 s; cancellable | U4 |
| Cell/row/column edit, undo | O(1) or O(log edits); **zero file reads for column delete** | U5 |
| Edit memory | proportional to number of edits | U8 |
| Row cache | bounded by bytes (default 32 MB), not rows | U8 |
| View (filter result) | 4 B per matching row | U8 |
| Save | streaming; >= 150 MB/s or disk-bound; peak extra RAM O(1) | U6 |

Steady-state RSS for a 500M-row file with no active filter: < 300 MB. With a
filter matching every row it is ~2 GB (4 B/row view); acceptable for v1, and
views could become run-length ranges later.

## 4. Architecture

```
        UI (egui)  ── Sheet trait ──┬── MemorySheet  (small files, unchanged)
                                    └── FileSheet
                                          ├─ ByteSource  (positional reads; errors surfaced)
                                          ├─ SparseIndex (1 offset / 64 rows, exact count)
                                          ├─ PieceTable  (row order: file runs + inserted rows)
                                          ├─ Overlay     (edited / inserted row contents)
                                          ├─ ColumnMap   (logical column -> source)
                                          └─ Scanner     (lean parser, memmem prefilter,
                                                          cancellable, streaming results)
```

### 4.1 Sparse index
Store the byte offset of every 64th row (plus exact row count and total
length). Row `i` is found by taking the sample at `i/64`, then lean-parsing
forward at most 63 rows with `csv_core`. Cost ~7 us; memory 0.125 B/row.
Because it scans with the same `csv_core` state machine as parsing, boundaries
always agree with the parser (property-tested; this part of the existing
`RowIndex` is kept). The builder is streaming and chunk-size independent
(also kept). What changes is *storage*: samples only, not every row.

### 4.2 Row order: a piece table
Today: a `Vec<usize>` of ids (8 B/row) built on the first insert/remove, with
O(rows) insert/remove/position_of. Replace with the structure text editors
use: an ordered list of *pieces*, each either a run of consecutive file rows
`[a, b)` or one inserted/put-back row id. Deleting a row splits a run;
inserting adds a piece. Memory is proportional to the number of edits;
position -> id and id -> position are O(log pieces) (file rows keep their
relative order, so a file id is located by binary search over runs).
`removed: HashSet` disappears: a removed file row is simply in no piece.

### 4.3 Overlay
`edited: HashMap<id, Arc<StringRecord>>` as now: copy-on-write rows, inserted
rows and put-back rows. Memory ∝ edits. Snapshots share it by `Arc`.

### 4.4 Columns: a column map (D1 decided: short rows are padded)
Today column edits are an operation log replayed on every read (measured:
~0.24 us per row per logged operation, and `remove_column` scans the whole
file to return undo values). Replace with a `ColumnMap`: a vector of
`logical column -> Original(i) | Added(values by row id)`. Reading a cell is
one lookup, O(1) regardless of how many columns were added/removed.
Undo of a column delete must not read the file, so `remove_column` returns an
opaque *token* (MemorySheet: the values, as today; FileSheet: the column's
source, O(1)) and `restore_column(col, token)` puts it back.

This needed a decision on ragged rows (D1), because the old contract clamped
`insert_column` to each row's own length, which a column map cannot express.
**Decided: a row shorter than the insertion column is treated as padded with
empty fields**, so the new column lines up in every row; removing a column
from a row that doesn't reach it is a no-op. The contract, `MemorySheet`,
`RefModel` and the current `FileSheet` already implement this; the column map
is now a pure internal optimisation that the existing tests guard.

### 4.5 Reading and scanning
* One reusable `csv_core` reader and buffers per thread; no allocation per row.
* Reads go through `ByteSource` in large blocks (>= 64 KB) with a small
  block cache, not one `read_at` per row. Errors are returned, remembered on
  the sheet, and surfaced to the UI (U7); they are never turned into `None`.
* **Filter:** if the needle contains no delimiter, quote or newline, search
  the raw buffer with `memmem` in large blocks; for each match, locate its
  row (binary search of the sample table + forward scan) and verify that a
  *field* contains it; deduplicate per row. Otherwise parse rows with the
  lean parser. Results stream in batches of positions.
* **Sort:** one pass extracting `(key, position)` for the sort column (number
  as `f64`, text as the first 8 bytes, 16 B/row); sort; resolve prefix ties
  by reading the cell. Cancellable between passes.
* All scans take a `Cancel` token, check it per block, and emit progress.
  The app already tags requests with an epoch; the token makes the *worker*
  stop, not just its result be ignored.
* Scans are single-threaded in v1; splitting by byte ranges at sample
  boundaries is a later step once the single-thread numbers are validated.

### 4.6 Save
Stream the output to a temp file in the same directory, then atomically
rename. Walk the piece table: untouched runs are copied as raw bytes (offset
range from the sparse index), edited rows are serialised with the file's
delimiter/quoting rules, inserted rows likewise. If the column map is not the
identity every row is re-serialised (lean parse + write, ~1 us/row). Check
free space first; on any error delete the temp file and keep the original.
After a successful save the new file becomes the base, the overlay and piece
table reset, and the sparse index is built from offsets noted while writing.
Before overwriting, compare size and modification time with what was loaded;
if changed, ask (reload / overwrite / Save As) (U7).

### 4.7 Choosing a backend
`open_sheet(path)` picks `MemorySheet` when the file is small
(default < 256 MB *and* < 5% of available RAM), else `FileSheet`. A setting
forces either. Both implement the same trait and the same conformance suite,
so behaviour is identical.

### 4.8 Concurrency and the app
Workers receive `snapshot()`s (cheap `Arc` clones) and a `Cancel` token, and
send batches (`View` chunks, progress) over the existing channel. The UI shows
live counts. A new filter/sort request cancels the previous one. Index build
runs the same way, so U1 holds.

## 5. Trait changes this implies

* `remove_column` returns a token; add `restore_column` (or make insert accept
  a token) so undo never needs per-row data from a file.
* Scans become incremental: `filter`/`sort` take a `Cancel` and a sink for
  batches (the current returning-a-Vec versions remain as helpers for tests).
* Add `fn health(&self) -> Option<IoProblem>` so the UI can show disk errors.
* `ByteSource::read_at` errors propagate to `health`.
Everything else (`cell`, `record`, `set_cell`, `insert_row`, `remove_row`,
`position_of`, `next_row_id`, `snapshot`) is unchanged, so `app.rs` barely moves.

## 6. Decisions needed

* **D1 - ragged rows and column edits. DECIDED (padding).** Inserting at
  column c inserts at c for every row, padding short rows with empty fields;
  removing column c from a row shorter than c is a no-op. Gives O(1) column
  edits with a column map. (The old behaviour clamped to each row's length.)
* **D2 - envelope. DECIDED:** 5-10 GB required, larger possible later. The
  sparse index and piece table are therefore in scope (see the table in
  section 1).
* **D3 - backend threshold** (256 MB / 5% of RAM) and whether users get a
  setting.
* **D4 - sort on huge files.** Accept 16 B/row transient memory and a full
  pass, or restrict sorting above a size?
* **D5 - zero-field rows on save** (a row with every column removed). Write a
  row of empty fields (`,,,`), or refuse the column delete?

## 7. How each requirement is tested

Correctness (exists): the `Sheet` conformance suite and the model-based
property test against `RefModel` run against every backend; the row index is
property-tested against the `csv` crate.

Memory (new, deterministic): a test binary with a counting
`#[global_allocator]`. Assertions are in bytes, so they are machine-independent:
index of N rows allocates <= 0.25 B x N + c; a 1M-row sheet with 1k edits holds
<= k x 1k; a filter view is 4 B x matches; the row cache never exceeds its byte
cap. Compared across N = 100k and 1M to prove linear/flat growth.

Complexity (new, deterministic): an instrumented `ByteSource` counting calls
and bytes. Assertions: reading a 60-row window reads <= X KB *and the same X
for a 10x larger file*; column delete/restore performs 0 reads; a cell edit
performs 0 writes; a no-match filter reads each byte exactly once; random row
access reads <= 2 blocks.

Performance (new, `--release --ignored`): generated files of 1M-100M rows
with budgets from section 3, set at ~50% of measured numbers and scaled by a
calibration (memmem/memcpy speed measured in the same run) so slow machines
do not false-fail. Reported as a table; also run on demand for tuning.

Usability (new, headless scenario tests with fault injection): typing a second
filter cancels the first (the old worker stops within N blocks); the first
result batch arrives before the scan completes; a `ByteSource` that errors
mid-scan surfaces via `health` and does not drop rows silently; a file
modified between open and save is detected; a writer failing at any byte
leaves the original file byte-identical; free-space shortage is reported
before writing.

## 8. Plan

1. **Spikes that close the unmeasured questions** (1-2 days of work, no
   production code): cold-cache read throughput and pread vs mmap on a large
   generated file; parallel scan scaling; sort of 10M rows by number and text;
   write throughput. Update section 2 and the budgets with real numbers.
2. **Decide D1-D5.**
3. **Test infrastructure first:** counting allocator, instrumented source,
   file generator, perf harness with the budgets above (all failing).
4. **Build to make them pass, in this order:** lean reader + block reads;
   sparse index; piece table; column map + tokens; scanner (filter, sort,
   cancel, progress); `health`; save; real file `ByteSource`; app wiring
   with progress UI and the backend threshold.
5. **Wire into the app behind a setting**, dogfood on real large files, then
   make it the default above the threshold.

## 9. What the existing code is and isn't

Kept, because it is the correctness specification and is independent of the
storage design: the `Sheet` trait, `MemorySheet`, `conformance.rs` (scenarios,
`RefModel`, model-based test), the load/lossy/ragged-row handling, the row
index's boundary logic and its property tests, delimiter/header sniffing.

Replaced: `FileSheet`'s read path (19 us/row), the dense index, the order
`Vec` + `removed` set, the column operation log, the row-count cache, and the
naive filter/sort. The steps 2-5 `FileSheet` is a **correctness prototype**
whose performance model was never checked; its tests carry over unchanged and
become the safety net for the rewrite.
