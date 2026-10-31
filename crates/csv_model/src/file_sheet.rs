//! A [`Sheet`] backed by the file itself instead of rows in memory.
//!
//! Loading builds a [`RowIndex`] (8 bytes/row); a row is read and parsed only
//! when something asks for it. This step is the read-only half: reads,
//! filter, sort, snapshot. Edits arrive in the next steps.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use csv::StringRecord;

use crate::lean::LeanParser;
use crate::{RowIndex, RowIndexBuilder, Sheet, SheetRow, SortOrder, View, sort_cells};

/// Random-access bytes. Positional reads (not mmap) so that a file shrinking
/// under us is an `Err`, never a crash.
pub trait ByteSource: Send + Sync {
    fn len(&self) -> u64;
    /// Read up to `buf.len()` bytes at `offset`; short reads allowed.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

pub struct VecSource(pub Vec<u8>);

impl ByteSource for VecSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
}

const INDEX_CHUNK: usize = 1 << 20;
const DEFAULT_CACHE_ROWS: usize = 1024;

/// Recently parsed rows, oldest evicted first. The table asks for the same
/// visible rows every frame, once per cell; this keeps that from re-reading
/// and re-parsing the row for each one.
///
/// Keyed by (row id, number of column operations applied): the cached row is
/// the file row *after* those operations, and the cache is shared with
/// snapshots, which may be at an earlier point in the column history.
type CacheKey = (usize, usize);

struct RowCache {
    rows: HashMap<CacheKey, Arc<StringRecord>>,
    order: VecDeque<CacheKey>,
    capacity: usize,
}

impl RowCache {
    fn insert(&mut self, key: CacheKey, row: Arc<StringRecord>) {
        if self.capacity == 0 || self.rows.insert(key, row).is_some() {
            return;
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.rows.remove(&old);
            }
        }
    }
}

/// Random row access reads aligned blocks this big. Small on purpose: a miss
/// copies a whole block, and the budget for a random row is 50 us (a 64 KiB
/// copy is ~12 us; 1 MiB would be ~200 us).
const BLOCK: u64 = 64 * 1024;
const DEFAULT_CACHE_BLOCKS: usize = 64;
/// Scans read this much at a time while moving forward through the file.
const SCAN_WINDOW: u64 = 1 << 20;

/// Recently read blocks of the file, oldest evicted first. The base file never
/// changes, so (unlike the row cache) this is shared freely with snapshots.
struct BlockCache {
    blocks: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl BlockCache {
    fn insert(&mut self, no: u64, block: Arc<Vec<u8>>) {
        if self.capacity == 0 || self.blocks.insert(no, block).is_some() {
            return;
        }
        self.order.push_back(no);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.blocks.remove(&old);
            }
        }
    }
}

/// The bytes of one row: a slice of a cached block, or a copy when the row
/// spans blocks.
enum RowBytes {
    Block(Arc<Vec<u8>>, Range<usize>),
    Owned(Vec<u8>),
}

impl RowBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            RowBytes::Block(block, r) => &block[r.clone()],
            RowBytes::Owned(v) => v,
        }
    }
}

/// A window onto the file for one scan. Rows are requested in increasing
/// position, so it reads large chunks (`SCAN_WINDOW`) and serves many rows from
/// each; if the scan jumps ahead (a sparse view) it falls back to small reads.
/// Bypasses the shared caches so a scan cannot evict what the table shows.
struct ScanWindow {
    start: u64,
    data: Vec<u8>,
}

impl ScanWindow {
    fn new() -> Self {
        ScanWindow {
            start: 0,
            data: Vec::new(),
        }
    }

    fn get(&mut self, sheet: &FileSheet, range: Range<u64>) -> Option<&[u8]> {
        let end = self.start + self.data.len() as u64;
        if self.data.is_empty() || range.start < self.start || range.end > end {
            let total = sheet.source.len();
            let sequential =
                !self.data.is_empty() && range.start >= self.start && range.start <= end + BLOCK;
            let want = (range.end - range.start).max(if sequential { SCAN_WINDOW } else { BLOCK });
            let len = want.min(total.saturating_sub(range.start)) as usize;
            self.data.resize(len, 0);
            self.start = range.start;
            if !sheet.read_exact_at(range.start, &mut self.data) {
                self.data.clear();
                return None;
            }
        }
        let lo = (range.start - self.start) as usize;
        let hi = (range.end - self.start) as usize;
        self.data.get(lo..hi)
    }
}

thread_local! {
    /// One parser per thread, reused for every row read; rebuilt only if the
    /// delimiter changes.
    static PARSER: RefCell<Option<(u8, LeanParser)>> = const { RefCell::new(None) };
}

/// A column insert or remove, replayed lazily on every row read from the
/// file. (To be replaced by a column map now that short rows are padded
/// rather than clamped; see docs/design/large-file-engine.md, 4.4.)
#[derive(Clone)]
enum ColOp {
    Remove(usize),
    Insert {
        col: usize,
        values: Arc<HashMap<usize, String>>,
    },
}

impl ColOp {
    fn apply(&self, id: usize, row: &StringRecord) -> StringRecord {
        match self {
            ColOp::Remove(col) => row
                .iter()
                .enumerate()
                .filter(|(i, _)| i != col)
                .map(|(_, f)| f)
                .collect(),
            ColOp::Insert { col, values } => {
                let mut fields: Vec<&str> = row.iter().collect();
                // Pad a short row so the new column lines up with the rest.
                fields.resize(fields.len().max(*col), "");
                fields.insert(*col, values.get(&id).map_or("", String::as_str));
                fields.into_iter().collect()
            }
        }
    }
}

