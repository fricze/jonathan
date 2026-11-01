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
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use csv::StringRecord;

use crate::lean::LeanParser;
use crate::pieces::Pieces;
use crate::{
    InsertedColumn, RemovedColumn, ReplacedAll, RowIndex, RowIndexBuilder, RowScope, Sheet,
    SheetRow, SortOrder, View, sort_cells,
};

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

/// Where a logical column's data comes from.
#[derive(Clone, Debug)]
enum ColSource {
    /// Field `i` of the row as stored in the file.
    Original(usize),
    /// A column added since load; the value for each row id (empty if absent).
    /// `id` is unique for the life of the process (not an address: addresses
    /// are reused after a free, which would let a rule meant for a removed
    /// column reach a new one).
    Added(u64, Arc<HashMap<usize, String>>),
}

/// How the length of a row changes under the column operations so far, as a
/// function of its stored length `k`: `f(k) = base + k - |{z in flat : z < k}|`.
///
/// Needed because padding is real: inserting a column at 3 into a 0-field row
/// leaves 3 empty fields behind even if that column is later removed, so the
/// length a row ends up with depends on the order of operations and on how
/// long the row was. Each operation is a monotone step function (the length
/// grows by 0 or 1 when `k` grows by 1), and so are their compositions;
/// `flat` lists the `k` where it grows by 0. It stays as small as the widest
/// column ever touched.
#[derive(Clone, Debug, Default)]
struct LenFn {
    base: usize,
    flat: Vec<usize>,
}

impl LenFn {
    fn is_identity(&self) -> bool {
        self.base == 0 && self.flat.is_empty()
    }

    fn apply(&self, k: usize) -> usize {
        self.base + k - self.flat.partition_point(|&z| z < k)
    }

    /// Follow with "insert a column at `c`": `x -> max(x, c) + 1`, which grows
    /// by 0 where `x < c`.
    fn then_insert(&mut self, c: usize) {
        let mut flat = self.flat.clone();
        let mut k = 0;
        while self.apply(k) < c {
            if self.apply(k + 1) > self.apply(k) {
                flat.push(k);
            }
            k += 1;
        }
        flat.sort_unstable();
        flat.dedup();
        self.base = self.base.max(c) + 1;
        self.flat = flat;
    }

    /// Follow with "remove column `c`": `x -> x - (x > c)`, which grows by 0
    /// where `x == c`.
    fn then_remove(&mut self, c: usize) {
        let mut flat = self.flat.clone();
        let mut k = 0;
        while self.apply(k) <= c {
            if self.apply(k) == c && self.apply(k + 1) > c {
                flat.push(k);
            }
            k += 1;
        }
        flat.sort_unstable();
        flat.dedup();
        self.base -= usize::from(self.base > c);
        self.flat = flat;
    }
}

/// Identity of a source column, stable while columns move around: a rule is
/// attached to this, not to a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SrcKey {
    Original(usize),
    /// An added column, by the address of its (shared, never mutated) values.
    Added(u64),
}

impl ColSource {
    /// A new added column with these per-row values, with a fresh identity.
    fn added(values: HashMap<usize, String>) -> ColSource {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        ColSource::Added(NEXT.fetch_add(1, Relaxed), Arc::new(values))
    }

    fn key(&self) -> SrcKey {
        match self {
            ColSource::Original(i) => SrcKey::Original(*i),
            ColSource::Added(id, _) => SrcKey::Added(*id),
        }
    }
}

/// The rows a replace-all rule covers.
#[derive(Clone, Debug)]
enum RuleRows {
    All,
    /// Sorted, unique ids.
    Ids(Arc<Vec<usize>>),
}

impl RuleRows {
    fn covers(&self, id: usize) -> bool {
        match self {
            RuleRows::All => true,
            RuleRows::Ids(ids) => ids.binary_search(&id).is_ok(),
        }
    }
}

/// "Replace `find` by `replace` in these source columns of these rows",
/// applied whenever a field is read from the file. A replace-all over a huge
/// file is one of these, not a million edited cells (design 4.10).
#[derive(Clone, Debug)]
struct Rule {
    rows: RuleRows,
    sources: Vec<SrcKey>,
    find: Vec<u8>,
    replace: Vec<u8>,
}

/// `text` with every `find` replaced; `None` if there was none.
fn replace_bytes(text: &[u8], find: &[u8], replace: &[u8]) -> Option<Vec<u8>> {
    let mut matches = memchr::memmem::find_iter(text, find).peekable();
    matches.peek()?;
    let mut out = Vec::with_capacity(text.len());
    let mut last = 0;
    for at in matches {
        out.extend_from_slice(&text[last..at]);
        out.extend_from_slice(replace);
        last = at + find.len();
    }
    out.extend_from_slice(&text[last..]);
    Some(out)
}

/// The file's columns as the user sees them, after inserts and removes.
///
/// A row is an unbounded sequence of fields, empty past its own length (the
/// contract's "short rows are padded"). `entries` describes logical columns
/// `0..entries.len()`; every column after those is the file's own, continuing
/// from original column `tail_start` (so removing column 3 of a 10-column file
/// is `entries = [O0, O1, O2]`, `tail_start = 4`: O4.. follow). `len` says how
/// long each row is. Reading a field is a lookup, whatever number of columns
/// were added or removed. The whole map is small (it grows with the widest
/// column touched, not with the file), so undo keeps a copy of it.
#[derive(Clone, Debug, Default)]
struct ColumnMap {
    entries: Vec<ColSource>,
    tail_start: usize,
    len: LenFn,
    /// Replace-all rules, oldest first.
    rules: Vec<Rule>,
}

impl ColumnMap {
    fn is_identity(&self) -> bool {
        self.entries.is_empty()
            && self.tail_start == 0
            && self.len.is_identity()
            && self.rules.is_empty()
    }

