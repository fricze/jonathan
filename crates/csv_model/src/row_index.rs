//! Sparse index of a CSV file's data rows.
//!
//! One streaming pass records where every `stride`-th data row starts (64 by
//! default: 0.125 bytes per row, 16 MB for a 10 GB file) and how many rows
//! there are. A row's own boundaries are found by reading the file from the
//! nearest sample and scanning forward with [`RowScanner`], which runs the same
//! `csv_core` state machine the parser uses (a naive "newline outside quotes"
//! scan is wrong: `ab"c` is a literal quote mid-field, and only a quote at the
//! start of a field opens a quoted one). [`RowWalker`] does that reading and
//! scanning, and keeps its place, so walking rows in order costs one scan of
//! each row and jumping to a row costs at most `stride` rows of scanning.
//!
//! Layout rules, which the saving code relies on:
//! * An optional UTF-8 BOM is skipped and sits before everything else.
//! * With a header, the first record is the header, not a data row.
//! * A row's range runs from its first byte to the start of the next row (or
//!   the end of the file), so it includes its own line terminator and any
//!   blank lines after it. Ranges therefore tile the data region exactly:
//!   copying them in order reproduces the file byte for byte.

use std::ops::Range;

use crate::file_sheet::ByteSource;
use crate::lean::Fields;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// One sample per this many data rows unless asked otherwise.
pub const DEFAULT_STRIDE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowIndex {
    header: Range<u64>,
    /// `samples[k]` is the offset of data row `k * stride`.
    samples: Vec<u64>,
    stride: usize,
    rows: usize,
    total: u64,
    delimiter: u8,
}

impl RowIndex {
    /// Index an in-memory file. Use [`RowIndexBuilder`] to stream chunks.
    pub fn build(bytes: &[u8], delimiter: u8, has_header: bool) -> RowIndex {
        Self::build_with_stride(bytes, delimiter, has_header, DEFAULT_STRIDE)
    }

    pub fn build_with_stride(
        bytes: &[u8],
        delimiter: u8,
        has_header: bool,
        stride: usize,
    ) -> RowIndex {
        let mut b = RowIndexBuilder::new(delimiter, has_header).with_stride(stride);
        b.feed(bytes);
        b.finish()
    }

    /// Number of data rows (the header is not one).
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Byte range of the header record (empty if there is none). Its end is
    /// where the data rows begin.
    pub fn header_range(&self) -> Range<u64> {
        self.header.clone()
    }

    /// Total file length in bytes.
    pub fn total_len(&self) -> u64 {
        self.total
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Offset of data row `k * stride`.
    pub fn sample(&self, k: usize) -> Option<u64> {
        self.samples.get(k).copied()
    }

    /// Memory this keeps alive.
    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.samples.capacity() * std::mem::size_of::<u64>()
    }

    /// Byte range of data row `i`, found by reading `source`. Convenience for
    /// one-off lookups; use a [`RowWalker`] to visit many rows.
    pub fn row_range(&self, source: &dyn ByteSource, i: usize) -> Option<Range<u64>> {
        let mut walker = RowWalker::new(self, source, 8 * 1024);
        walker.seek_id(i)?;
        walker.row_range()
    }

    /// The data row whose byte range contains `offset`; `None` for offsets in
    /// the BOM, the header, or past the end.
    pub fn row_at_offset(&self, source: &dyn ByteSource, offset: u64) -> Option<usize> {
        RowWalker::new(self, source, 8 * 1024).seek_offset(offset)
    }
}

/// Streaming builder: feed the file in chunks of any size, then `finish`.
/// Chunk boundaries never change the result.
pub struct RowIndexBuilder {
    reader: csv_core::Reader,
    delimiter: u8,
    has_header: bool,
    stride: usize,
    /// Bytes consumed so far (absolute file offset of the next input byte).
    offset: u64,
    /// The first bytes are held back until we can tell whether they are a BOM.
    head: Vec<u8>,
    head_done: bool,
    /// Between records (outside any quote), where line terminators are blank
    /// lines. Starts true: the file begins between records.
    between: bool,
    /// Records seen so far, the header included.
    records: usize,
    header_start: Option<u64>,
    samples: Vec<u64>,
    rows: usize,
    /// Scratch for csv_core, which insists on writing the fields out.
    out: Vec<u8>,
    ends: Vec<usize>,
}

