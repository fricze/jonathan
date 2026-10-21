//! A [`Sheet`] backed by the file itself instead of rows in memory.
//!
//! Loading builds a [`RowIndex`] (8 bytes/row); a row is read and parsed only
//! when something asks for it. This step is the read-only half: reads,
//! filter, sort, snapshot. Edits arrive in the next steps.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use csv::StringRecord;

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
struct RowCache {
    rows: HashMap<usize, Arc<StringRecord>>,
    order: VecDeque<usize>,
    capacity: usize,
}

impl RowCache {
    fn insert(&mut self, position: usize, row: Arc<StringRecord>) {
        if self.capacity == 0 || self.rows.insert(position, row).is_some() {
            return;
        }
        self.order.push_back(position);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.rows.remove(&old);
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
    /// Rows changed since load, keyed by row id. The file itself is never
    /// written. A row is copied in here the first time one of its cells is
    /// edited and read from here ever after, so the cache cannot serve it
    /// stale. `Arc` makes a snapshot share the copies until either side
    /// edits one (`Arc::make_mut` then copies it).
    edited: HashMap<usize, Arc<StringRecord>>,
}

impl FileSheet {
    pub fn new(source: Arc<dyn ByteSource>, delimiter: u8, has_header: bool) -> io::Result<Self> {
        Self::with_cache_capacity(source, delimiter, has_header, DEFAULT_CACHE_ROWS)
    }

    /// Like `new`, but remembers at most `capacity` parsed rows.
    pub fn with_cache_capacity(
        source: Arc<dyn ByteSource>,
        delimiter: u8,
        has_header: bool,
        capacity: usize,
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
                capacity,
            })),
            edited: HashMap::new(),
        })
    }

    /// Read and parse the row at `position` straight from the source. `None`
    /// if there is no such row or its bytes can no longer be read.
    fn read_row(&self, position: usize) -> Option<StringRecord> {
        let range = self.index.row_range(position)?;
        let mut bytes = vec![0u8; (range.end - range.start) as usize];
        let mut filled = 0;
        while filled < bytes.len() {
            match self
                .source
                .read_at(range.start + filled as u64, &mut bytes[filled..])
            {
                Ok(0) | Err(_) => return None,
                Ok(n) => filled += n,
            }
        }
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(&bytes[..]);
        let mut raw = csv::ByteRecord::new();
        if !reader.read_byte_record(&mut raw).ok()? {
            return None;
        }
        Some(
            StringRecord::from_byte_record(raw.clone())
                .unwrap_or_else(|_| StringRecord::from_byte_record_lossy(raw)),
        )
    }

    /// Row id at `position`. Ids equal positions until rows can be inserted
    /// or removed.
    fn id_at(&self, position: usize) -> Option<usize> {
        (position < self.index.len()).then_some(position)
    }

    /// The row as the user sees it, bypassing the cache: the edited copy if
    /// there is one, else straight from the file. Scans use this.
    fn load_row(&self, position: usize) -> Option<Cow<'_, StringRecord>> {
        match self.edited.get(&self.id_at(position)?) {
            Some(edited) => Some(Cow::Borrowed(edited.as_ref())),
            None => self.read_row(position).map(Cow::Owned),
        }
    }

    /// The row as the user sees it, through the cache.
    fn row(&self, position: usize) -> Option<Arc<StringRecord>> {
        if let Some(edited) = self.edited.get(&self.id_at(position)?) {
            return Some(edited.clone());
        }
        let lock = || self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = lock().rows.get(&position) {
            return Some(hit.clone());
        }
        let row = Arc::new(self.read_row(position)?);
        lock().insert(position, row.clone());
        Some(row)
    }
}

impl Sheet for FileSheet {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn row_id(&self, position: usize) -> Option<usize> {
        (position < self.len()).then_some(position)
    }

    fn position_of(&self, row_id: usize) -> Option<usize> {
        self.row_id(row_id)
    }

    fn next_row_id(&self) -> usize {
        self.len()
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
        // Ids equal positions for now (see `id_at`).
        let Some(current) = self.row(row_id) else {
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

    fn remove_row(&mut self, _position: usize) -> Option<SheetRow> {
        todo!("FileSheet edits")
    }

    fn insert_row(&mut self, _position: usize, _row: SheetRow) {
        todo!("FileSheet edits")
    }

    fn remove_column(&mut self, _col: usize) -> Vec<(usize, String)> {
        todo!("FileSheet edits")
    }

    fn insert_column(&mut self, _col: usize, _values_by_row: &[(usize, String)]) {
        todo!("FileSheet edits")
    }

    // Scans bypass the cache: a pass over every row would otherwise evict
    // the rows the table is showing.
    fn filter(&self, needle: &str) -> View {
        (0..self.len())
            .filter(|&p| {
                needle.is_empty()
                    || self
                        .load_row(p)
                        .is_some_and(|r| r.iter().any(|c| c.contains(needle)))
            })
            .map(|p| p as u32)
            .collect()
    }

    fn sort(&self, view: View, sort_by: (usize, SortOrder)) -> View {
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
        sort_cells(
            texts.iter().map(|(t, p)| (t.as_str(), *p)).collect(),
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