    /// Memory this map keeps alive. (An `Added` column's value table is shared
    /// and counted where it was made, but a token holding the only reference
    /// does keep it alive; `added_values_bytes` is added by callers that know.)
    fn size_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.entries.capacity() * std::mem::size_of::<ColSource>()
            + self.len.flat.capacity() * std::mem::size_of::<usize>()
            + self
                .rules
                .iter()
                .map(|r| {
                    std::mem::size_of::<Rule>()
                        + r.sources.capacity() * std::mem::size_of::<SrcKey>()
                        + r.find.capacity()
                        + r.replace.capacity()
                        + match &r.rows {
                            RuleRows::All => 0,
                            RuleRows::Ids(ids) => ids.capacity() * std::mem::size_of::<usize>(),
                        }
                })
                .sum::<usize>()
    }

    /// Some field's text is not what the file's bytes say (an added column
    /// with values, or a replace-all rule), so a search of the bytes cannot
    /// rule a row out.
    fn bytes_do_not_tell_the_whole_story(&self) -> bool {
        !self.rules.is_empty()
            || self
                .entries
                .iter()
                .any(|e| matches!(e, ColSource::Added(_, v) if !v.is_empty()))
    }

    /// Identity of the source at logical column `col`.
    fn source_key(&self, col: usize) -> SrcKey {
        match self.entries.get(col) {
            Some(source) => source.key(),
            None => SrcKey::Original(self.tail_start + (col - self.entries.len())),
        }
    }

    /// Add a replace-all rule for the sources currently at logical `cols`.
    fn add_rule(&mut self, rows: RuleRows, cols: &[usize], find: &str, replace: &str) {
        let mut sources: Vec<SrcKey> = cols.iter().map(|&c| self.source_key(c)).collect();
        sources.dedup();
        self.rules.push(Rule {
            rows,
            sources,
            find: find.as_bytes().to_vec(),
            replace: replace.as_bytes().to_vec(),
        });
    }

    /// `base`, the text a source has in row `id`, after the rules.
    fn apply_rules<'a>(&self, id: usize, key: SrcKey, base: &'a [u8]) -> Cow<'a, [u8]> {
        let mut text = Cow::Borrowed(base);
        for rule in &self.rules {
            if rule.rows.covers(id) && rule.sources.contains(&key) {
                if let Some(new) = replace_bytes(&text, &rule.find, &rule.replace) {
                    text = Cow::Owned(new);
                }
            }
        }
        text
    }

    /// Spell out the first `upto` logical columns in `entries`.
    fn materialize(&mut self, upto: usize) {
        while self.entries.len() < upto {
            self.entries.push(ColSource::Original(self.tail_start));
            self.tail_start += 1;
        }
    }

    /// Trailing entries that are just the file's own next columns carry no
    /// information; dropping them keeps the map as small as it can be.
    fn normalize(&mut self) {
        while let Some(ColSource::Original(i)) = self.entries.last() {
            if *i + 1 != self.tail_start {
                break;
            }
            self.entries.pop();
            self.tail_start -= 1;
        }
    }

    fn remove(&mut self, col: usize) -> ColSource {
        self.materialize(col + 1);
        let removed = self.entries.remove(col);
        self.normalize();
        self.len.then_remove(col);
        removed
    }

    fn insert(&mut self, col: usize, source: ColSource) {
        self.materialize(col);
        self.entries.insert(col, source);
        self.normalize();
        self.len.then_insert(col);
    }

    fn field<'a>(&'a self, id: usize, raw: &[&'a [u8]], col: usize) -> Cow<'a, [u8]> {
        let (key, base): (SrcKey, &'a [u8]) = match self.entries.get(col) {
            Some(source @ ColSource::Original(i)) => {
                (source.key(), raw.get(*i).copied().unwrap_or(b""))
            }
            Some(source @ ColSource::Added(_, values)) => (
                source.key(),
                values.get(&id).map_or(&b""[..], |v| v.as_bytes()),
            ),
            None => {
                let i = self.tail_start + (col - self.entries.len());
                (SrcKey::Original(i), raw.get(i).copied().unwrap_or(b""))
            }
        };
        self.apply_rules(id, key, base)
    }

    /// The row's fields as the user sees it, given its stored fields.
    fn logical<'a>(&'a self, id: usize, raw: &[&'a [u8]]) -> Vec<Cow<'a, [u8]>> {
        (0..self.len.apply(raw.len()))
            .map(|col| self.field(id, raw, col))
            .collect()
    }

    /// Just field `col` of the row (`None` where the row has no such field).
    fn logical_field<'a>(
        &'a self,
        id: usize,
        raw: &[&'a [u8]],
        col: usize,
    ) -> Option<Cow<'a, [u8]>> {
        (col < self.len.apply(raw.len())).then(|| self.field(id, raw, col))
    }
}

/// What a file-backed `remove_column` hands back for `restore_column`: the
/// column map as it was before the removal (small, whatever the file's
/// length), the removed column's source, and the column's value in each row
/// that had been copied into memory (those rows do not read it from the file).
struct FileColumn {
    before: ColumnMap,
    source: ColSource,
    edited: Vec<(usize, Option<String>)>,
}

/// What a file-backed `replace_all` hands back for `undo_replace_all`: the
/// column map before it (rules are part of the map) and the previous record of
/// every row held in memory that it rewrote.
struct FileReplace {
    before: ColumnMap,
    edited: Vec<(usize, StringRecord)>,
}

/// What a file-backed `insert_column` hands back for `undo_insert_column`: the
/// column map before the insertion and the length each in-memory row had.
struct FileInsert {
    before: ColumnMap,
    edited: Vec<(usize, usize)>,
}

fn record_without(row: &StringRecord, col: usize) -> StringRecord {
    row.iter()
        .enumerate()
        .filter(|(i, _)| *i != col)
        .map(|(_, f)| f)
        .collect()
}