impl RowIndexBuilder {
    pub fn new(delimiter: u8, has_header: bool) -> Self {
        RowIndexBuilder {
            reader: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            delimiter,
            has_header,
            stride: DEFAULT_STRIDE,
            offset: 0,
            head: Vec::new(),
            head_done: false,
            between: true,
            records: 0,
            header_start: None,
            samples: Vec::new(),
            rows: 0,
            out: vec![0; 8 * 1024],
            ends: vec![0; 256],
        }
    }

    /// Keep one sample per `stride` data rows (at least 1; 1 is a dense index).
    pub fn with_stride(mut self, stride: usize) -> Self {
        self.stride = stride.max(1);
        self
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        if self.head_done {
            self.scan(chunk);
            return;
        }
        self.head.extend_from_slice(chunk);
        if self.head.len() < BOM.len() {
            return;
        }
        self.head_done = true;
        let head = std::mem::take(&mut self.head);
        let skip = if head.starts_with(&BOM) { BOM.len() } else { 0 };
        self.offset += skip as u64;
        self.scan(&head[skip..]);
    }

    pub fn finish(mut self) -> RowIndex {
        if !self.head_done {
            // Fewer than three bytes in the whole file: too short for a BOM.
            let head = std::mem::take(&mut self.head);
            self.head_done = true;
            self.scan(&head);
        }
        let total = self.offset;
        self.samples.shrink_to_fit();
        let first_row = self.samples.first().copied();
        // `header.end` is always where the data rows begin. With no header it
        // is an empty range at the first row (or at the end).
        let header = match (self.has_header, self.header_start) {
            (true, Some(start)) => start..first_row.unwrap_or(total),
            (true, None) => total..total,
            (false, _) => {
                let at = first_row.unwrap_or(total);
                at..at
            }
        };
        RowIndex {
            header,
            samples: self.samples,
            stride: self.stride,
            rows: self.rows,
            total,
            delimiter: self.delimiter,
        }
    }

    fn record_begins(&mut self, at: u64) {
        if self.has_header && self.records == 0 {
            self.header_start = Some(at);
        } else {
            if self.rows % self.stride == 0 {
                self.samples.push(at);
            }
            self.rows += 1;
        }
        self.records += 1;
    }

    fn scan(&mut self, chunk: &[u8]) {
        let mut i = 0;
        while i < chunk.len() {
            if self.between {
                while i < chunk.len() && (chunk[i] == b'\n' || chunk[i] == b'\r') {
                    i += 1;
                }
                if i == chunk.len() {
                    break;
                }
                self.record_begins(self.offset + i as u64);
                self.between = false;
            }
            let (result, nin, _, _) =
                self.reader
                    .read_record(&chunk[i..], &mut self.out, &mut self.ends);
            i += nin;
            if let csv_core::ReadRecordResult::Record = result {
                self.between = true;
            }
        }
        self.offset += chunk.len() as u64;
    }
}

/// Finds where a row ends, given the bytes from its first byte on.
pub struct RowScanner {
    delimiter: u8,
    reader: csv_core::Reader,
    out: Vec<u8>,
    ends: Vec<usize>,
    /// Sizes of the fields the last `row_len` saw, when it saw the whole row
    /// in one go (so the output is complete).
    last: Option<(usize, usize)>,
}

impl RowScanner {
    pub fn delimiter(&self) -> u8 {
        self.delimiter
    }

    pub fn new(delimiter: u8) -> Self {
        RowScanner {
            delimiter,
            reader: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            out: vec![0; 4 * 1024],
            ends: vec![0; 256],
            last: None,
        }
    }

    /// The fields of the row the last `row_len` call delimited, if it produced
    /// them (a row too big for the scratch space, or an unterminated last row,
    /// does not).
    pub fn fields(&self) -> Option<Fields<'_>> {
        self.last
            .map(|(nout, nends)| Fields::new(&self.out[..nout], &self.ends[..nends]))
    }

    /// Length of the row that starts at `bytes[0]`: through its terminator and
    /// the blank lines after it, i.e. up to the next row's first byte. `None`
    /// when `bytes` ends before that is known; `eof` says no more bytes exist,
    /// so the row simply ends where `bytes` does.
    pub fn row_len(&mut self, bytes: &[u8], eof: bool) -> Option<usize> {
        self.reader.reset();
        self.last = None;
        let mut i = 0;
        let mut overflowed = false;
        loop {
            let (result, nin, nout, nends) =
                self.reader
                    .read_record(&bytes[i..], &mut self.out, &mut self.ends);
            i += nin;
            match result {
                csv_core::ReadRecordResult::Record => {
                    if !overflowed {
                        self.last = Some((nout, nends));
                    }
                    while i < bytes.len() && (bytes[i] == b'\n' || bytes[i] == b'\r') {
                        i += 1;
                    }
                    // Blank lines may go on in bytes we do not have yet.
                    return (i < bytes.len() || eof).then_some(i);
                }
                csv_core::ReadRecordResult::InputEmpty => {
                    return eof.then_some(bytes.len());
                }
                csv_core::ReadRecordResult::End => return Some(bytes.len()),
                // Output buffers full: the fields of this row cannot be handed
                // out, but we only need where it ends, so keep going.
                _ => overflowed = true,
            }
        }
    }
}

