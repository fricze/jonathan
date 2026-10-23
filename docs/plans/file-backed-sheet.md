# Plan: file-backed `Sheet` backend (TDD)

Goal: open files far larger than RAM with `MemorySheet`-level fidelity. The
`Sheet` trait (`crates/csv_model/src/sheet.rs`) is the seam; this adds a second
implementation, `FileSheet`, without touching `app.rs` beyond the load path.

Non-goals: Polars, type inference, editing in place on disk, multi-format.
Principle: cells are the file's exact text; unedited rows are saved byte-for-byte.

## Design

**Row index.** One streaming pass records the byte offset where each data row
starts (8 bytes/row). The scan must use the *same* state machine as parsing, or
rows disagree with what the parser sees -- naive quote toggling is wrong
(`ab"c` is a literal quote mid-field; `csv` treats a quote as special only at
field start). Use `csv_core::Reader` with the file's delimiter/quote. Handles
`\n`, `\r\n`, `\r`, quoted newlines, `""` escapes, no trailing newline, BOM
(skip), header row (first record, not indexed as data), empty file.

**Byte source.** `trait ByteSource { fn len(); fn read_at(offset, buf) }`.
Implementations: `VecSource` (tests, deterministic) and `FileSource` (positional
reads + small block cache). Chosen over mmap as the default because a file
truncated by another process mid-session turns into SIGBUS with mmap but into
an `Err` with positional reads. mmap can be a third source later.

**Reading.** `cell`/`record` parse the one row slice on demand into an owned
`StringRecord`. A small LRU of parsed rows (behind a `Mutex`, since the trait
reads through `&self`) keeps per-frame scrolling from re-parsing the same
visible rows for every cell. Invalid UTF-8 -> lossy, same as `read_rows`.

**Overlay (edits).** Base rows have id = original index. State on top:
- `rows: HashMap<RowId, StringRecord>` -- edited rows (copy-on-write: first edit
  parses the row, then mutates) and inserted rows (ids >= base count).
- `order: Option<Vec<u32>>` position -> id, created lazily on the first insert
  or remove (identity until then, zero memory). Inverse map built lazily so
  `position_of` stops being O(n) on big files.
- `columns: Vec<ColSource>` where `ColSource` = `Original(i)` | `Added(values by
  row id)`. Column insert/remove edits this list, not every row. `remove_column`
  must still return every row's old value for undo (one column read, O(rows));
  accepted for v1 and noted as the first thing to revisit.

**Snapshot.** `Arc` of source + index, clone of the (small) overlay. Cheap, so
filter/sort workers no longer copy the data.

**Filter.** Parallel (rayon) scan over row chunks. Prefilter on raw bytes with
`memchr::memmem` when the needle contains no delimiter/quote/newline (then any
cell containing it implies the row bytes contain it); confirm candidates by
parsing. Overlay rows are checked from the overlay. Result must equal
`MemorySheet::filter` exactly.

**Sort.** Extract the sort column once into `(SortKey, position)`, sort, return
the view. Memory is O(rows * key) for the sorted column only. Same ordering
rules as `MemorySheet::sort` (numbers by value, then text; stable).

**Save.** Stream to a temp file in the same directory, then atomic rename:
unedited rows are copied as raw bytes (exact quoting, line endings, BOM
preserved); edited/inserted rows are written with the file's delimiter; if the
column list is not the identity, every row is re-serialised. After the rename
the new file becomes the base: overlay cleared, index rebuilt from offsets
noted while writing. The old source is dropped before rename (Windows).

**Selection.** `open_sheet(path, format)` in `csv_model`: below a size threshold
(start ~64 MB) use `MemorySheet`; above it, `FileSheet`. Env var to force
either. The worker thread builds the index, then sends `SetMaster` as today.

## Trait changes needed first (small, own commit)

- `fn next_row_id(&self) -> usize` (default via `row_id` scan). `SetMaster`
  currently calls `row_id` for every row to find the max.
- Nothing else; `position_of` stays, `FileSheet` just makes it fast.

## TDD sequence

Each step: write the failing tests, run them red, implement, run green, commit.

0. **Conformance suite** (DONE). Lift the existing `MemorySheet` tests into
   `fn conformance(make: fn(SheetVec) -> Box<dyn Sheet>)`. Green for
   `MemorySheet`. Add a model-based property test (`proptest`): apply the same
   random sequence of `set_cell / remove_row / insert_row / remove_column /
   insert_column / snapshot` to a `MemorySheet` and the candidate; after each op
   assert identical `len`, `row_id`, `record` for every position. This is the
   main oracle for everything below.