#[derive(Clone)]
pub struct FileSheet {
    source: Arc<dyn ByteSource>,
    index: Arc<RowIndex>,
    delimiter: u8,
    cache: Arc<Mutex<RowCache>>,
    blocks: Arc<Mutex<BlockCache>>,
    /// Rows changed since load, keyed by row id. The file itself is never
    /// written. A row is copied in here the first time one of its cells is
    /// edited (or when it is inserted, or put back after a removal) and is
    /// read from here ever after, so the cache cannot serve it stale. `Arc`
    /// makes a snapshot share the copies until either side edits one
    /// (`Arc::make_mut` then copies it).
    edited: HashMap<usize, Arc<StringRecord>>,
    /// Display order as row ids. `None` means untouched: position == id, and
    /// nothing is stored. The first insert or remove builds `0..n` and every
    /// later one edits it. Shared with snapshots until either side edits it.
    order: Option<Arc<Vec<usize>>>,
    /// File rows (ids below the row count) that have been removed.
    removed: HashSet<usize>,
    /// Column operations since load, oldest first. Rows read from the file
    /// have all of them applied; rows in `edited` already do (they are
    /// updated when an operation is added). Shared with snapshots.
    col_ops: Arc<Vec<ColOp>>,
}

impl FileSheet {
    pub fn new(source: Arc<dyn ByteSource>, delimiter: u8, has_header: bool) -> io::Result<Self> {
        Self::with_caches(
            source,
            delimiter,
            has_header,
            DEFAULT_CACHE_ROWS,
            DEFAULT_CACHE_BLOCKS,
        )
    }

    /// Like `new`, but remembers at most `capacity` parsed rows and caches no
    /// blocks, so every row read that misses the row cache reaches the source.
    /// A hook for tests that count source reads.
    pub fn with_cache_capacity(
        source: Arc<dyn ByteSource>,
        delimiter: u8,
        has_header: bool,
        capacity: usize,
    ) -> io::Result<Self> {
        Self::with_caches(source, delimiter, has_header, capacity, 0)
    }

    pub fn with_caches(
        source: Arc<dyn ByteSource>,
        delimiter: u8,
        has_header: bool,
        row_capacity: usize,
        block_capacity: usize,
    ) -> io::Result<Self> {
        let mut builder = RowIndexBuilder::new(delimiter, has_header);
        let mut buf = vec![0u8; INDEX_CHUNK];
        let total = source.len();
        let mut at = 0u64;
        while at < total {
            let n = source.read_at(at, &mut buf)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended while indexing",
                ));
            }
            builder.feed(&buf[..n]);
            at += n as u64;
        }
        Ok(FileSheet {
            source,
            index: Arc::new(builder.finish()),
            delimiter,
            cache: Arc::new(Mutex::new(RowCache {
                rows: HashMap::new(),
                order: VecDeque::new(),
                capacity: row_capacity,
            })),
            blocks: Arc::new(Mutex::new(BlockCache {
                blocks: HashMap::new(),
                order: VecDeque::new(),
                capacity: block_capacity,
            })),
            edited: HashMap::new(),
            order: None,
            removed: HashSet::new(),
            col_ops: Arc::new(Vec::new()),
        })
    }

    /// Fill `buf` from `offset`, looping over short reads. `false` on error
    /// or if the source ends early (e.g. the file shrank).
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> bool {
        let mut filled = 0;
        while filled < buf.len() {
            match self
                .source
                .read_at(offset + filled as u64, &mut buf[filled..])
            {
                Ok(0) | Err(_) => return false,
                Ok(n) => filled += n,
            }
        }
        true
    }

    /// Block number `no` of the file, through the block cache.
    fn block(&self, no: u64) -> Option<Arc<Vec<u8>>> {
        let lock = || self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().blocks.get(&no) {
            return Some(hit.clone());
        }
        let start = no * BLOCK;
        let len = BLOCK.min(self.source.len().checked_sub(start)?) as usize;
        let mut data = vec![0u8; len];
        if !self.read_exact_at(start, &mut data) {
            return None;
        }
        let block = Arc::new(data);
        lock().insert(no, block.clone());
        Some(block)
    }

    /// The bytes of a row, from the block cache (one block, or a copy
    /// assembled from several when the row spans a boundary).
    fn row_bytes(&self, range: Range<u64>) -> Option<RowBytes> {
        if range.end <= range.start {
            return Some(RowBytes::Owned(Vec::new()));
        }
        let (first, last) = (range.start / BLOCK, (range.end - 1) / BLOCK);
        if first == last {
            let block = self.block(first)?;
            let base = first * BLOCK;
            let r = (range.start - base) as usize..(range.end - base) as usize;
            return (r.end <= block.len()).then(|| RowBytes::Block(block, r));
        }
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        for no in first..=last {
            let block = self.block(no)?;
            let base = no * BLOCK;
            let lo = range.start.max(base) - base;
            let hi = (range.end.min(base + BLOCK) - base) as usize;
            out.extend_from_slice(block.get(lo as usize..hi)?);
        }
        Some(RowBytes::Owned(out))
    }

    /// Read and parse file row `file_row` into a record. `None` if there is no
    /// such row or its bytes can no longer be read.
    fn read_row(&self, file_row: usize) -> Option<StringRecord> {
        let bytes = self.row_bytes(self.index.row_range(file_row)?)?;
        PARSER.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.as_ref().is_none_or(|(d, _)| *d != self.delimiter) {
                *slot = Some((self.delimiter, LeanParser::new(self.delimiter)));
            }
            let parser = &mut slot.as_mut()?.1;
            let fields = parser.parse(bytes.as_slice())?;
            let mut raw = csv::ByteRecord::new();
            for f in fields.iter() {
                raw.push_field(f);
            }
            Some(match StringRecord::from_byte_record(raw) {
                Ok(record) => record,
                Err(e) => StringRecord::from_byte_record_lossy(e.into_byte_record()),
            })
        })
    }

    /// True while positions are file rows in file order with the original
    /// columns, which lets scans read the file front to back.
    fn scan_is_streamable(&self) -> bool {
        self.order.is_none() && self.col_ops.is_empty()
    }

    /// Row id at display `position`.
    fn id_at(&self, position: usize) -> Option<usize> {
        match &self.order {
            Some(order) => order.get(position).copied(),
            None => (position < self.index.len()).then_some(position),
        }
    }

    /// Whether `id` names a file row that is still part of the sheet and has
    /// not been replaced by an edited copy.
    fn is_live_file_row(&self, id: usize) -> bool {
        id < self.index.len() && !self.removed.contains(&id)
    }

    /// The row with `id` as the user sees it, bypassing the cache: the edited
    /// copy if there is one, else straight from the file. Scans use this.
    fn load_by_id(&self, id: usize) -> Option<Cow<'_, StringRecord>> {
        if let Some(edited) = self.edited.get(&id) {
            return Some(Cow::Borrowed(edited.as_ref()));
        }
        if !self.is_live_file_row(id) {
            return None;
        }
        let raw = self.read_row(id)?;
        Some(Cow::Owned(
            self.col_ops.iter().fold(raw, |row, op| op.apply(id, &row)),
        ))
    }

    fn load_row(&self, position: usize) -> Option<Cow<'_, StringRecord>> {
        self.load_by_id(self.id_at(position)?)
    }

    /// The row with `id` as the user sees it, through the cache (keyed by id,
    /// not position, so inserts and removals never invalidate it).
    fn row_by_id(&self, id: usize) -> Option<Arc<StringRecord>> {
        if let Some(edited) = self.edited.get(&id) {
            return Some(edited.clone());
        }
        if !self.is_live_file_row(id) {
            return None;
        }
        let key = (id, self.col_ops.len());
        let lock = || self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().rows.get(&key) {
            return Some(hit.clone());
        }
        let raw = self.read_row(id)?;
        let row = Arc::new(self.col_ops.iter().fold(raw, |row, op| op.apply(id, &row)));
        lock().insert(key, row.clone());
        Some(row)
    }

    fn row(&self, position: usize) -> Option<Arc<StringRecord>> {
        self.row_by_id(self.id_at(position)?)
    }

    /// Record a column operation: future file reads replay it, and rows
    /// already copied into `edited` are updated now.
    fn push_col_op(&mut self, op: ColOp) {
        for (id, row) in self.edited.iter_mut() {
            *row = Arc::new(op.apply(*id, row));
        }
        Arc::make_mut(&mut self.col_ops).push(op);
    }

    /// The order list, built as `0..n` on first use and copied if a
    /// snapshot still shares it.
    fn order_mut(&mut self) -> &mut Vec<usize> {
        let n = self.index.len();
        Arc::make_mut(self.order.get_or_insert_with(|| Arc::new((0..n).collect())))
    }
}

