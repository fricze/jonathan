//! A reusable, allocation-free parser for one CSV record held in memory.
//!
//! `csv::Reader` allocates a buffer and a record per use, ~19 us for a short
//! row; this reuses one `csv_core` reader and two scratch buffers and parses a
//! row in ~0.1 us (docs/design/large-file-engine.md, section 2). It parses
//! exactly the record at the start of the slice and ignores what follows (the
//! row index gives slices that end with their terminator and trailing blank
//! lines). It uses the same `csv_core` state machine as the `csv` crate and the
//! row index, so all three agree on what a row is.

/// The fields of one parsed record, as byte slices into the parser's buffer.
pub struct Fields<'a> {
    out: &'a [u8],
    ends: &'a [usize],
}

impl<'a> Fields<'a> {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&'a [u8]> {
        let end = *self.ends.get(i)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        Some(&self.out[start..end])
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        (0..self.len()).map(|i| self.get(i).unwrap())
    }
}

pub struct LeanParser {
    reader: csv_core::Reader,
    out: Vec<u8>,
    ends: Vec<usize>,
}

impl LeanParser {
    pub fn new(delimiter: u8) -> Self {
        LeanParser {
            reader: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            out: Vec::new(),
            ends: Vec::new(),
        }
    }

    /// Parse the record at the start of `row`. `None` if `row` holds no
    /// record (empty or only blank lines).
    pub fn parse(&mut self, row: &[u8]) -> Option<Fields<'_>> {
        // Output is never longer than the input and a record never has more
        // fields than bytes + 1, so the buffers cannot fill up mid-record.
        if self.out.len() < row.len() + 1 {
            self.out.resize(row.len() + 1, 0);
        }
        if self.ends.len() < row.len() + 2 {
            self.ends.resize(row.len() + 2, 0);
        }
        self.reader.reset();
        let (result, _, mut nout, mut nends) =
            self.reader.read_record(row, &mut self.out, &mut self.ends);
        match result {
            csv_core::ReadRecordResult::Record => {}
            csv_core::ReadRecordResult::InputEmpty => {
                // The row ended without a terminator: signal end of input to
                // get the record that was in progress, appending to what we have.
                let (r, _, o, e) =
                    self.reader
                        .read_record(&[], &mut self.out[nout..], &mut self.ends[nends..]);
                if !matches!(r, csv_core::ReadRecordResult::Record) {
                    return None;
                }
                nout += o;
                nends += e;
            }
            _ => return None,
        }
        Some(Fields {
            out: &self.out[..nout],
            ends: &self.ends[..nends],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// What the csv crate says the first record of `bytes` is.
    fn reference(bytes: &[u8], delimiter: u8) -> Option<Vec<Vec<u8>>> {
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(bytes);
        let mut rec = csv::ByteRecord::new();
        rdr.read_byte_record(&mut rec)
            .unwrap()
            .then(|| rec.iter().map(<[u8]>::to_vec).collect())
    }

    fn lean(p: &mut LeanParser, bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
        p.parse(bytes)
            .map(|f| f.iter().map(<[u8]>::to_vec).collect())
    }

    fn check(bytes: &[u8], delimiter: u8) {
        let mut p = LeanParser::new(delimiter);
        assert_eq!(
            lean(&mut p, bytes),
            reference(bytes, delimiter),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }

    #[test]
    fn simple_rows_and_terminators() {
        check(b"a,b,c\n", b',');
        check(b"a,b,c\r\n", b',');
        check(b"a,b,c\r", b',');
        check(b"a,b,c", b',');
        check(b"a\n", b',');
        check(b",\n", b',');
        check(b",,\n", b',');
        check(b"\"\"\n", b',');
    }

    #[test]
    fn quotes_escapes_and_embedded_newlines() {
        check(b"\"a,b\",c\n", b',');
        check(b"\"say \"\"hi\"\"\",x\n", b',');
        check(b"\"line1\nline2\",x\n", b',');
        check(b"\"line1\r\nline2\",x\r\n", b',');
        check(b"ab\"c,d\n", b','); // literal mid-field quote
        check(b"\"never closed\nstill going", b',');
    }

    #[test]
    fn only_the_first_record_is_parsed_and_trailing_blank_lines_are_ignored() {
        check(b"a,b\nc,d\n", b',');
        check(b"a,b\n\n\n", b',');
        check(b"a,b\r\n\r\nx", b',');
    }

    #[test]
    fn no_record_in_empty_or_blank_input() {
        let mut p = LeanParser::new(b',');
        assert!(p.parse(b"").is_none());
        assert!(p.parse(b"\n\r\n").is_none());
    }

    #[test]
    fn other_delimiters() {
        check(b"a;b,c;d\n", b';');
        check(b"a\tb\t\"x\ty\"\n", b'\t');
        check(b"a|b|c\n", b'|');
    }

    #[test]
    fn invalid_utf8_passes_through_as_bytes() {
        check(b"a,\xff\xfe,c\n", b',');
    }

    #[test]
    fn a_parser_is_reusable_across_rows_of_very_different_sizes() {
        let mut p = LeanParser::new(b',');
        let big = format!("{},{}\n", "x".repeat(200_000), vec!["f"; 50_000].join(","));
        for row in [
            &b"a,b\n"[..],
            big.as_bytes(),
            b"c\n",
            b"\"q\nq\",z\n",
            big.as_bytes(),
            b"\n",
            b"d,e\n",
        ] {
            assert_eq!(lean(&mut p, row), reference(row, b','), "len {}", row.len());
        }
        assert_eq!(p.parse(big.as_bytes()).unwrap().len(), 50_001);
    }

    fn csvish() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop::sample::select(vec![
                b'a', b'b', b' ', b',', b';', b'"', b'"', b'\n', b'\r', 0xff,
            ]),
            0..80,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        #[test]
        fn agrees_with_the_csv_crate_on_arbitrary_bytes(bytes in csvish(), delim in prop::sample::select(vec![b',', b';'])) {
            check(&bytes, delim);
        }

        #[test]
        fn one_parser_reused_across_many_rows_agrees_every_time(rows in prop::collection::vec(csvish(), 1..12)) {
            let mut p = LeanParser::new(b',');
            for row in &rows {
                prop_assert_eq!(lean(&mut p, row), reference(row, b','));
            }
        }
    }
}
