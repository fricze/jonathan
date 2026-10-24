//! Byte-offset index of a CSV file's data rows.
//!
//! One streaming pass records where each data row starts, so a row can later
//! be read by seeking to `row_range(i)` and parsing just those bytes. The
//! scan runs the same `csv_core` state machine the `csv` crate parses with,
//! so the rows found here are exactly the rows the parser sees (a naive
//! "newline outside quotes" scan is wrong: `ab"c` is a literal quote
//! mid-field, and only a quote at the start of a field opens a quoted one).
//!
//! Layout rules, which the saving code relies on:
//! * An optional UTF-8 BOM is skipped and sits before everything else.
//! * With a header, the first record is the header, not a data row.
//! * A row's range runs from its first byte to the start of the next row (or
//!   the end of the file), so it includes its own line terminator and any
//!   blank lines after it. Ranges therefore tile the data region exactly:
//!   copying them in order reproduces the file byte for byte.

use std::ops::Range;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowIndex {
    header: Range<u64>,
    starts: Vec<u64>,
    total: u64,
}

impl RowIndex {
    /// Index an in-memory file. Use [`RowIndexBuilder`] to stream chunks.
    pub fn build(bytes: &[u8], delimiter: u8, has_header: bool) -> RowIndex {
        let mut b = RowIndexBuilder::new(delimiter, has_header);
        b.feed(bytes);
        b.finish()
    }

    /// Number of data rows (the header is not one).
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Byte range of data row `i`, terminator included.
    pub fn row_range(&self, i: usize) -> Option<Range<u64>> {
        let start = *self.starts.get(i)?;
        let end = self.starts.get(i + 1).copied().unwrap_or(self.total);
        Some(start..end)
    }

    /// Byte range of the header record (empty if there is none).
    pub fn header_range(&self) -> Range<u64> {
        self.header.clone()
    }

    /// Total file length in bytes.
    pub fn total_len(&self) -> u64 {
        self.total
    }
}

/// Streaming builder: feed the file in chunks of any size, then `finish`.
/// Chunk boundaries never change the result.
pub struct RowIndexBuilder {
    reader: csv_core::Reader,
    has_header: bool,
    /// Bytes consumed so far (absolute file offset of the next input byte).
    offset: u64,
    /// The first bytes are held back until we can tell whether they are a BOM.
    head: Vec<u8>,
    head_done: bool,
    /// Between records (outside any quote), where line terminators are blank
    /// lines. Starts true: the file begins between records.
    between: bool,
    /// Start offset of every record seen, header included.
    starts: Vec<u64>,
    /// Scratch for csv_core, which insists on writing the fields out.
    out: Vec<u8>,
    ends: Vec<usize>,
}

impl RowIndexBuilder {
    pub fn new(delimiter: u8, has_header: bool) -> Self {
        RowIndexBuilder {
            reader: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            has_header,
            offset: 0,
            head: Vec::new(),
            head_done: false,
            between: true,
            starts: Vec::new(),
            out: vec![0; 8 * 1024],
            ends: vec![0; 256],
        }
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
        let mut starts = self.starts;
        // `header.end` is always where the data rows begin tiling. With no
        // header it is an empty range at the first row (or at the end).
        let header = if self.has_header {
            match starts.is_empty() {
                true => total..total,
                false => starts.remove(0)..starts.first().copied().unwrap_or(total),
            }
        } else {
            let at = starts.first().copied().unwrap_or(total);
            at..at
        };
        RowIndex {
            header,
            starts,
            total,
        }
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
                self.starts.push(self.offset + i as u64);
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
    fn assert_matches_parser(bytes: &[u8], delimiter: u8, has_header: bool) -> RowIndex {
        let idx = RowIndex::build(bytes, delimiter, has_header);
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
        let mut at = idx.header_range().end;
        for i in 0..idx.len() {
            let r = idx.row_range(i).unwrap();
            assert_eq!(r.start, at, "row {i} starts where the previous ended");
            at = r.end;
        }
        assert_eq!(at, bytes.len() as u64, "ranges reach the end of the file");
        idx
    }

    fn row_strs(bytes: &[u8], idx: &RowIndex) -> Vec<String> {
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
        let idx = RowIndex::build(&b, b',', true);
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
        let idx = RowIndex::build(b"", b',', true);
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
}