fn read_exact(source: &dyn ByteSource, offset: u64, buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read_at(offset + filled as u64, &mut buf[filled..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => filled += n,
        }
    }
    true
}

/// A cursor over the data rows of a file. It remembers the row it is on and a
/// window of bytes around it, so moving to the next row (or a few rows ahead)
/// is a scan of those rows, and jumping anywhere else is a read from the
/// nearest sample plus a scan of at most `stride` rows.
pub struct RowWalker<'a> {
    index: &'a RowIndex,
    source: &'a dyn ByteSource,
    scanner: RowScanner,
    buf: Vec<u8>,
    buf_start: u64,
    /// How much to read when walking forward, and after a jump.
    scan_chunk: usize,
    jump_chunk: usize,
    /// The walker has just jumped (so the next read should be small).
    jumped: bool,
    positioned: bool,
    id: usize,
    start: u64,
    len: Option<u64>,
    /// The row whose fields the scanner is holding, if any.
    fields_of: Option<usize>,
}

impl<'a> RowWalker<'a> {
    /// `chunk` is how many bytes a read fetches while walking forward (it grows
    /// for long rows); after a jump it fetches at most 8 KiB, enough for the
    /// handful of rows between a sample and the row wanted.
    pub fn new(index: &'a RowIndex, source: &'a dyn ByteSource, chunk: usize) -> Self {
        Self::reusing(
            index,
            source,
            chunk,
            RowScanner::new(index.delimiter),
            Vec::new(),
        )
    }

    /// Like `new`, but with a scanner (for this index's delimiter) and a buffer
    /// from an earlier walker, handed back by [`RowWalker::recycle`]. Building a
    /// scanner compiles a state machine, which costs more than locating a row.
    pub fn reusing(
        index: &'a RowIndex,
        source: &'a dyn ByteSource,
        chunk: usize,
        scanner: RowScanner,
        mut buf: Vec<u8>,
    ) -> Self {
        // Keep the allocation, not the bytes: they belong to another walk.
        buf.clear();
        RowWalker {
            index,
            source,
            scanner,
            buf,
            buf_start: 0,
            scan_chunk: chunk.max(64),
            jump_chunk: chunk.clamp(64, 8 * 1024),
            jumped: true,
            positioned: false,
            id: 0,
            start: 0,
            len: None,
            fields_of: None,
        }
    }

    /// Give back the scanner and buffer for the next walker to reuse.
    pub fn recycle(self) -> (RowScanner, Vec<u8>) {
        (self.scanner, self.buf)
    }

    /// Id of the row the walker is on.
    pub fn id(&self) -> usize {
        self.id
    }

    fn anchor(&mut self, sample: usize) -> bool {
        let Some(start) = self.index.sample(sample) else {
            return false;
        };
        self.id = sample * self.index.stride;
        self.start = start;
        self.len = None;
        self.positioned = true;
        self.jumped = true;
        true
    }

    /// Scan the current row in the bytes already held. `None` if they do not
    /// reach far enough to tell where it ends.
    fn scan_held(&mut self) -> Option<u64> {
        let held_end = self.buf_start + self.buf.len() as u64;
        if self.start < self.buf_start || self.start >= held_end {
            return None;
        }
        let from = (self.start - self.buf_start) as usize;
        let eof = held_end >= self.index.total;
        self.scanner
            .row_len(&self.buf[from..], eof)
            .map(|len| len as u64)
    }