1. **Row index** (DONE; pure, on `&[u8]`). Cases: LF, CRLF, lone CR, quoted newline,
   `""` escape, mid-field quote, no trailing newline, blank trailing line, BOM,
   headerless, `;`/tab/`|`, empty file, header-only, ragged rows. Property: for
   random CSV text, parsing each indexed slice yields the same records as
   `csv::Reader` over the whole text.
2. **FileSheet read-only** (DONE) on `VecSource`: `len`, `row_id == position`,
   `cell`, `record`, ragged rows, invalid UTF-8, out-of-range -> `None`, cache
   returns the same values as a cold read. Conformance (read-only subset) green.
3. **Cell edits** (DONE): `set_cell` + read-back, edit then snapshot isolation,
   `set_cell` on a missing row/col returns `false`.
4. **Structural edits** (DONE): `remove_row` / `insert_row` / `position_of`, id
   stability, remove-then-reinsert round trip (mirrors the app's undo tests),
   insert past the end clamps. Full conformance + property test green.
5. **Column ops** via `ColSource`: remove/insert round trip returns the same
   values, insert at 0 / middle / end, ragged rows.
6. **filter / sort** equal to `MemorySheet` on random data (property test),
   including edited and inserted rows, needles containing delimiters/quotes
   (prefilter must be skipped), empty needle, numeric-vs-text sort.
7. **Save**: no edits -> output byte-identical to input (CRLF, BOM, odd
   quoting); one edited cell -> only that row differs; insert/remove rows;
   column ops; delimiter/headerless formats; atomic (failed write leaves the
   original untouched); after save, reads and a second save still work.
8. **`FileSource`** (positional reads + block cache) against the same tests via
   a temp file; truncation mid-session returns errors, not a crash.
9. **`open_sheet` + app wiring**: threshold selection; app-level tests using a
   `FileSheet`; `#[ignore]`d perf test generating a multi-million-row file to
   assert index time and resident memory budgets. Manual GUI pass (scroll, edit,
   undo, sort, filter, save) on a large file.

## New dependencies

`csv-core` (indexing), `memchr` (prefilter), `rayon` (parallel filter),
`proptest` (dev). `memmap2` only if an mmap source is added later.

## Risks / open questions

- Undo for `remove_column` stores one string per row: fine to ~millions of
  rows, heavy at hundreds of millions. Likely needs a column-handle undo entry.
- Row cache sizing and lock contention if the UI ever reads from several threads.
- Index build time on first open of multi-GB files: v1 blocks behind the
  existing `loading` flag; progressive indexing (rows appear while scanning) is
  a follow-up.
- UTF-16 files: transcode to a temp UTF-8 copy at open (not lazy). Other
  encodings: refuse with a clear message.
- Lossy rows save back with U+FFFD (the load warning says so). Preserving the
  original bytes of untouched rows on save would fix it, and the raw-copy save
  path in step 7 gets that for free.
- Files changed on disk by another process while open: detect via size/mtime
  check before save and prompt, rather than silently overwriting.

## Notes from implementation

- A file can only hold rows with at least one field (a zero-field row is a blank
  line, which CSV skips) and ids at load are sequential from 0. The conformance
  suite's load-time scenarios and generators respect that; edits may still
  produce zero-field rows (removing a row's only column). Saving such a row
  would write a blank line and lose it -- same as `MemorySheet` today; decide
  what save should do before step 7.
- `filter` and `sort` in `FileSheet` are correct but unoptimised full scans
  that bypass the row cache; step 6 adds the prefilter and parallelism under
  the existing equality-with-`MemorySheet` property tests.
- Step 2 left the five edit methods as `todo!()`; steps 3-5 replace them, so
  `FileSheet` must not be wired into the app before then.
- Step 4 state: `order: Option<Arc<Vec<usize>>>` (identity until the first
  insert/remove), `removed: HashSet` of file row ids, and the `edited` map now
  also holds inserted and put-back rows. The row cache is keyed by id.
  Known costs: insert/remove move the order list (O(rows)); `position_of` and
  `next_row_id` scan it once it exists; after the first structural edit memory
  is ~16 bytes/row (index + order). Candidates if it matters: u32 ids, or a
  tree/gap structure instead of a flat Vec.