/// `row` with `value` inserted at `col`, padding a shorter row first.
fn record_with(row: &StringRecord, col: usize, value: &str) -> StringRecord {
    let mut fields: Vec<&str> = row.iter().collect();
    fields.resize(fields.len().max(col), "");
    fields.insert(col, value);
    fields.into_iter().collect()
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
    /// Display order, as runs of consecutive row ids (see `pieces.rs`): one
    /// run for an untouched file, a few more per insert or remove. Shared with
    /// snapshots until either side edits it.
    pieces: Arc<Pieces>,
    /// File rows (ids below the row count) that have been removed.
    removed: HashSet<usize>,
    /// The columns as the user sees them. Rows read from the file are mapped
    /// through it; rows in `edited` already are (updated when a column is
    /// added or removed). Shared with snapshots.
    columns: Arc<ColumnMap>,
    /// Bumped on every column change; part of the row cache key, because the
    /// cache is shared with snapshots that may see an older column layout.
    col_version: usize,
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
        let index = builder.finish();
        let row_count = index.len();
        Ok(FileSheet {
            source,
            index: Arc::new(index),
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
            pieces: Arc::new(Pieces::identity(row_count)),
            removed: HashSet::new(),
            columns: Arc::new(ColumnMap::default()),
            col_version: 0,
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

    /// Read and parse file row `file_row` into a record, as the user sees it
    /// (columns mapped). `None` if there is no such row or its bytes can no
    /// longer be read.
    fn read_row(&self, file_row: usize) -> Option<StringRecord> {
        self.read_row_with(file_row, true)
    }

    /// Like `read_row`, but with the fields exactly as stored in the file.
    fn read_stored_row(&self, file_row: usize) -> Option<StringRecord> {
        self.read_row_with(file_row, false)
    }

    fn read_row_with(&self, file_row: usize, mapped: bool) -> Option<StringRecord> {
        let bytes = self.row_bytes(self.index.row_range(file_row)?)?;
        PARSER.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.as_ref().is_none_or(|(d, _)| *d != self.delimiter) {
                *slot = Some((self.delimiter, LeanParser::new(self.delimiter)));
            }
            let parser = &mut slot.as_mut()?.1;
            let fields = parser.parse(bytes.as_slice())?;
            let mut raw = csv::ByteRecord::new();
            if mapped && !self.columns.is_identity() {
                let stored: Vec<&[u8]> = fields.iter().collect();
                for f in self.columns.logical(file_row, &stored) {
                    raw.push_field(&f);
                }
            } else {
                for f in fields.iter() {
                    raw.push_field(f);
                }
            }
            Some(match StringRecord::from_byte_record(raw) {
                Ok(record) => record,
                Err(e) => StringRecord::from_byte_record_lossy(e.into_byte_record()),
            })
        })
    }

    /// Whether any field of file row `id` (columns mapped), parsed from
    /// `bytes`, contains what `finder` looks for.
    fn fields_contain(
        &self,
        parser: &mut LeanParser,
        bytes: &[u8],
        id: usize,
        finder: &memchr::memmem::Finder<'_>,
    ) -> bool {
        let Some(fields) = parser.parse(bytes) else {
            return false;
        };
        if self.columns.is_identity() {
            return fields.iter().any(|c| finder.find(c).is_some());
        }
        let stored: Vec<&[u8]> = fields.iter().collect();
        self.columns
            .logical(id, &stored)
            .iter()
            .any(|c| finder.find(c).is_some())
    }

    /// Row id at display `position`.
    fn id_at(&self, position: usize) -> Option<usize> {
        self.pieces.id_at(position)
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
        self.read_row(id).map(Cow::Owned)
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
        let key = (id, self.col_version);
        let lock = || self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().rows.get(&key) {
            return Some(hit.clone());
        }
        let row = Arc::new(self.read_row(id)?);
        lock().insert(key, row.clone());
        Some(row)
    }

    fn row(&self, position: usize) -> Option<Arc<StringRecord>> {
        self.row_by_id(self.id_at(position)?)
    }

    /// Visit the rows that come from the file (present in the sheet and not held
    /// in memory) in display order, or just those with the given ids, with their
    /// fields as the user sees them. With a `prefilter`, rows whose bytes do not
    /// contain it are skipped unparsed; only sound when no field's text can differ
    /// from the file's bytes (see `bytes_do_not_tell_the_whole_story`).
    fn visit_file_rows(
        &self,
        ids: Option<&[usize]>,
        prefilter: Option<&memchr::memmem::Finder<'_>>,
        mut f: impl FnMut(usize, &[Cow<'_, [u8]>]),
    ) {
        let file_rows = self.index.len();
        let mut window = ScanWindow::new();
        let mut parser = LeanParser::new(self.delimiter);
        let mut visit = |id: usize| {
            if self.edited.contains_key(&id) {
                return;
            }
            let Some(bytes) = self.index.row_range(id).and_then(|r| window.get(self, r)) else {
                return;
            };
            if prefilter.is_some_and(|p| p.find(bytes).is_none()) {
                return;
            }
            let Some(fields) = parser.parse(bytes) else {
                return;
            };
            let stored: Vec<&[u8]> = fields.iter().collect();
            f(id, &self.columns.logical(id, &stored));
        };
        match ids {
            None => {
                for (_, run) in self.pieces.runs() {
                    for id in run.first..(run.first + run.len).min(file_rows) {
                        visit(id);
                    }
                }
            }
            Some(ids) => {
                for &id in ids {
                    if id < file_rows && self.pieces.position_of(id).is_some() {
                        visit(id);
                    }
                }
            }
        }
    }

    /// Remember `record` as the content of row `id`, unless it is exactly what
    /// the file gives for that row (through the column map): then the row is
    /// just the file's again and nothing needs to stay in memory. This is what
    /// returns the memory when an edit is undone, and what makes a row put
    /// back unchanged a plain file row once more.
    fn keep_row(&mut self, id: usize, record: StringRecord) {
        if id < self.index.len() && self.read_row(id).as_ref() == Some(&record) {
            self.edited.remove(&id);
            self.removed.remove(&id);
        } else {
            self.edited.insert(id, Arc::new(record));
        }
    }

    /// The value a file column has in row `id`, as it was in the file (what a
    /// restored column puts back into rows that were copied into memory
    /// after the column was removed).
    fn stored_value(&self, before: &ColumnMap, source: &ColSource, id: usize) -> Option<String> {
        let text = match source {
            ColSource::Original(i) if id < self.index.len() => {
                self.read_stored_row(id)?.get(*i)?.to_string()
            }
            ColSource::Original(_) => return None,
            ColSource::Added(_, values) => values.get(&id).cloned().unwrap_or_default(),
        };
        // The rules in force when the column was removed apply to its text.
        let ruled = before.apply_rules(id, source.key(), text.as_bytes());
        Some(String::from_utf8_lossy(&ruled).into_owned())
    }
}

#[cfg(test)]
impl FileSheet {
    fn cached_rows(&self) -> usize {
        self.cache.lock().unwrap().rows.len()
    }

    /// More than the single run of an untouched file.
    fn order_is_materialized(&self) -> bool {
        !self.pieces.is_identity(self.index.len())
    }

    /// Rows currently held in memory (edited, inserted or put back).
    fn rows_in_memory(&self) -> usize {
        self.edited.len()
    }

    fn order_bytes(&self) -> usize {
        self.pieces.size_bytes()
    }
}

impl Sheet for FileSheet {
    fn len(&self) -> usize {
        self.pieces.len()
    }

    fn row_id(&self, position: usize) -> Option<usize> {
        self.id_at(position)
    }

    fn position_of(&self, row_id: usize) -> Option<usize> {
        self.pieces.position_of(row_id)
    }

    fn next_row_id(&self) -> usize {
        self.pieces.next_id()
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
        self.keep_row(row_id, rewritten);
        true
    }

    fn remove_row(&mut self, position: usize) -> Option<SheetRow> {
        // Read first: a row that can't be read can't be handed back for
        // undo, so it stays where it is.
        let id = self.id_at(position)?;
        let record = (*self.row_by_id(id)?).clone();
        Arc::make_mut(&mut self.pieces).remove_at(position);
        self.edited.remove(&id);
        if id < self.index.len() {
            self.removed.insert(id);
        }
        Some((id, record))
    }

    fn insert_row(&mut self, position: usize, (id, record): SheetRow) {
        Arc::make_mut(&mut self.pieces).insert_at(position, id);
        self.keep_row(id, record);
    }

    fn remove_column(&mut self, col: usize) -> RemovedColumn {
        // Rows copied into memory are updated now (remembering their value);
        // every other row reads through the column map, so nothing in the
        // file is touched, however large it is.
        let mut edited = Vec::with_capacity(self.edited.len());
        for (id, row) in self.edited.iter_mut() {
            let value = row.get(col).map(str::to_string);
            if value.is_some() {
                *row = Arc::new(record_without(row, col));
            }
            edited.push((*id, value));
        }
        let before = (*self.columns).clone();
        let source = Arc::make_mut(&mut self.columns).remove(col);
        self.col_version += 1;
        let size = before.size_bytes()
            + std::mem::size_of::<FileColumn>()
            + edited.capacity() * std::mem::size_of::<(usize, Option<String>)>()
            + edited
                .iter()
                .map(|(_, v)| v.as_ref().map_or(0, |s| s.capacity()))
                .sum::<usize>();
        RemovedColumn::new(
            FileColumn {
                before,
                source,
                edited,
            },
            size,
        )
    }

    fn restore_column(&mut self, col: usize, removed: &RemovedColumn) {
        let Some(token) = removed.payload::<FileColumn>() else {
            return;
        };
        let remembered: HashMap<usize, &Option<String>> =
            token.edited.iter().map(|(id, v)| (*id, v)).collect();
        let ids: Vec<usize> = self.edited.keys().copied().collect();
        for id in ids {
            // A row that was already in memory when the column was removed
            // remembers its value; one copied in since (and returned to its
            // earlier state by the undo stack) takes it from the file.
            let value = match remembered.get(&id) {
                Some(v) => (*v).clone(),
                None => self.stored_value(&token.before, &token.source, id),
            };
            if let (Some(value), Some(row)) = (value, self.edited.get_mut(&id)) {
                *row = Arc::new(record_with(row, col, &value));
            }
        }
        // Under the undo-stack discipline the map is exactly as it was right
        // after the removal, so putting the old one back is the exact inverse.
        self.columns = Arc::new(token.before.clone());
        self.col_version += 1;
    }

    fn insert_column(&mut self, col: usize, values_by_row: &[(usize, String)]) -> InsertedColumn {
        let values: HashMap<usize, String> = values_by_row.iter().cloned().collect();
        let before = (*self.columns).clone();
        let mut edited = Vec::with_capacity(self.edited.len());
        for (id, row) in self.edited.iter_mut() {
            edited.push((*id, row.len()));
            *row = Arc::new(record_with(
                row,
                col,
                values.get(id).map_or("", String::as_str),
            ));
        }
        Arc::make_mut(&mut self.columns).insert(col, ColSource::added(values));
        self.col_version += 1;
        let size = before.size_bytes()
            + std::mem::size_of::<FileInsert>()
            + edited.capacity() * std::mem::size_of::<(usize, usize)>();
        InsertedColumn::new(FileInsert { before, edited }, size)
    }

    fn undo_insert_column(&mut self, col: usize, inserted: &InsertedColumn) {
        let Some(token) = inserted.payload::<FileInsert>() else {
            return;
        };
        let remembered: HashMap<usize, usize> = token.edited.iter().copied().collect();
        let ids: Vec<usize> = self.edited.keys().copied().collect();
        for id in ids {
            // A row that was in memory when the column went in remembers its
            // old length. One copied in since (and returned to its earlier
            // state by the undo stack) is a file row: its old length follows
            // from the stored length and the map as it was.
            let old_len = match remembered.get(&id) {
                Some(&len) => Some(len),
                None if id < self.index.len() => self
                    .read_stored_row(id)
                    .map(|stored| token.before.len.apply(stored.len())),
                None => None,
            };
            let Some(row) = self.edited.get(&id) else {
                continue;
            };
            let mut rebuilt = if col < row.len() {
                record_without(row, col)
            } else {
                (**row).clone()
            };
            if let Some(len) = old_len {
                rebuilt = rebuilt.iter().take(len).collect();
            }
            self.edited.insert(id, Arc::new(rebuilt));
        }
        // Under the undo-stack discipline the map is exactly as it was right
        // after the insertion, so putting the old one back is the exact inverse.
        self.columns = Arc::new(token.before.clone());
        self.col_version += 1;
    }

    // Scans bypass the shared caches so they cannot evict what the table is
    // showing. They walk the display order run by run: each run of consecutive
    // ids is a stretch of the file that can be read front to back through a
    // large window, parsed with the lean parser, and (for filter) searched as
    // raw bytes first. Rows held in memory are judged from memory instead.
    fn filter(&self, needle: &str) -> View {
        if needle.is_empty() {
            return (0..self.len() as u32).collect();
        }
        let finder = memchr::memmem::Finder::new(needle.as_bytes());
        // A field's text appears verbatim in the row's bytes unless it contains
        // a quote (stored doubled), so a needle without one can rule out a row
        // by searching bytes alone, no parsing. Added columns hold values that
        // are not in the bytes at all.
        let prefilter =
            !needle.as_bytes().contains(&b'"') && !self.columns.bytes_do_not_tell_the_whole_story();
        // One search per window of many rows is sound when a match cannot span
        // two rows, i.e. the needle has no line break.
        let by_window = prefilter && !needle.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r');
        let mut edited_ids: Vec<usize> = self.edited.keys().copied().collect();
        edited_ids.sort_unstable();
        let file_rows = self.index.len();
        let mut parser = LeanParser::new(self.delimiter);
        let mut hits = View::new();
        for (start, run) in self.pieces.runs() {
            let first_hit = hits.len();
            let position = |id: usize| (start + id - run.first) as u32;
            let file_end = (run.first + run.len).min(file_rows);
            if by_window {
                let mut buf = Vec::new();
                let mut row = run.first;
                while row < file_end {
                    // The window: whole rows from `row` up to about SCAN_WINDOW bytes.
                    let first = self.index.row_range(row).unwrap();
                    let target = first.start + SCAN_WINDOW;
                    let last = match self
                        .index
                        .row_at_offset(target.min(self.index.total_len() - 1))
                    {
                        Some(last) if target < self.index.total_len() => last,
                        _ => file_rows - 1,
                    };
                    let last = last.clamp(row, file_end - 1);
                    let end = self.index.row_range(last).unwrap().end;
                    buf.resize((end - first.start) as usize, 0);
                    if !self.read_exact_at(first.start, &mut buf) {
                        break; // the file went away; report what was found so far
                    }
                    let mut at = 0usize;
                    while let Some(found) = finder.find(&buf[at..]) {
                        let offset = first.start + (at + found) as u64;
                        let Some(id) = self.index.row_at_offset(offset) else {
                            break;
                        };
                        let range = self.index.row_range(id).unwrap();
                        if !self.edited.contains_key(&id) {
                            let bytes = &buf[(range.start - first.start) as usize
                                ..(range.end - first.start) as usize];
                            if self.fields_contain(&mut parser, bytes, id, &finder) {
                                hits.push(position(id));
                            }
                        }
                        at = (range.end - first.start) as usize; // next row onward
                    }
                    row = last + 1;
                }
            } else {
                let mut window = ScanWindow::new();
                for id in run.first..file_end {
                    if self.edited.contains_key(&id) {
                        continue;
                    }
                    let hit = match self.index.row_range(id).and_then(|r| window.get(self, r)) {
                        Some(bytes) if !prefilter || finder.find(bytes).is_some() => {
                            self.fields_contain(&mut parser, bytes, id, &finder)
                        }
                        _ => false,
                    };
                    if hit {
                        hits.push(position(id));
                    }
                }
            }
            // Rows held in memory (edited, inserted, put back) are judged by
            // their own text, never by the file's.
            let lo = edited_ids.partition_point(|&id| id < run.first);
            let hi = edited_ids.partition_point(|&id| id < run.first + run.len);
            for &id in &edited_ids[lo..hi] {
                if self.edited[&id].iter().any(|c| c.contains(needle)) {
                    hits.push(position(id));
                }
            }
            hits[first_hit..].sort_unstable();
        }
        hits
    }

    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View {
        // Copy the sort column's text into one arena (one allocation, not one
        // per row), then sort `(text, position)` pairs borrowed from it.
        let mut window = ScanWindow::new();
        let mut parser = LeanParser::new(self.delimiter);
        let mut arena = String::new();
        let mut spans = Vec::with_capacity(view.len());
        for p in view {
            let start = arena.len();
            if let Some(id) = self.id_at(p as usize) {
                match self.edited.get(&id) {
                    Some(row) => arena.push_str(row.get(sort_by.0).unwrap_or("")),
                    None => {
                        let fields = self
                            .index
                            .row_range(id)
                            .and_then(|r| window.get(self, r))
                            .and_then(|bytes| parser.parse(bytes));
                        if let Some(fields) = fields {
                            let cell = if self.columns.is_identity() {
                                fields.get(sort_by.0).map(Cow::Borrowed)
                            } else {
                                let stored: Vec<&[u8]> = fields.iter().collect();
                                self.columns.logical_field(id, &stored, sort_by.0)
                            };
                            if let Some(cell) = cell {
                                arena.push_str(&String::from_utf8_lossy(&cell));
                            }
                        }
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

    fn replace_all(
        &mut self,
        rows: RowScope<'_>,
        cols: &[usize],
        find: &str,
        replace: &str,
    ) -> (usize, ReplacedAll) {
        let before = (*self.columns).clone();
        let mut cols = cols.to_vec();
        cols.sort_unstable();
        cols.dedup();
        let mut changed = 0;
        let mut rewritten: Vec<(usize, StringRecord)> = Vec::new();
        if !find.is_empty() && find != replace && !cols.is_empty() {
            let ids: Option<Vec<usize>> = match rows {
                RowScope::All => None,
                RowScope::Ids(ids) => {
                    let mut ids = ids.to_vec();
                    ids.sort_unstable();
                    ids.dedup();
                    Some(ids)
                }
            };
            let covers = |id: usize| ids.as_ref().is_none_or(|v| v.binary_search(&id).is_ok());

            // Rows held in memory are rewritten now; the token keeps their old text.
            for (id, row) in self.edited.iter_mut() {
                if !covers(*id) {
                    continue;
                }
                let mut fields: Option<Vec<String>> = None;
                for &col in &cols {
                    let Some(old) = row.get(col) else { continue };
                    let new = old.replace(find, replace);
                    if new != old {
                        fields.get_or_insert_with(|| row.iter().map(str::to_string).collect())
                            [col] = new;
                        changed += 1;
                    }
                }
                if let Some(fields) = fields {
                    rewritten.push((*id, (**row).clone()));
                    *row = Arc::new(fields.iter().map(String::as_str).collect());
                }
            }

            // Everything else comes from the file: count what would change in
            // one streaming pass, and if anything would, record the rule.
            let finder = memchr::memmem::Finder::new(find.as_bytes());
            let prefilter = !find.as_bytes().contains(&b'"')
                && !self.columns.bytes_do_not_tell_the_whole_story();
            let mut from_file = 0;
            self.visit_file_rows(ids.as_deref(), prefilter.then_some(&finder), |_, fields| {
                for &col in &cols {
                    if let Some(cell) = fields.get(col) {
                        if replace_bytes(cell, find.as_bytes(), replace.as_bytes())
                            .is_some_and(|new| new != **cell)
                        {
                            from_file += 1;
                        }
                    }
                }
            });
            if from_file > 0 {
                let rule_rows = match ids {
                    None => RuleRows::All,
                    Some(ids) => RuleRows::Ids(Arc::new(ids)),
                };
                Arc::make_mut(&mut self.columns).add_rule(rule_rows, &cols, find, replace);
                changed += from_file;
            }
        }
        self.col_version += 1;
        let size = before.size_bytes()
            + std::mem::size_of::<FileReplace>()
            + rewritten.capacity() * std::mem::size_of::<(usize, StringRecord)>()
            + rewritten
                .iter()
                .map(|(_, r)| 96 + r.as_slice().len() + (r.len() + 1) * 8)
                .sum::<usize>();
        (
            changed,
            ReplacedAll::new(
                FileReplace {
                    before,
                    edited: rewritten,
                },
                size,
            ),
        )
    }

    fn undo_replace_all(&mut self, replaced: &ReplacedAll) {
        let Some(token) = replaced.payload::<FileReplace>() else {
            return;
        };
        // Under the undo-stack discipline the map is as the replace-all left
        // it, so the old one (rules included) is the exact inverse; the rows
        // that were in memory get their old text back.
        self.columns = Arc::new(token.before.clone());
        for (id, old) in &token.edited {
            self.edited.insert(*id, Arc::new(old.clone()));
        }
        self.col_version += 1;
    }

    fn edit_bytes(&self) -> usize {
        // Per row held in memory: the map entry, the Arc, the record's boxed
        // header and vectors, and its text and field ends.
        const PER_ROW_FIXED: usize = 24 + 16 + 96;
        let rows: usize = self
            .edited
            .values()
            .map(|r| {
                PER_ROW_FIXED + r.as_slice().len() + (r.len() + 1) * std::mem::size_of::<usize>()
            })
            .sum();
        rows + self.pieces.size_bytes() + self.columns.size_bytes() + self.removed.len() * 16
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
    fn restoring_a_column_brings_back_file_edited_and_inserted_rows_exactly() {
        let mut s = open(b"a,1\nb,2\nc,3\n", false);
        s.set_cell(2, 1, "EDITED");
        s.insert_row(1, (9, StringRecord::from(vec!["ins", "ert"])));
        let token = s.remove_column(1);
        assert_eq!(first_cells(&s), ["a", "ins", "b", "c"]);
        assert_eq!(s.cell(0, 1), None);
        s.restore_column(1, &token);
        let all: Vec<_> = (0..s.len()).map(|p| strs(&s, p).unwrap()).collect();
        assert_eq!(
            all,
            vec![
                vec!["a", "1"],
                vec!["ins", "ert"],
                vec!["b", "2"],
                vec!["c", "EDITED"]
            ]
        );
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
        let mut w = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(vec![]);
        for r in rows {
            w.write_record(r).unwrap();
        }
        open(&w.into_inner().unwrap(), false)
    }

    fn naive_filter(rows: &[Vec<String>], needle: &str) -> Vec<u32> {
        (0..rows.len())
            .filter(|&i| rows[i].iter().any(|c| c.contains(needle)))
            .map(|i| i as u32)
            .collect()
    }

    #[test]
    fn filtering_across_many_scan_windows_matches_a_naive_search() {
        let rows = big_rows(50_000);
        let sheet = big_sheet(&rows);
        for needle in [
            "user3",
            "city96",
            "comma 4421",
            "x",
            "\"7\"",
            "say \"1",
            "0,",
            "user49999",
            "zzz",
        ] {
            assert_eq!(
                sheet.filter(needle),
                naive_filter(&rows, needle),
                "needle {needle:?}"
            );
        }
    }

    #[test]
    fn filter_and_sort_after_deletes_report_positions_in_the_new_order() {
        // Runs far shorter than a scan window: the search must stop at the end
        // of each run, not carry on into the rows after a deleted one.
        let mut rows = big_rows(30_000);
        let mut sheet = big_sheet(&rows);
        for pos in [20_000usize, 7, 29_000, 15_000, 15_000, 3] {
            // the same deletions, applied to the sheet and the naive copy
            if pos < rows.len() {
                sheet.remove_row(pos).unwrap();
                rows.remove(pos);
            }
        }
        assert_eq!(sheet.len(), rows.len());
        for needle in [
            "user3",
            "city96",
            "comma 4421",
            "x",
            "say \"7\"",
            "0,",
            "zzz",
        ] {
            assert_eq!(
                sheet.filter(needle),
                naive_filter(&rows, needle),
                "needle {needle:?}"
            );
        }
        let view: View = (0..rows.len() as u32).collect();
        let sorted = sheet.sort(view, (0, SortOrder::Dsc));
        let want: Vec<u32> = (0..rows.len() as u32).rev().collect();
        assert_eq!(
            sorted, want,
            "ids are numeric, so descending is the reverse of the order"
        );
    }

    #[test]
    fn edited_rows_are_judged_by_their_edited_text_across_windows() {
        let mut rows = big_rows(50_000);
        let mut sheet = big_sheet(&rows);
        for (id, col, text) in [
            (0usize, 1usize, "needle-a"),
            (24_999, 3, "needle-a, here"),
            (49_999, 2, "xx"),
            (1234, 1, "user99999"),
        ] {
            assert!(sheet.set_cell(id, col, text));
            rows[id][col] = text.to_string();
        }
        for needle in [
            "needle-a",
            "user99999",
            "user1234",
            "xx",
            "city0",
            "comma 24999",
        ] {
            assert_eq!(
                sheet.filter(needle),
                naive_filter(&rows, needle),
                "needle {needle:?}"
            );
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

    // ---- ColumnMap, tested on its own against a naive per-row model ----

    /// One column operation, as applied to a single row by the padded
    /// semantics of the contract: insert pads a short row up to the column;
    /// removing a column a row doesn't reach changes nothing.
    #[derive(Clone, Debug)]
    enum MapOp {
        Remove(usize),
        Insert(usize, Option<String>),
        /// Replace `find` by `replace` in these columns of every row.
        Replace(Vec<usize>, String, String),
    }

    fn naive(raw: &[&str], ops: &[MapOp]) -> Vec<String> {
        let mut row: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
        for op in ops {
            match op {
                MapOp::Remove(c) => {
                    if *c < row.len() {
                        row.remove(*c);
                    }
                }
                MapOp::Insert(c, v) => {
                    while row.len() < *c {
                        row.push(String::new());
                    }
                    row.insert(*c, v.clone().unwrap_or_default());
                }
                MapOp::Replace(cols, find, replace) => {
                    let mut cols = cols.clone();
                    cols.sort_unstable();
                    cols.dedup();
                    for c in cols {
                        if let Some(cell) = row.get_mut(c) {
                            *cell = cell.replace(find.as_str(), replace);
                        }
                    }
                }
            }
        }
        row
    }

    fn mapped(map: &ColumnMap, raw: &[&str]) -> Vec<String> {
        let raw: Vec<&[u8]> = raw.iter().map(|s| s.as_bytes()).collect();
        map.logical(0, &raw)
            .iter()
            .map(|f| String::from_utf8(f.to_vec()).unwrap())
            .collect()
    }

    fn build(ops: &[MapOp]) -> ColumnMap {
        let mut map = ColumnMap::default();
        for op in ops {
            match op {
                MapOp::Remove(c) => {
                    map.remove(*c);
                }
                MapOp::Insert(c, v) => {
                    let values: HashMap<usize, String> = v.iter().map(|v| (0, v.clone())).collect();
                    map.insert(*c, ColSource::added(values));
                }
                MapOp::Replace(cols, find, replace) => {
                    map.add_rule(RuleRows::All, cols, find, replace);
                }
            }
        }
        map
    }

    fn map_ops() -> impl Strategy<Value = Vec<MapOp>> {
        let op = prop_oneof![
            (0usize..7).prop_map(MapOp::Remove),
            (0usize..7, prop::option::of("[a-c]{0,2}")).prop_map(|(c, v)| MapOp::Insert(c, v)),
            (
                prop::collection::vec(0usize..7, 0..4),
                "[a-c]{1,2}",
                "[a-c0-9]{0,2}"
            )
                .prop_map(|(cols, find, replace)| MapOp::Replace(cols, find, replace)),
        ];
        prop::collection::vec(op, 0..10)
    }

    #[test]
    fn an_untouched_column_map_is_the_identity() {
        let map = ColumnMap::default();
        assert!(map.is_identity());
        assert_eq!(mapped(&map, &["a", "b", "c"]), ["a", "b", "c"]);
        assert_eq!(mapped(&map, &[]), Vec::<String>::new());
    }

    #[test]
    fn inserting_then_removing_column_zero_returns_to_the_identity() {
        let mut map = ColumnMap::default();
        map.insert(0, ColSource::added(HashMap::new()));
        assert!(!map.is_identity());
        map.remove(0);
        assert!(map.is_identity(), "so scans can skip the mapping again");
    }

    #[test]
    fn padding_left_by_an_insert_survives_removing_that_column() {
        // A row shorter than the column is padded by the insert; removing the
        // new column does not un-pad it. The map must keep saying so.
        let mut map = ColumnMap::default();
        map.insert(3, ColSource::added(HashMap::new()));
        map.remove(3);
        assert_eq!(mapped(&map, &[]), ["", "", ""]);
        assert_eq!(mapped(&map, &["a"]), ["a", "", ""]);
        assert_eq!(
            mapped(&map, &["a", "b", "c", "d"]),
            ["a", "b", "c", "d"],
            "long rows are unchanged"
        );
    }

    #[test]
    fn a_rule_follows_its_column_when_columns_move() {
        let mut map = ColumnMap::default();
        map.add_rule(RuleRows::All, &[2], "a", "Q");
        assert_eq!(mapped(&map, &["a", "a", "a"]), ["a", "a", "Q"]);
        map.insert(0, ColSource::added(HashMap::new()));
        assert_eq!(
            mapped(&map, &["a", "a", "a"]),
            ["", "a", "a", "Q"],
            "the replaced column is now index 3"
        );
        map.remove(1);
        assert_eq!(mapped(&map, &["a", "a", "a"]), ["", "a", "Q"]);
    }

    #[test]
    fn a_rule_never_reaches_a_column_added_after_its_own_column_was_removed() {
        // Found by the property test: identifying an added column by the address
        // of its values let a freed address be reused, so this rule (meant for
        // the first column) reached the second.
        let mut map = ColumnMap::default();
        map.insert(0, ColSource::added(HashMap::new()));
        map.add_rule(RuleRows::All, &[0], "b", "");
        map.remove(0);
        for _ in 0..50 {
            let mut m = map.clone();
            m.insert(0, ColSource::added(HashMap::from([(0, "b".to_string())])));
            assert_eq!(
                mapped(&m, &[]),
                ["b"],
                "the new column was added after the rule"
            );
        }
    }

    #[test]
    fn a_rule_covers_only_its_rows() {
        let mut map = ColumnMap::default();
        map.add_rule(RuleRows::Ids(Arc::new(vec![2, 5])), &[0], "a", "Q");
        let raw: Vec<&[u8]> = vec![b"a"];
        let show = |id| String::from_utf8(map.logical(id, &raw)[0].to_vec()).unwrap();
        assert_eq!(
            (show(1), show(2), show(5), show(6)),
            ("a".into(), "Q".into(), "Q".into(), "a".into())
        );
    }

    #[test]
    fn rules_make_the_map_not_the_identity_and_the_bytes_untrustworthy() {
        let mut map = ColumnMap::default();
        assert!(map.is_identity() && !map.bytes_do_not_tell_the_whole_story());
        map.add_rule(RuleRows::All, &[0], "a", "b");
        assert!(!map.is_identity() && map.bytes_do_not_tell_the_whole_story());
    }

    #[test]
    fn column_map_examples() {
        let mut map = ColumnMap::default();
        map.remove(1);
        assert_eq!(mapped(&map, &["a", "b", "c"]), ["a", "c"]);
        assert_eq!(
            mapped(&map, &["a"]),
            ["a"],
            "a row that doesn't reach the column is untouched"
        );
        map.insert(3, ColSource::added(HashMap::new()));
        assert_eq!(
            mapped(&map, &["a", "b", "c"]),
            ["a", "c", "", ""],
            "padded up to the new column"
        );
        assert_eq!(
            mapped(&map, &["a", "b", "c", "d", "e"]),
            ["a", "c", "d", "", "e"]
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn column_map_matches_applying_the_operations_to_each_row(
            ops in map_ops(), raw in prop::collection::vec("[a-z]{0,2}", 0..6),
        ) {
            let raw: Vec<&str> = raw.iter().map(String::as_str).collect();
            let map = build(&ops);
            prop_assert_eq!(mapped(&map, &raw), naive(&raw, &ops));
            // one logical field at a time agrees with the whole row
            let want = naive(&raw, &ops);
            let raw_b: Vec<&[u8]> = raw.iter().map(|s| s.as_bytes()).collect();
            for col in 0..9 {
                let got = map.logical_field(0, &raw_b, col).map(|f| String::from_utf8(f.to_vec()).unwrap()).unwrap_or_default();
                prop_assert_eq!(got, want.get(col).cloned().unwrap_or_default(), "col {}", col);
            }
        }

    }

    #[test]
    fn a_row_whose_edit_is_undone_is_no_longer_held_in_memory() {
        let mut s = open(b"a,1\nb,2\nc,3\n", false);
        assert_eq!(s.rows_in_memory(), 0);
        assert!(s.set_cell(1, 0, "B"));
        assert_eq!(s.rows_in_memory(), 1);
        assert!(s.set_cell(1, 0, "b")); // back to what the file says
        assert_eq!(
            s.rows_in_memory(),
            0,
            "it equals the file again, so the copy is dropped"
        );
        assert_eq!(strs(&s, 1), Some(vec!["b".into(), "2".into()]));
        // a second edit of another cell keeps it, and reverting both drops it
        assert!(s.set_cell(1, 0, "B"));
        assert!(s.set_cell(1, 1, "TWO"));
        assert!(s.set_cell(1, 0, "b"));
        assert_eq!(s.rows_in_memory(), 1, "still differs in one cell");
        assert!(s.set_cell(1, 1, "2"));
        assert_eq!(s.rows_in_memory(), 0);
    }

    #[test]
    fn a_removed_row_put_back_unchanged_is_a_file_row_again() {
        let mut s = open(b"a,1\nb,2\nc,3\n", false);
        let row = s.remove_row(1).unwrap();
        s.insert_row(1, row);
        assert_eq!(
            s.rows_in_memory(),
            0,
            "nothing to remember: it is the file's row"
        );
        assert_eq!(ids(&s), vec![0, 1, 2]);
        assert!(
            !s.order_is_materialized(),
            "and the order is a single run again"
        );
        // an edited row put back keeps its edit
        s.set_cell(2, 1, "THREE");
        let row = s.remove_row(2).unwrap();
        s.insert_row(0, row);
        assert_eq!(s.rows_in_memory(), 1);
        assert_eq!(strs(&s, 0), Some(vec!["c".into(), "THREE".into()]));
    }

    #[test]
    fn column_edits_still_restore_exactly_when_rows_were_dropped_from_memory() {
        let mut s = open(b"a,b,c\nd,e,f\n", false);
        s.set_cell(1, 1, "E");
        let token = s.remove_column(1);
        s.set_cell(0, 0, "A");
        s.set_cell(0, 0, "a"); // dropped again: equals what the file gives under the map
        s.restore_column(1, &token);
        assert_eq!(strs(&s, 0), Some(vec!["a".into(), "b".into(), "c".into()]));
        assert_eq!(strs(&s, 1), Some(vec!["d".into(), "E".into(), "f".into()]));
    }

    #[test]
    fn edit_bytes_counts_changes_and_returns_to_a_floor_when_they_are_undone() {
        let mut s = open(&csv_like(5_000), false);
        let floor = s.edit_bytes();
        assert!(
            floor < 1024,
            "an untouched sheet holds almost nothing: {floor}"
        );
        for k in 0..1_000 {
            s.set_cell(k * 5, 1, "an edited value");
        }
        let held = s.edit_bytes();
        assert!(held > 1_000 * 150, "about 150 B+ per edited row: {held}");
        for k in 0..1_000 {
            s.set_cell(k * 5, 1, &format!("user{}", k * 5));
        }
        assert_eq!(s.rows_in_memory(), 0);
        assert!(
            s.edit_bytes() <= floor + 64,
            "undone edits give the memory back"
        );
    }

    /// `n` rows `i,user<i>,x` (so a cell can be set back to its original text).
    fn csv_like(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| format!("{i},user{i},x\n"))
            .collect::<String>()
            .into_bytes()
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