    /// Length of the current row, reading more of the file only if it must.
    fn current_len(&mut self) -> Option<u64> {
        if let Some(len) = self.len {
            return Some(len);
        }
        let mut found = self.scan_held();
        let mut want = if self.jumped {
            self.jump_chunk
        } else {
            self.scan_chunk
        };
        while found.is_none() {
            // Read from the row's start, as much as `want` (doubling until the
            // row, however long, is delimited or the file ends).
            let n = (self.start + want as u64).min(self.index.total) - self.start;
            self.buf.resize(n as usize, 0);
            self.buf_start = self.start;
            if !read_exact(self.source, self.start, &mut self.buf) {
                self.buf.clear();
                return None;
            }
            found = self.scan_held();
            want = want.saturating_mul(2);
        }
        self.len = found;
        self.fields_of = found.map(|_| self.id);
        found
    }

    /// Move to the next row. `false` past the last one.
    pub fn advance(&mut self) -> bool {
        let Some(len) = self.current_len() else {
            return false;
        };
        self.start += len;
        self.id += 1;
        self.len = None;
        self.jumped = false;
        self.id < self.index.rows
    }

    /// Move to row `id`. `None` if there is no such row or the file cannot be read.
    pub fn seek_id(&mut self, id: usize) -> Option<()> {
        if id >= self.index.rows {
            return None;
        }
        let near = self.positioned && id >= self.id && id - self.id <= 4 * self.index.stride;
        if !near && !self.anchor(id / self.index.stride) {
            return None;
        }
        while self.id < id {
            if !self.advance() {
                return None;
            }
        }
        Some(())
    }

    /// Move to the row containing byte `offset`; its id. `None` for offsets
    /// before the first row or past the end.
    pub fn seek_offset(&mut self, offset: u64) -> Option<usize> {
        let first = self.index.header.end;
        if offset >= self.index.total || offset < first || self.index.rows == 0 {
            return None;
        }
        let near = self.positioned && offset >= self.start && offset - self.start <= 16 * 1024;
        if !near {
            let sample = self
                .index
                .samples
                .partition_point(|&s| s <= offset)
                .checked_sub(1)?;
            if !self.anchor(sample) {
                return None;
            }
        }
        loop {
            let len = self.current_len()?;
            if offset < self.start + len {
                return Some(self.id);
            }
            if !self.advance() {
                return None;
            }
        }
    }

    /// Byte range of the current row.
    pub fn row_range(&mut self) -> Option<Range<u64>> {
        if !self.positioned || self.id >= self.index.rows {
            return None;
        }
        let len = self.current_len()?;
        Some(self.start..self.start + len)
    }