#[cfg(test)]
impl FileSheet {
    fn cached_rows(&self) -> usize {
        self.cache.lock().unwrap().rows.len()
    }

    fn order_is_materialized(&self) -> bool {
        self.order.is_some()
    }
}

impl Sheet for FileSheet {
    fn len(&self) -> usize {
        self.order.as_ref().map_or(self.index.len(), |o| o.len())
    }

    fn row_id(&self, position: usize) -> Option<usize> {
        self.id_at(position)
    }

    fn position_of(&self, row_id: usize) -> Option<usize> {
        match &self.order {
            Some(order) => order.iter().position(|&id| id == row_id),
            None => (row_id < self.index.len()).then_some(row_id),
        }
    }

    fn next_row_id(&self) -> usize {
        match &self.order {
            Some(order) => order.iter().max().map_or(0, |m| m + 1),
            None => self.index.len(),
        }
    }

    fn cell(&self, position: usize, col: usize) -> Option<Cow<'_, str>> {
        self.row(position)?
            .get(col)
            .map(|v| Cow::Owned(v.to_string()))
    }

    fn record(&self, position: usize) -> Option<Cow<'_, StringRecord>> {
        self.row(position).map(|r| Cow::Owned((*r).clone()))
    }

    fn set_cell(&mut self, row_id: usize, col: usize, value: &str) -> bool {
        let Some(current) = self.row_by_id(row_id) else {
            return false;
        };
        if col >= current.len() {
            return false;
        }
        let rewritten: StringRecord = current
            .iter()
            .enumerate()
            .map(|(i, f)| if i == col { value } else { f })
            .collect();
        self.edited.insert(row_id, Arc::new(rewritten));
        true
    }

    fn remove_row(&mut self, position: usize) -> Option<SheetRow> {
        // Read first: a row that can't be read can't be handed back for
        // undo, so it stays where it is.
        let id = self.id_at(position)?;
        let record = (*self.row_by_id(id)?).clone();
        self.order_mut().remove(position);
        self.edited.remove(&id);
        if id < self.index.len() {
            self.removed.insert(id);
        }
        Some((id, record))
    }

    fn insert_row(&mut self, position: usize, (id, record): SheetRow) {
        let order = self.order_mut();
        let at = position.min(order.len());
        order.insert(at, id);
        self.edited.insert(id, Arc::new(record));
    }

    fn remove_column(&mut self, col: usize) -> Vec<(usize, String)> {
        // Collect first, from the rows as they are now. Rows that cannot be
        // read from the file are skipped (they have no value to hand back).
        let removed = (0..self.len())
            .filter_map(|position| {
                let id = self.id_at(position)?;
                let row = self.load_by_id(id)?;
                row.get(col).map(|value| (id, value.to_string()))
            })
            .collect();
        self.push_col_op(ColOp::Remove(col));
        removed
    }

    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]) {
        let values = values_by_row.iter().cloned().collect();
        self.push_col_op(ColOp::Insert {
            col,
            values: Arc::new(values),
        });
    }

    // Scans bypass the shared caches so they cannot evict what the table is
    // showing. When the order and columns are untouched (the common case: a
    // file opened, perhaps a few cells edited) they stream the file through a
    // large window and parse rows with the lean parser; otherwise they go
    // row by row through the general path.
    fn filter(&self, needle: &str) -> View {
        if needle.is_empty() {
            return (0..self.len() as u32).collect();
        }
        if !self.scan_is_streamable() {
            return (0..self.len())
                .filter(|&p| {
                    self.load_row(p)
                        .is_some_and(|r| r.iter().any(|c| c.contains(needle)))
                })
                .map(|p| p as u32)
                .collect();
        }
        let finder = memchr::memmem::Finder::new(needle.as_bytes());
        // A field's text appears verbatim in the row's bytes unless it contains
        // a quote (stored doubled), so a needle without one can rule out a row
        // by searching bytes alone, no parsing.
        let prefilter = !needle.as_bytes().contains(&b'"');
        let mut parser = LeanParser::new(self.delimiter);
        let mut hits = View::new();
        if prefilter && !needle.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r') {
            // One search per window of many rows. A match cannot span two rows
            // (the needle has no line break), so each match lies in one row;
            // only those rows are parsed to check a *field* contains it.
            let mut buf = Vec::new();
            let mut row = 0usize;
            while row < self.index.len() {
                let first = self.index.row_range(row).unwrap();
                let target = first.start + SCAN_WINDOW;
                let last = self
                    .index
                    .row_at_offset(target.min(self.index.total_len() - 1))
                    .unwrap_or(row);
                let last = if last < row || target >= self.index.total_len() {
                    self.index.len() - 1
                } else {
                    last
                };
                let end = self.index.row_range(last).unwrap().end;
                buf.resize((end - first.start) as usize, 0);
                if !self.read_exact_at(first.start, &mut buf) {
                    break; // the file went away; report what was found so far
                }
                let mut at = 0usize;
                while let Some(found) = finder.find(&buf[at..]) {
                    let offset = first.start + (at + found) as u64;
                    let Some(r) = self.index.row_at_offset(offset) else {
                        break;
                    };
                    let range = self.index.row_range(r).unwrap();
                    if !self.edited.contains_key(&r) {
                        let bytes = &buf[(range.start - first.start) as usize
                            ..(range.end - first.start) as usize];
                        if parser
                            .parse(bytes)
                            .is_some_and(|f| f.iter().any(|c| finder.find(c).is_some()))
                        {
                            hits.push(r as u32);
                        }
                    }
                    at = (range.end - first.start) as usize; // next row onward
                }
                row = last + 1;
            }
            // Edited rows are judged by their edited text, never by the file's.
            hits.extend(
                self.edited
                    .iter()
                    .filter(|(_, rec)| rec.iter().any(|c| c.contains(needle)))
                    .map(|(&id, _)| id as u32),
            );
            hits.sort_unstable();
            hits.dedup();
            return hits;
        }
        let mut window = ScanWindow::new();
        for p in 0..self.index.len() {
            let hit = match self.edited.get(&p) {
                Some(row) => row.iter().any(|c| c.contains(needle)),
                None => match self.index.row_range(p).and_then(|r| window.get(self, r)) {
                    Some(bytes) if !prefilter || finder.find(bytes).is_some() => parser
                        .parse(bytes)
                        .is_some_and(|f| f.iter().any(|c| finder.find(c).is_some())),
                    _ => false,
                },
            };
            if hit {
                hits.push(p as u32);
            }
        }
        hits
    }

    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View {
        if !self.scan_is_streamable() {
            let texts: Vec<(String, u32)> = view
                .into_iter()
                .map(|p| {
                    let text = self
                        .load_row(p as usize)
                        .and_then(|r| r.get(sort_by.0).map(str::to_string))
                        .unwrap_or_default();
                    (text, p)
                })
                .collect();
            return sort_cells(
                texts.iter().map(|(t, p)| (t.as_str(), *p)).collect(),
                sort_by.1,
            );
        }
        // Copy the sort column's text into one arena (one allocation, not one
        // per row), then sort `(text, position)` pairs borrowed from it.
        let mut window = ScanWindow::new();
        let mut parser = LeanParser::new(self.delimiter);
        let mut arena = String::new();
        let mut spans = Vec::with_capacity(view.len());
        for p in view {
            let start = arena.len();
            match self.edited.get(&(p as usize)) {
                Some(row) => arena.push_str(row.get(sort_by.0).unwrap_or("")),
                None => {
                    let cell = self
                        .index
                        .row_range(p as usize)
                        .and_then(|r| window.get(self, r))
                        .and_then(|bytes| parser.parse(bytes))
                        .and_then(|f| f.get(sort_by.0));
                    if let Some(cell) = cell {
                        arena.push_str(&String::from_utf8_lossy(cell));
                    }
                }
            }
            spans.push((start, arena.len(), p));
        }
        sort_cells(
            spans.iter().map(|&(a, b, p)| (&arena[a..b], p)).collect(),
            sort_by.1,
        )
    }

    fn snapshot(&self) -> Box<dyn Sheet> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SheetVec;
    use crate::conformance::{self, Make};
    use proptest::prelude::*;

    fn open(bytes: &[u8], header: bool) -> FileSheet {
        FileSheet::new(Arc::new(VecSource(bytes.to_vec())), b',', header).unwrap()
    }

    fn strs(s: &FileSheet, pos: usize) -> Option<Vec<String>> {
        s.record(pos)
            .map(|r| r.iter().map(str::to_string).collect())
    }

    /// Serialise rows to CSV bytes and open them as a headerless FileSheet.
    fn make_file(rows: SheetVec) -> Box<dyn Sheet> {
        let mut w = csv::WriterBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_writer(vec![]);
        for (_, r) in &rows {
            w.write_record(r).unwrap();
        }
        Box::new(open(&w.into_inner().unwrap(), false))
    }

    const MAKE: &Make = &make_file;

    #[test]
    fn reads_rows_cells_and_ids_from_bytes() {
        let s = open(b"a,b\n1,2\n3,4\n", true);
        assert_eq!(s.len(), 2);
        assert_eq!(strs(&s, 0), Some(vec!["1".into(), "2".into()]));
        assert_eq!(s.cell(1, 1).as_deref(), Some("4"));
        assert_eq!(
            (s.row_id(0), s.row_id(1), s.row_id(2)),
            (Some(0), Some(1), None)
        );
        assert_eq!(s.position_of(1), Some(1));
        assert_eq!(s.next_row_id(), 2);
    }

    #[test]
    fn header_row_is_not_a_data_row_and_headerless_keeps_it() {
        assert_eq!(open(b"h1,h2\n1,2\n", true).len(), 1);
        assert_eq!(open(b"1,2\n3,4\n", false).len(), 2);
    }

    #[test]
    fn bom_crlf_quotes_and_embedded_newlines_parse() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"h\r\n\"a\r\nb\"\"c\",d\r\nx,y\r\n");
        let s = open(&b, true);
        assert_eq!(s.len(), 2);
        assert_eq!(strs(&s, 0), Some(vec!["a\r\nb\"c".into(), "d".into()]));
        assert_eq!(strs(&s, 1), Some(vec!["x".into(), "y".into()]));
    }

    #[test]
    fn other_delimiters_parse() {
        let s = FileSheet::new(Arc::new(VecSource(b"h;i\n1,5;2\n".to_vec())), b';', true).unwrap();
        assert_eq!(strs(&s, 0), Some(vec!["1,5".into(), "2".into()]));
    }

    #[test]
    fn ragged_rows_are_kept_as_they_are() {
        let s = open(b"a,b,c\n1\n2,3,4,5\n", true);
        assert_eq!(strs(&s, 0), Some(vec!["1".into()]));
        assert_eq!(strs(&s, 1).unwrap().len(), 4);
    }

    #[test]
    fn invalid_utf8_is_shown_lossily() {
        let s = open(b"h\nok\nx\xff\n", true);
        assert_eq!(strs(&s, 1), Some(vec!["x\u{fffd}".into()]));
    }

    #[test]
    fn rows_without_a_trailing_newline_and_blank_lines_read_correctly() {
        let s = open(b"h\n1\n\n\n2", true);
        assert_eq!(s.len(), 2);
        assert_eq!(strs(&s, 0), Some(vec!["1".into()]));
        assert_eq!(strs(&s, 1), Some(vec!["2".into()]));
    }

    #[test]
    fn a_tiny_cache_changes_nothing_about_what_is_read() {
        let bytes = b"h\na,1\nb,2\nc,3\nd,4\n";
        let cold =
            FileSheet::with_cache_capacity(Arc::new(VecSource(bytes.to_vec())), b',', true, 1)
                .unwrap();
        let warm = open(bytes, true);
        for pass in 0..3 {
            for pos in [0, 3, 1, 3, 2, 0] {
                assert_eq!(strs(&cold, pos), strs(&warm, pos), "pass {pass} pos {pos}");
            }
        }
    }

    struct Failing;
    impl ByteSource for Failing {
        fn len(&self) -> u64 {
            10
        }
        fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("disk gone"))
        }
    }

    #[test]
    fn a_source_that_fails_at_open_is_an_error_not_a_panic() {
        assert!(FileSheet::new(Arc::new(Failing), b',', true).is_err());
    }

    /// Index fine, then the file "shrinks": reads return None instead of panicking.
    struct Shrinks(VecSource, std::sync::atomic::AtomicBool);
    impl ByteSource for Shrinks {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn read_at(&self, o: u64, b: &mut [u8]) -> io::Result<usize> {
            if self.1.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(0);
            }
            self.0.read_at(o, b)
        }
    }

    #[test]
    fn a_file_that_shrinks_after_open_reads_as_missing_rows() {
        let src = Arc::new(Shrinks(
            VecSource(b"h\n1\n2\n".to_vec()),
            Default::default(),
        ));
        let s = FileSheet::new(src.clone(), b',', true).unwrap();
        assert_eq!(s.len(), 2);
        src.1.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(s.cell(0, 0), None);
        assert!(s.record(1).is_none());
    }

    struct Counting(VecSource, std::sync::atomic::AtomicUsize);
    impl ByteSource for Counting {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn read_at(&self, o: u64, b: &mut [u8]) -> io::Result<usize> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.0.read_at(o, b)
        }
    }

    #[test]
    fn cache_serves_repeat_reads_and_evicts_the_oldest_row() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = Arc::new(Counting(
            VecSource(b"a\nb\nc\n".to_vec()),
            Default::default(),
        ));
        let s = FileSheet::with_cache_capacity(src.clone(), b',', false, 2).unwrap();
        let after_index = src.1.load(Relaxed);

        // Every cell of one row costs a single source read.
        for col in 0..3 {
            let _ = s.cell(0, col);
        }
        assert_eq!(src.1.load(Relaxed), after_index + 1);

        s.cell(1, 0);
        s.cell(0, 0);
        s.cell(1, 0);
        assert_eq!(
            src.1.load(Relaxed),
            after_index + 2,
            "rows 0 and 1 both cached"
        );

        s.cell(2, 0); // evicts row 0, the oldest
        assert_eq!(src.1.load(Relaxed), after_index + 3);
        s.cell(1, 0);
        assert_eq!(src.1.load(Relaxed), after_index + 3, "row 1 survived");
        s.cell(0, 0);
        assert_eq!(
            src.1.load(Relaxed),
            after_index + 4,
            "row 0 was evicted, read again"
        );
    }

    #[test]
    fn scans_do_not_disturb_the_cache() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = Arc::new(Counting(
            VecSource(b"a\nb\nc\nd\n".to_vec()),
            Default::default(),
        ));
        let s = FileSheet::with_cache_capacity(src.clone(), b',', false, 2).unwrap();
        s.cell(0, 0);
        let before = src.1.load(Relaxed);
        s.filter("zzz");
        s.sort(vec![0, 1, 2, 3], (0, SortOrder::Asc));
        let during_scans = src.1.load(Relaxed);
        s.cell(0, 0);
        assert_eq!(
            src.1.load(Relaxed),
            during_scans,
            "row 0 still cached after full scans (before={before})"
        );
    }

    #[test]
    fn snapshot_reads_the_same_rows() {
        let s = open(b"h\n1\n2\n", true);
        let snap = s.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.cell(1, 0).as_deref(), Some("2"));
    }

    #[test]
    fn edited_cell_reads_back_through_cell_and_record() {
        let mut s = open(b"h1,h2\n1,2\n3,4\n", true);
        assert!(s.set_cell(1, 0, "X"));
        assert_eq!(s.cell(1, 0).as_deref(), Some("X"));
        assert_eq!(strs(&s, 1), Some(vec!["X".into(), "4".into()]));
        assert_eq!(
            strs(&s, 0),
            Some(vec!["1".into(), "2".into()]),
            "other rows untouched"
        );
    }

    #[test]
    fn editing_a_row_that_is_already_cached_shows_the_new_value() {
        let mut s = open(b"1,2\n3,4\n", false);
        assert_eq!(s.cell(0, 0).as_deref(), Some("1")); // now cached
        s.set_cell(0, 0, "new");
        assert_eq!(s.cell(0, 0).as_deref(), Some("new"));
        assert_eq!(strs(&s, 0), Some(vec!["new".into(), "2".into()]));
    }

    #[test]
    fn a_row_edited_twice_keeps_both_edits() {
        let mut s = open(b"1,2\n", false);
        s.set_cell(0, 0, "a");
        s.set_cell(0, 1, "b");
        assert_eq!(strs(&s, 0), Some(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn edited_rows_are_served_from_memory_not_re_read() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = Arc::new(Counting(
            VecSource(b"1,2\n3,4\n".to_vec()),
            Default::default(),
        ));
        let mut s = FileSheet::with_cache_capacity(src.clone(), b',', false, 0).unwrap();
        s.set_cell(0, 0, "x");
        let after_edit = src.1.load(Relaxed);
        for _ in 0..5 {
            s.cell(0, 0);
            s.cell(0, 1);
        }
        assert_eq!(
            src.1.load(Relaxed),
            after_edit,
            "even with the row cache off"
        );
    }

    #[test]
    fn a_failed_edit_does_not_create_or_change_anything() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = Arc::new(Counting(
            VecSource(b"1,2\n3\n".to_vec()),
            Default::default(),
        ));
        let mut s = FileSheet::with_cache_capacity(src.clone(), b',', false, 0).unwrap();
        assert!(!s.set_cell(7, 0, "x"), "no such row");
        assert!(!s.set_cell(1, 1, "x"), "column past the end of a short row");
        assert_eq!(strs(&s, 1), Some(vec!["3".into()]));
        let before = src.1.load(Relaxed);
        s.cell(1, 0);
        assert_eq!(
            src.1.load(Relaxed),
            before + 1,
            "row 1 still comes from the file"
        );
    }

    #[test]
    fn a_row_that_cannot_be_read_cannot_be_edited() {
        let src = Arc::new(Shrinks(VecSource(b"1,2\n".to_vec()), Default::default()));
        let mut s = FileSheet::new(src.clone(), b',', false).unwrap();
        src.1.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!s.set_cell(0, 0, "x"));
    }

    #[test]
    fn filter_sees_edited_text_not_the_old_text() {
        let mut s = open(b"apple\nbanana\n", false);
        s.set_cell(0, 0, "cherry");
        assert_eq!(s.filter("cherry"), vec![0]);
        assert_eq!(s.filter("apple"), Vec::<u32>::new());
    }

    #[test]
    fn sort_orders_by_edited_text() {
        let mut s = open(b"1\n2\n3\n", false);
        s.set_cell(0, 0, "9");
        assert_eq!(s.sort(vec![0, 1, 2], (0, SortOrder::Asc)), vec![1, 2, 0]);
    }

    fn ids(s: &FileSheet) -> Vec<usize> {
        (0..s.len()).map(|p| s.row_id(p).unwrap()).collect()
    }

    fn first_cells(s: &FileSheet) -> Vec<String> {
        (0..s.len())
            .map(|p| s.cell(p, 0).unwrap().into_owned())
            .collect()
    }

    #[test]
    fn removing_a_middle_row_shifts_positions_but_not_ids() {
        let mut s = open(b"a\nb\nc\nd\n", false);
        let (id, rec) = s.remove_row(1).unwrap();
        assert_eq!((id, rec.get(0)), (1, Some("b")));
        assert_eq!(s.len(), 3);
        assert_eq!(ids(&s), vec![0, 2, 3]);
        assert_eq!(first_cells(&s), ["a", "c", "d"]);
        assert_eq!(s.position_of(2), Some(1));
        assert_eq!(s.position_of(1), None);
    }

    #[test]
    fn removing_out_of_range_changes_nothing() {
        let mut s = open(b"a\nb\n", false);
        assert!(s.remove_row(2).is_none());
        assert_eq!(ids(&s), vec![0, 1]);
        assert!(
            !s.order_is_materialized(),
            "a failed removal builds nothing"
        );
    }

    #[test]
    fn remove_then_reinsert_puts_the_row_back_exactly() {
        let mut s = open(b"a,1\nb,2\nc,3\n", false);
        let row = s.remove_row(1).unwrap();
        s.insert_row(1, row);
        assert_eq!(ids(&s), vec![0, 1, 2]);
        assert_eq!(strs(&s, 1), Some(vec!["b".into(), "2".into()]));
        assert_eq!(s.position_of(1), Some(1));
    }

    #[test]
    fn an_edited_row_survives_remove_and_reinsert() {
        let mut s = open(b"a,1\nb,2\n", false);
        s.set_cell(1, 1, "EDITED");
        let row = s.remove_row(1).unwrap();
        assert_eq!(row.1.get(1), Some("EDITED"));
        s.insert_row(0, row);
        assert_eq!(ids(&s), vec![1, 0]);
        assert_eq!(strs(&s, 0), Some(vec!["b".into(), "EDITED".into()]));
    }

    #[test]
    fn insert_at_start_middle_end_and_past_the_end() {
        let mut s = open(b"a\nb\n", false);
        s.insert_row(0, (10, StringRecord::from(vec!["start"])));
        s.insert_row(2, (11, StringRecord::from(vec!["mid"])));
        s.insert_row(4, (12, StringRecord::from(vec!["end"])));
        s.insert_row(99, (13, StringRecord::from(vec!["clamped"])));
        assert_eq!(
            first_cells(&s),
            ["start", "a", "mid", "b", "end", "clamped"]
        );
        assert_eq!(ids(&s), vec![10, 0, 11, 1, 12, 13]);
        assert_eq!(s.next_row_id(), 14);
    }

    #[test]
    fn an_inserted_row_is_editable_and_removable() {
        let mut s = open(b"a\n", false);
        s.insert_row(1, (5, StringRecord::from(vec!["x", "y"])));
        assert!(s.set_cell(5, 1, "Y"));
        assert_eq!(strs(&s, 1), Some(vec!["x".into(), "Y".into()]));
        assert_eq!(s.remove_row(1).unwrap().0, 5);
        assert_eq!(s.position_of(5), None);
        assert!(!s.set_cell(5, 0, "gone"));
    }

    #[test]
    fn a_removed_file_row_cannot_be_edited_until_it_is_reinserted() {
        let mut s = open(b"a\nb\n", false);
        let row = s.remove_row(0).unwrap();
        assert!(!s.set_cell(0, 0, "x"));
        s.insert_row(0, row);
        assert!(s.set_cell(0, 0, "x"));
    }

    #[test]
    fn the_order_list_is_only_built_by_the_first_structural_edit() {
        let mut s = open(b"a\nb\n", false);
        s.set_cell(0, 0, "edit");
        s.filter("a");
        s.snapshot();
        assert!(!s.order_is_materialized());
        s.remove_row(0);
        assert!(s.order_is_materialized());
    }

    #[test]
    fn rows_cached_before_a_shift_are_not_re_read_after_it() {
        use std::sync::atomic::Ordering::Relaxed;
        let src = Arc::new(Counting(
            VecSource(b"a\nb\nc\n".to_vec()),
            Default::default(),
        ));
        let mut s = FileSheet::with_cache_capacity(src.clone(), b',', false, 8).unwrap();
        s.cell(2, 0); // cache row id 2 while it is at position 2
        s.remove_row(0); // row id 2 is now at position 1 (this reads row 0)
        let before = src.1.load(Relaxed);
        assert_eq!(s.cell(1, 0).as_deref(), Some("c"));
        assert_eq!(
            src.1.load(Relaxed),
            before,
            "served from the id-keyed cache"
        );
    }

    #[test]
    fn a_snapshot_keeps_the_old_order() {
        let mut s = open(b"a\nb\nc\n", false);
        let snap = s.snapshot();
        s.remove_row(0);
        s.insert_row(0, (9, StringRecord::from(vec!["new"])));
        assert_eq!(snap.len(), 3);
        assert_eq!(snap.cell(0, 0).as_deref(), Some("a"));
        assert_eq!(s.cell(0, 0).as_deref(), Some("new"));
    }

    #[test]
    fn removing_a_column_returns_values_in_display_order_for_every_kind_of_row() {
        let mut s = open(b"a,1\nb,2\nc,3\n", false);
        s.set_cell(2, 1, "EDITED");
        s.insert_row(1, (9, StringRecord::from(vec!["ins", "ert"])));
        let removed = s.remove_column(1);
        assert_eq!(
            removed,
            vec![
                (0, "1".to_string()),
                (9, "ert".to_string()),
                (1, "2".to_string()),
                (2, "EDITED".to_string())
            ]
        );
        assert_eq!(first_cells(&s), ["a", "ins", "b", "c"]);
        assert_eq!(s.cell(0, 1), None);
    }

    #[test]
    fn edits_made_before_a_column_op_follow_it() {
        let mut s = open(b"a,b,c\n", false);
        s.set_cell(0, 2, "C!");
        s.remove_column(0);
        assert_eq!(strs(&s, 0), Some(vec!["b".into(), "C!".into()]));
        s.insert_column(0, &[]);
        assert_eq!(strs(&s, 0), Some(vec!["".into(), "b".into(), "C!".into()]));
    }

    #[test]
    fn cell_edits_after_a_column_op_address_the_new_columns() {
        let mut s = open(b"a,b,c\n", false);
        s.remove_column(0);
        assert!(s.set_cell(0, 0, "B"));
        assert!(!s.set_cell(0, 2, "no"), "only two columns are left");
        assert_eq!(strs(&s, 0), Some(vec!["B".into(), "c".into()]));
    }

    #[test]
    fn a_cached_row_is_not_served_stale_after_a_column_op() {
        let mut s = open(b"a,b\nc,d\n", false);
        assert_eq!(s.cell(0, 0).as_deref(), Some("a")); // cached
        s.remove_column(0);
        assert_eq!(s.cell(0, 0).as_deref(), Some("b"));
        s.insert_column(0, &[]);
        assert_eq!(s.cell(0, 0).as_deref(), Some(""));
    }

    #[test]
    fn a_snapshot_never_sees_rows_cached_under_a_newer_column_layout() {
        let mut s = open(b"a,b\n", false);
        let snap = s.snapshot();
        s.remove_column(0);
        assert_eq!(s.cell(0, 0).as_deref(), Some("b")); // fills the shared cache
        assert_eq!(snap.cell(0, 0).as_deref(), Some("a"));
        assert_eq!(snap.cell(0, 1).as_deref(), Some("b"));
    }

    #[test]
    fn filter_and_sort_use_the_current_columns() {
        let mut s = open(b"x,3\ny,1\nz,2\n", false);
        s.remove_column(0);
        assert_eq!(s.filter("x"), Vec::<u32>::new());
        assert_eq!(s.filter("3"), vec![0]);
        assert_eq!(s.sort(vec![0, 1, 2], (0, SortOrder::Asc)), vec![1, 2, 0]);
    }

    #[test]
    fn column_edit_contract_holds() {
        conformance::run_column_edits(MAKE);
    }

    #[test]
    fn the_whole_sheet_contract_holds() {
        conformance::run_all(MAKE);
    }

    #[test]
    fn row_edit_contract_holds() {
        conformance::run_row_edits(MAKE);
    }

    /// ~4 MB of rows, so a scan needs several windows and every window
    /// boundary lands somewhere different.
    fn big_rows(n: usize) -> Vec<Vec<String>> {
        (0..n)
            .map(|i| {
                vec![
                    i.to_string(),
                    format!("user{i}"),
                    format!("city{}", i % 97),
                    format!("note, with comma {i}"),
                    format!("say \"{}\"", i % 13),
                ]
            })
            .collect()
    }

    fn big_sheet(rows: &[Vec<String>]) -> FileSheet {
        let mut w = csv::WriterBuilder::new().has_headers(false).from_writer(vec![]);
        for r in rows {
            w.write_record(r).unwrap();
        }
        open(&w.into_inner().unwrap(), false)
    }

    fn naive_filter(rows: &[Vec<String>], needle: &str) -> Vec<u32> {
        (0..rows.len()).filter(|&i| rows[i].iter().any(|c| c.contains(needle))).map(|i| i as u32).collect()
    }

    #[test]
    fn filtering_across_many_scan_windows_matches_a_naive_search() {
        let rows = big_rows(50_000);
        let sheet = big_sheet(&rows);
        for needle in ["user3", "city96", "comma 4421", "x", "\"7\"", "say \"1", "0,", "user49999", "zzz"] {
            assert_eq!(sheet.filter(needle), naive_filter(&rows, needle), "needle {needle:?}");
        }
    }

    #[test]
    fn edited_rows_are_judged_by_their_edited_text_across_windows() {
        let mut rows = big_rows(50_000);
        let mut sheet = big_sheet(&rows);
        for (id, col, text) in [(0usize, 1usize, "needle-a"), (24_999, 3, "needle-a, here"), (49_999, 2, "xx"), (1234, 1, "user99999")] {
            assert!(sheet.set_cell(id, col, text));
            rows[id][col] = text.to_string();
        }
        for needle in ["needle-a", "user99999", "user1234", "xx", "city0", "comma 24999"] {
            assert_eq!(sheet.filter(needle), naive_filter(&rows, needle), "needle {needle:?}");
        }
    }

    #[test]
    fn a_needle_with_a_line_break_matches_only_inside_a_field() {
        // "x\ny" would match across the boundary of rows 0 and 1 in the raw
        // bytes, but no field contains it.
        let s = open(b"x\ny\n\"p\nq\",z\n", false);
        assert_eq!(s.filter("x\ny"), Vec::<u32>::new());
        assert_eq!(s.filter("p\nq"), vec![2]);
    }

    #[test]
    fn rows_longer_than_a_scan_window_and_than_a_block_read_correctly() {
        let huge = "y".repeat(2_500_000);
        let mut data = b"a,1\n".to_vec();
        data.extend_from_slice(format!("\"{huge}\",2\nb,3\n").as_bytes());
        let s = open(&data, false);
        assert_eq!(s.len(), 3);
        assert_eq!(s.cell(1, 0).unwrap().len(), 2_500_000);
        assert_eq!(s.cell(2, 0).as_deref(), Some("b"));
        assert_eq!(s.filter("yyyy"), vec![1]);
        assert_eq!(s.filter("b"), vec![2]);
        assert_eq!(s.sort(vec![0, 1, 2], (1, SortOrder::Dsc)), vec![2, 1, 0]);
    }

    #[test]
    fn rows_straddling_block_boundaries_read_back_exactly() {
        let rows = big_rows(3_000); // ~250 KB: several 64 KiB blocks
        let sheet = big_sheet(&rows);
        for (i, want) in rows.iter().enumerate() {
            assert_eq!(strs(&sheet, i).as_ref(), Some(want), "row {i}");
        }
    }

    #[test]
    fn cell_edit_contract_holds() {
        conformance::run_cell_edits(MAKE);
    }

    #[test]
    fn read_only_contract_holds() {
        conformance::run_read_only(MAKE);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn loads_exactly_what_a_memory_sheet_holds(initial in conformance::initial_rows()) {
            conformance::check_ops(MAKE, &initial, &[]);
        }

        #[test]
        fn random_cell_edits_match_the_reference_model(
            initial in conformance::initial_rows(),
            ops in conformance::cell_edit_ops(),
        ) {
            conformance::check_ops(MAKE, &initial, &ops);
        }

        #[test]
        fn random_column_row_and_cell_edits_match_the_reference_model(
            initial in conformance::initial_rows(),
            ops in conformance::ops(),
        ) {
            conformance::check_ops(MAKE, &initial, &ops);
        }

        #[test]
        fn random_row_edits_match_the_reference_model(
            initial in conformance::initial_rows(),
            ops in conformance::row_ops(),
        ) {
            conformance::check_ops(MAKE, &initial, &ops);
        }

        #[test]
        fn filter_and_sort_match_a_memory_sheet(
            initial in conformance::initial_rows(),
            needle in prop_oneof!["[a-c0-9,\" ]{0,2}", Just(String::new())],
            col in 0usize..4,
            dsc: bool,
        ) {
            conformance::check_filter_sort(MAKE, &initial, &needle, col, dsc);
            // and with a needle lifted from the data, so it usually matches
            if let Some(cell) = initial.first().and_then(|r| r.first()) {
                conformance::check_filter_sort(MAKE, &initial, cell, col, dsc);
            }
        }
    }
}