    /// The current row's fields, if the scan that delimited it also produced
    /// them (the usual case: a row that fits the scanner's scratch space and
    /// ends in a terminator). Saves parsing the row a second time; when it is
    /// `None`, parse [`RowWalker::row_bytes`] instead.
    pub fn row_fields(&mut self) -> Option<Fields<'_>> {
        self.row_range()?;
        if self.fields_of == Some(self.id) {
            self.scanner.fields()
        } else {
            None
        }
    }

    /// The current row's bytes (valid until the walker moves).
    pub fn row_bytes(&mut self) -> Option<&[u8]> {
        let range = self.row_range()?;
        let from = (range.start - self.buf_start) as usize;
        self.buf
            .get(from..from + (range.end - range.start) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_sheet::VecSource;
    use proptest::prelude::*;

    /// The sparse index together with the bytes it indexes, offering the old
    /// dense-index calls so every boundary test below runs through the new code.
    /// Stride 2, so even tiny inputs have several samples.
    struct Idx {
        index: RowIndex,
        src: VecSource,
    }

    impl Idx {
        fn build(bytes: &[u8], delimiter: u8, has_header: bool) -> Idx {
            Self::with_stride(bytes, delimiter, has_header, 2)
        }

        fn with_stride(bytes: &[u8], delimiter: u8, has_header: bool, stride: usize) -> Idx {
            Idx {
                index: RowIndex::build_with_stride(bytes, delimiter, has_header, stride),
                src: VecSource(bytes.to_vec()),
            }
        }

        fn len(&self) -> usize {
            self.index.len()
        }

        fn header_range(&self) -> Range<u64> {
            self.index.header_range()
        }

        fn total_len(&self) -> u64 {
            self.index.total_len()
        }

        fn row_range(&self, i: usize) -> Option<Range<u64>> {
            self.index.row_range(&self.src, i)
        }

        fn row_at_offset(&self, offset: u64) -> Option<usize> {
            self.index.row_at_offset(&self.src, offset)
        }
    }

    /// Parse `bytes` the way the app does (flexible, no header handling).
    fn parse(bytes: &[u8], delimiter: u8) -> Vec<Vec<String>> {
        csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(bytes)
            .records()
            .map(|r| r.unwrap().iter().map(str::to_string).collect())
            .collect()
    }

    fn slice<'a>(bytes: &'a [u8], r: Range<u64>) -> &'a [u8] {
        &bytes[r.start as usize..r.end as usize]
    }

    /// The index agrees with a full parse: same row count, and parsing each
    /// row's slice alone gives exactly that row.
    fn assert_matches_parser(bytes: &[u8], delimiter: u8, has_header: bool) -> Idx {
        let idx = Idx::build(bytes, delimiter, has_header);
        let all = parse(bytes, delimiter);
        let skip = usize::from(has_header && !all.is_empty());
        let want = &all[skip..];
        assert_eq!(
            idx.len(),
            want.len(),
            "row count for {:?}",
            String::from_utf8_lossy(bytes)
        );
        for (i, w) in want.iter().enumerate() {
            let got = parse(slice(bytes, idx.row_range(i).unwrap()), delimiter);
            assert_eq!(
                got,
                vec![w.clone()],
                "row {i} of {:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        // Ranges tile [header.end, total) with no gaps or overlaps.
        let ranges: Vec<Range<u64>> = (0..idx.len()).map(|i| idx.row_range(i).unwrap()).collect();
        let mut at = idx.header_range().end;
        for (i, r) in ranges.iter().enumerate() {
            assert_eq!(r.start, at, "row {i} starts where the previous ended");
            at = r.end.clone();
        }
        assert_eq!(at, bytes.len() as u64, "ranges reach the end of the file");
        if bytes.len() < 4000 {
            let first = idx.header_range().end;
            for off in 0..bytes.len() as u64 + 3 {
                let want = (off >= first && off < bytes.len() as u64)
                    .then(|| ranges.iter().position(|r| r.contains(&off)).unwrap());
                assert_eq!(
                    idx.row_at_offset(off),
                    want,
                    "row_at_offset({off}) of {:?}",
                    String::from_utf8_lossy(bytes)
                );
            }
        }
        idx
    }

    fn row_strs(bytes: &[u8], idx: &Idx) -> Vec<String> {
        (0..idx.len())
            .map(|i| String::from_utf8_lossy(slice(bytes, idx.row_range(i).unwrap())).into_owned())
            .collect()
    }

    #[test]
    fn lf_rows_with_a_header() {
        let b = b"h1,h2\n1,2\n3,4\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 2);
        assert_eq!(slice(b, idx.header_range()), b"h1,h2\n");
        assert_eq!(row_strs(b, &idx), ["1,2\n", "3,4\n"]);
    }

    #[test]
    fn crlf_keeps_its_terminator_with_its_row() {
        let b = b"h\r\n1\r\n2\r\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["1\r\n", "2\r\n"]);
    }

    #[test]
    fn lone_cr_terminates_rows() {
        let b = b"h\r1\r2\r";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn mixed_line_endings() {
        let b = b"h\n1\r\n2\r3\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["1\r\n", "2\r", "3\n"]);
    }

    #[test]
    fn quoted_newline_stays_inside_its_row() {
        let b = b"h1,h2\n\"a\nb\",c\nd,e\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["\"a\nb\",c\n", "d,e\n"]);
    }

    #[test]
    fn quoted_crlf_stays_inside_its_row() {
        let b = b"h\r\n\"a\r\nb\"\r\nc\r\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn doubled_quote_escape_does_not_end_the_field() {
        let b = b"h\n\"say \"\"hi\"\"\nthere\"\nnext\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn mid_field_quote_is_literal_and_does_not_open_a_quoted_field() {
        // A quote that isn't at the start of a field is plain text, so the
        // newline after it really ends the row.
        let b = b"h\nab\"c,d\ne,f\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["ab\"c,d\n", "e,f\n"]);
    }

    #[test]
    fn no_trailing_newline() {
        let b = b"h\n1\n2";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["1\n", "2"]);
    }

    #[test]
    fn blank_lines_belong_to_the_row_before_them() {
        let b = b"h\n1\n\n\n2\n\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["1\n\n\n", "2\n\n"]);
    }

    #[test]
    fn blank_lines_made_of_bare_cr_belong_to_the_row_before_them() {
        let b = b"h\n1\r\r\r2\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(row_strs(b, &idx), ["1\r\r\r", "2\n"]);
    }

    #[test]
    fn leading_blank_lines_before_the_header_are_not_rows() {
        let b = b"\n\nh\n1\n";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn unterminated_quote_at_eof_is_one_last_row() {
        let b = b"h\n1\n\"never closed\nstill going";
        let idx = assert_matches_parser(b, b',', true);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn bom_is_skipped_and_not_part_of_the_header() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"h\n1\n");
        let idx = Idx::build(&b, b',', true);
        assert_eq!(idx.header_range(), 3..5);
        assert_eq!(idx.len(), 1);
        assert_eq!(slice(&b, idx.row_range(0).unwrap()), b"1\n");
    }

    #[test]
    fn headerless_file_indexes_every_record() {
        let b = b"1,2\n3,4\n";
        let idx = assert_matches_parser(b, b',', false);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.header_range(), 0..0);
    }

    #[test]
    fn other_delimiters() {
        for d in [b';', b'\t', b'|'] {
            let line = |a: &str, b: &str| format!("{a}{}{b}\n", d as char);
            let text = format!(
                "{}{}\"x{}y\n z\"{}",
                line("h", "i"),
                line("1", "2"),
                d as char,
                line("3", "4")
            );
            assert_matches_parser(text.as_bytes(), d, true);
        }
    }

    #[test]
    fn a_comma_is_ordinary_text_when_the_delimiter_is_a_semicolon() {
        let b = b"h;i\n1,5;2\n";
        let idx = assert_matches_parser(b, b';', true);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn empty_file() {
        let idx = Idx::build(b"", b',', true);
        assert_eq!(
            (idx.len(), idx.header_range(), idx.total_len()),
            (0, 0..0, 0)
        );
        assert!(idx.row_range(0).is_none());
    }

    #[test]
    fn header_only_with_and_without_newline() {
        for b in [&b"a,b\n"[..], &b"a,b"[..]] {
            let idx = assert_matches_parser(b, b',', true);
            assert_eq!(idx.len(), 0);
            assert_eq!(idx.header_range(), 0..b.len() as u64);
        }
    }

    #[test]
    fn only_blank_lines() {
        let idx = assert_matches_parser(b"\n\r\n\n", b',', true);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn ragged_rows() {
        assert_matches_parser(b"a,b,c\n1\n2,3,4,5\n,,\n", b',', true);
    }

    #[test]
    fn bom_split_across_chunks() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"h\n1\n");
        for split in 0..=b.len() {
            let mut bld = RowIndexBuilder::new(b',', true);
            bld.feed(&b[..split]);
            bld.feed(&b[split..]);
            assert_eq!(
                bld.finish(),
                RowIndex::build(&b, b',', true),
                "split at {split}"
            );
        }
    }

    #[test]
    fn one_byte_at_a_time_matches_one_shot() {
        let b = b"h1,h2\r\n\"a\r\nb\"\"c\",d\n\n\nx,\"y\"\r\n\"open";
        let mut bld = RowIndexBuilder::new(b',', true);
        for byte in b {
            bld.feed(std::slice::from_ref(byte));
        }
        assert_eq!(bld.finish(), RowIndex::build(b, b',', true));
    }

    #[test]
    fn rows_longer_than_the_internal_scratch_buffers() {
        let big = "x".repeat(100_000);
        let wide = vec!["f"; 5_000].join(",");
        let text = format!("h\n{big}\n{wide}\n\"{big}\nmore\"\nlast\n");
        let idx = assert_matches_parser(text.as_bytes(), b',', true);
        assert_eq!(idx.len(), 4);
    }

    // Random text over an alphabet that exercises every state: quotes,
    // both newline kinds, the delimiter, and plain bytes.
    fn csvish() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop::sample::select(vec![
                b'a', b'b', b' ', b',', b';', b'"', b'"', b'\n', b'\r', b'\n',
            ]),
            0..120,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn index_agrees_with_the_csv_parser_on_arbitrary_text(
            bytes in csvish(), delim in prop::sample::select(vec![b',', b';']), header: bool,
        ) {
            assert_matches_parser(&bytes, delim, header);
        }

        #[test]
        fn chunking_never_changes_the_index(
            bytes in csvish(), cuts in prop::collection::vec(any::<usize>(), 0..6), header: bool,
        ) {
            let mut points: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
            points.sort_unstable();
            let mut bld = RowIndexBuilder::new(b',', header);
            let mut at = 0;
            for p in points {
                bld.feed(&bytes[at..p]);
                at = p;
            }
            bld.feed(&bytes[at..]);
            prop_assert_eq!(bld.finish(), RowIndex::build(&bytes, b',', header));
        }
    }

    // ---- the sparse index against an independent oracle ----

    /// The oracle: with stride 1 every row has a sample, so row `i` is the bytes
    /// from `samples[i]` to `samples[i + 1]` - no scanning involved.
    fn oracle(bytes: &[u8], delimiter: u8, has_header: bool) -> (RowIndex, Vec<Range<u64>>) {
        let dense = RowIndex::build_with_stride(bytes, delimiter, has_header, 1);
        let ranges = (0..dense.len())
            .map(|i| dense.sample(i).unwrap()..dense.sample(i + 1).unwrap_or(dense.total_len()))
            .collect();
        (dense, ranges)
    }

    #[test]
    fn a_sparse_index_keeps_one_offset_per_stride_rows() {
        let text: String = (0..1000).map(|i| format!("{i},x\n")).collect();
        for stride in [1usize, 3, 64, 1000, 5000] {
            let idx = RowIndex::build_with_stride(text.as_bytes(), b',', false, stride);
            assert_eq!(idx.len(), 1000);
            assert_eq!(idx.sample(0), Some(0));
            assert_eq!(idx.stride(), stride);
            let samples = (0..).take_while(|&k| idx.sample(k).is_some()).count();
            assert_eq!(samples, 1000usize.div_ceil(stride), "stride {stride}");
        }
        let big = RowIndex::build(text.repeat(100).as_bytes(), b',', false);
        assert_eq!(big.len(), 100_000);
        assert!(
            big.size_bytes() < 16 * 1024,
            "0.125 B/row, not 8: {} bytes",
            big.size_bytes()
        );
    }

    #[test]
    fn jumping_around_a_file_with_one_walker_finds_every_row() {
        let text: String = (0..500)
            .map(|i| format!("{i},\"q\nq\",{}\n\n", "z".repeat(i % 7)))
            .collect();
        let bytes = text.as_bytes();
        let (_, want) = oracle(bytes, b',', false);
        let idx = RowIndex::build_with_stride(bytes, b',', false, 7);
        let src = VecSource(bytes.to_vec());
        let mut w = RowWalker::new(&idx, &src, 64);
        let order = (0..500)
            .chain((0..500).rev())
            .chain((0..500).step_by(37))
            .chain((0..500).step_by(3));
        for i in order {
            assert_eq!(w.seek_id(i), Some(()));
            assert_eq!(w.row_range(), Some(want[i].clone()), "row {i}");
            assert_eq!(w.id(), i);
        }
        assert_eq!(w.seek_id(500), None);
    }

    #[test]
    fn fields_from_the_walker_always_belong_to_the_current_row() {
        // Short rows, quoted rows, a row too big for the scanner's scratch
        // space, and an unterminated last row; visited forwards, backwards and
        // by jumps, with the fields asked for in different orders.
        let big = "w".repeat(20_000);
        let mut text = String::new();
        for i in 0..60 {
            match i % 9 {
                4 => text.push_str(&format!("{i},\"{big}\",x\n")),
                7 => text.push_str(&format!("{i},\"a\nb\",\"\"\"\"\n\n")),
                _ => text.push_str(&format!("{i},v{i}\n")),
            }
        }
        text.push_str("last,unterminated");
        let bytes = text.as_bytes();
        let idx = RowIndex::build_with_stride(bytes, b',', false, 4);
        let src = VecSource(bytes.to_vec());
        let mut w = RowWalker::new(&idx, &src, 64);
        let mut lean = crate::lean::LeanParser::new(b',');
        let order: Vec<usize> = (0..61)
            .chain((0..61).rev())
            .chain((0..61).step_by(5))
            .collect();
        for (n, i) in order.into_iter().enumerate() {
            w.seek_id(i).unwrap();
            if n % 2 == 0 {
                w.row_range().unwrap();
            }
            let row = w.row_bytes().unwrap().to_vec();
            let want: Vec<Vec<u8>> = lean
                .parse(&row)
                .map(|f| f.iter().map(|c| c.to_vec()).collect())
                .unwrap_or_default();
            if let Some(f) = w.row_fields() {
                let got: Vec<Vec<u8>> = f.iter().map(|c| c.to_vec()).collect();
                assert_eq!(got, want, "row {i}");
            }
        }
    }

    #[test]
    fn a_row_longer_than_the_walkers_window_is_read_whole() {
        let long = "y".repeat(100_000);
        let text = format!("a\n\"{long}\"\nb\n");
        let idx = RowIndex::build_with_stride(text.as_bytes(), b',', false, 2);
        let src = VecSource(text.clone().into_bytes());
        let mut w = RowWalker::new(&idx, &src, 64);
        w.seek_id(1).unwrap();
        assert_eq!(w.row_range(), Some(2..(2 + 100_003)));
        assert_eq!(w.row_bytes().unwrap().len(), 100_003);
        w.seek_id(2).unwrap();
        assert_eq!(w.row_bytes(), Some(&b"b\n"[..]));
    }

    #[test]
    fn a_source_that_fails_makes_the_walk_stop_not_panic() {
        struct Broken;
        impl ByteSource for Broken {
            fn len(&self) -> u64 {
                100
            }
            fn read_at(&self, _: u64, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("gone"))
            }
        }
        let idx = RowIndex::build_with_stride(b"a\nb\nc\n", b',', false, 1);
        let mut w = RowWalker::new(&idx, &Broken, 64);
        // Finding a sample needs no read; reading the row it names does.
        assert_eq!(w.seek_id(1).and_then(|_| w.row_range()), None);
        assert_eq!(w.seek_id(2), None, "two rows on needs the bytes in between");
        assert_eq!(idx.row_range(&Broken, 0), None);
        assert_eq!(idx.row_at_offset(&Broken, 2), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2500))]

        #[test]
        fn sparse_walking_agrees_with_the_oracle_for_every_stride_and_window(
            bytes in csvish(), delim in prop::sample::select(vec![b',', b';']), header: bool,
            stride in 1usize..6, chunk in 1usize..40, jumps in prop::collection::vec(any::<usize>(), 0..12),
        ) {
            let (dense, want) = oracle(&bytes, delim, header);
            let idx = RowIndex::build_with_stride(&bytes, delim, header, stride);
            prop_assert_eq!(idx.len(), dense.len());
            prop_assert_eq!(idx.header_range(), dense.header_range());
            prop_assert_eq!(idx.total_len(), dense.total_len());
            let src = VecSource(bytes.clone());
            let mut w = RowWalker::new(&idx, &src, chunk);
            // in order, by advancing
            if !want.is_empty() {
                prop_assert_eq!(w.seek_id(0), Some(()));
                for (i, range) in want.iter().enumerate() {
                    prop_assert_eq!(w.id(), i);
                    prop_assert_eq!(w.row_range(), Some(range.clone()));
                    prop_assert_eq!(w.advance(), i + 1 < want.len());
                }
            }
            // jumping about
            for j in &jumps {
                if want.is_empty() { break }
                let i = j % want.len();
                prop_assert_eq!(w.seek_id(i), Some(()));
                prop_assert_eq!(w.row_range(), Some(want[i].clone()));
            }
            // every byte offset
            for off in 0..bytes.len() as u64 + 2 {
                let expect = want.iter().position(|r| r.contains(&off));
                prop_assert_eq!(w.seek_offset(off), expect, "offset {}", off);
            }
        }

        #[test]
        fn streaming_a_sparse_index_in_pieces_gives_the_same_index(
            bytes in csvish(), cuts in prop::collection::vec(any::<usize>(), 0..6), header: bool, stride in 1usize..6,
        ) {
            let mut points: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
            points.sort_unstable();
            let mut bld = RowIndexBuilder::new(b',', header).with_stride(stride);
            let mut at = 0;
            for p in points {
                bld.feed(&bytes[at..p]);
                at = p;
            }
            bld.feed(&bytes[at..]);
            prop_assert_eq!(bld.finish(), RowIndex::build_with_stride(&bytes, b',', header, stride));
        }
    }

    #[test]
    fn a_recycled_buffer_does_not_leak_bytes_from_the_previous_walk() {
        let first = VecSource(b"AAAA\nBBBB\nCCCC\n".to_vec());
        let second = VecSource(b"x\ny\nz\n".to_vec());
        let i1 = RowIndex::build_with_stride(&first.0, b',', false, 1);
        let i2 = RowIndex::build_with_stride(&second.0, b',', false, 1);
        let mut w = RowWalker::new(&i1, &first, 64);
        w.seek_id(2).unwrap();
        assert_eq!(w.row_bytes(), Some(&b"CCCC\n"[..]));
        let (scanner, buf) = w.recycle();
        let mut w = RowWalker::reusing(&i2, &second, 64, scanner, buf);
        w.seek_id(1).unwrap();
        assert_eq!(w.row_bytes(), Some(&b"y\n"[..]));
        w.seek_id(2).unwrap();
        assert_eq!(w.row_bytes(), Some(&b"z\n"[..]));
    }
}
