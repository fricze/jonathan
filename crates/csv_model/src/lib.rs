//! Pure CSV data model for Jonathan: the row/column types, and the
//! edit/sort/filter/read/write operations on them. No UI dependency --
//! every function here is plain data in, plain data out, independently
//! testable without egui or any app state.

use csv::StringRecord;

mod row_index;
mod sheet;
pub use row_index::{RowIndex, RowIndexBuilder};
pub use sheet::{MemorySheet, Sheet};

#[cfg(test)]
pub(crate) mod conformance;

pub type ColumnId = usize;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Clone, Default)]
pub struct FileHeader {
    /// Stable identity for this column, assigned once when the file loads
    /// and never reassigned -- unlike its position in the columns Vec, which
    /// shifts on column insert/delete. Mirrors master_row's role for rows.
    pub id: ColumnId,
    pub name: String,
    pub visible: bool,
    pub sort: Option<SortOrder>,
}

/// (master row index, record). The master index is assigned once when a file
/// is loaded and carried through sort/filter so edits to a filtered or sorted
/// view can always be written back to the correct row in master data.
pub type SheetRow = (usize, StringRecord);
pub type SheetVec = Vec<SheetRow>;

/// Update a single cell in a sheet, addressed by master row index. Returns
/// `true` if a row with that master index (and the column) existed.
pub fn edit_record(sheet: &mut SheetVec, master_row: usize, col: usize, value: &str) -> bool {
    if let Some((_, record)) = sheet.iter_mut().find(|(idx, _)| *idx == master_row) {
        if col < record.len() {
            *record = record
                .iter()
                .enumerate()
                .map(|(i, f)| if i == col { value } else { f })
                .collect();
            return true;
        }
    }
    false
}

/// Wrap a field value in double-quotes if it contains `delimiter`, a
/// double-quote, or a newline. Internal double-quotes are escaped by doubling.
pub fn quote_field(value: &str, delimiter: u8) -> String {
    if value.contains(delimiter as char) || value.contains(['"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// `quote_field` for comma-delimited CSV.
pub fn csv_quote(value: &str) -> String {
    quote_field(value, b',')
}

/// Rows of cells as delimited text (newline between rows), quoting only
/// where `delimiter` makes it necessary -- tab for clipboard/TSV, comma for CSV.
pub fn format_rows(rows: &[Vec<String>], delimiter: u8) -> String {
    let sep = (delimiter as char).to_string();
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|c| quote_field(c, delimiter))
                .collect::<Vec<_>>()
                .join(&sep)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse pasted text into a grid of cells. Text containing a tab is TSV (what
/// spreadsheets put on the clipboard). Otherwise, multi-line text is split on
/// whichever delimiter splits it consistently (CSV); anything else -- e.g. a
/// lone "Smith, John" -- is not split, so each line is one cell.
pub fn parse_clipboard(text: &str) -> Vec<Vec<String>> {
    let delimiter = if text.contains('\t') {
        b'\t'
    } else {
        match best_delimiter(text.as_bytes()) {
            Some((rows, _, d)) if rows >= 2 => d,
            _ => b'\t',
        }
    };
    sample_records(text.as_bytes(), delimiter, usize::MAX)
}

pub fn write_csv(
    path: &str,
    headers: &[FileHeader],
    data: &dyn Sheet,
    format: CsvFormat,
) -> Result<(), csv::Error> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(format.delimiter)
        .flexible(true)
        .from_path(path)?;
    if format.has_header {
        writer.write_record(headers.iter().map(|h| h.name.as_str()))?;
    }
    for position in 0..data.len() {
        if let Some(record) = data.record(position) {
            writer.write_record(record.as_ref())?;
        }
    }
    writer.flush()?;
    Ok(())
}

/// A filtered and/or sorted view of a file: positions into the master
/// `SheetVec`, in display order. Positions (not rows) so a view costs 4
/// bytes/row and never goes stale on cell edits; callers must fix them up
/// when master rows are inserted or removed.
pub type View = Vec<u32>;

/// Positions of master rows with any cell containing `filter`. An empty
/// `filter` matches every row, including rows with no cells.
pub fn filter_indices(master: &SheetVec, filter: &str) -> View {
    master
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| filter.is_empty() || r.iter().any(|c| c.contains(filter)))
        .map(|(i, _)| i as u32)
        .collect()
}

/// Sort key for one cell: finite numbers order numerically and before any
/// text; everything else (including empty cells) orders as plain strings.
enum SortKey<'a> {
    Num(f64),
    Text(&'a str),
}

impl<'a> SortKey<'a> {
    fn parse(cell: &'a str) -> Self {
        match cell.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => SortKey::Num(n),
            _ => SortKey::Text(cell),
        }
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.total_cmp(b),
            (SortKey::Num(_), SortKey::Text(_)) => std::cmp::Ordering::Less,
            (SortKey::Text(_), SortKey::Num(_)) => std::cmp::Ordering::Greater,
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
        }
    }
}

/// `view` sorted by column `sort_by.0` of the rows it points at (stable).
/// Numeric cells sort by value (`2` < `10`), ahead of non-numeric ones.
pub fn sort_indices(master: &SheetVec, view: View, sort_by: (usize, SortOrder)) -> View {
    let mut keyed: Vec<(SortKey, u32)> = view
        .into_iter()
        .map(|i| {
            let cell = master[i as usize].1.get(sort_by.0).unwrap_or_default();
            (SortKey::parse(cell), i)
        })
        .collect();
    match sort_by.1 {
        SortOrder::Asc => keyed.sort_by(|a, b| a.0.cmp(&b.0)),
        SortOrder::Dsc => keyed.sort_by(|a, b| b.0.cmp(&a.0)),
    }
    keyed.into_iter().map(|(_, i)| i).collect()
}

/// How a file is laid out on disk, so it can be read and written back the
/// same way.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct CsvFormat {
    pub delimiter: u8,
    /// Whether the first row holds column names (otherwise it is data).
    pub has_header: bool,
}

impl Default for CsvFormat {
    fn default() -> Self {
        CsvFormat {
            delimiter: b',',
            has_header: true,
        }
    }
}

const DELIMITER_CANDIDATES: [u8; 4] = [b',', b';', b'\t', b'|'];
const SNIFF_BYTES: u64 = 64 * 1024;
const SNIFF_RECORDS: usize = 100;

fn sample_records(sample: &[u8], delimiter: u8, limit: usize) -> Vec<Vec<String>> {
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(sample)
        .records()
        .take(limit)
        .filter_map(|r| r.ok())
        .map(|r| r.iter().map(str::to_string).collect())
        .collect()
}

/// The candidate delimiter that splits the most sampled records into the
/// same number (> 1) of fields, as (matching rows, fields, delimiter). Ties
/// favour the earlier candidate (comma first).
fn best_delimiter(sample: &[u8]) -> Option<(usize, usize, u8)> {
    let mut best: Option<(usize, usize, u8)> = None;
    for &d in &DELIMITER_CANDIDATES {
        let records = sample_records(sample, d, SNIFF_RECORDS);
        let mut counts = std::collections::HashMap::new();
        for r in &records {
            *counts.entry(r.len()).or_insert(0usize) += 1;
        }
        let Some((&fields, &rows)) = counts.iter().max_by_key(|(f, c)| (**c, **f)) else {
            continue;
        };
        if fields > 1 && best.is_none_or(|(br, bf, _)| (rows, fields) > (br, bf)) {
            best = Some((rows, fields, d));
        }
    }
    best
}

/// Guess the delimiter of a sample of file content; comma if unclear.
pub fn sniff_delimiter(sample: &[u8]) -> u8 {
    best_delimiter(sample).map_or(b',', |(_, _, d)| d)
}

/// Guess whether the first row is a header: it is data if some column's
/// first cell is numeric while every other sampled cell in that column is
/// too (a header would be text above numbers). Otherwise it is a header.
pub fn sniff_has_header(records: &[Vec<String>]) -> bool {
    let Some((first, rest)) = records.split_first() else {
        return true;
    };
    if rest.is_empty() {
        return true;
    }
    let numeric = |c: &str| matches!(SortKey::parse(c), SortKey::Num(_));
    let first_row_is_data = first.iter().enumerate().any(|(col, cell)| {
        numeric(cell) && rest.iter().all(|r| r.get(col).is_some_and(|c| numeric(c)))
    });
    !first_row_is_data
}

pub fn sniff_format(path: &str) -> std::io::Result<CsvFormat> {
    use std::io::Read;
    let mut buf = Vec::new();
    let file_len = std::fs::metadata(path)?.len();
    std::fs::File::open(path)?
        .take(SNIFF_BYTES)
        .read_to_end(&mut buf)?;
    // Drop a trailing partial line so it can't skew the field counts.
    if file_len > SNIFF_BYTES {
        if let Some(end) = buf.iter().rposition(|b| *b == b'\n') {
            buf.truncate(end + 1);
        }
    }
    let delimiter = sniff_delimiter(&buf);
    let has_header = sniff_has_header(&sample_records(&buf, delimiter, SNIFF_RECORDS));
    Ok(CsvFormat {
        delimiter,
        has_header,
    })
}

pub fn iterate_csv(
    path: &str,
    format: CsvFormat,
) -> csv::Result<(csv::Reader<std::fs::File>, StringRecord)> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(format.delimiter)
        .has_headers(format.has_header)
        .flexible(true)
        .from_path(path)?;
    let headers = if format.has_header {
        rdr.headers()?.clone()
    } else {
        // Peek at the first row only to learn the column count.
        let mut first = StringRecord::new();
        let columns = if rdr.read_record(&mut first)? {
            first.len()
        } else {
            0
        };
        let mut rdr2 = csv::ReaderBuilder::new()
            .delimiter(format.delimiter)
            .has_headers(false)
            .flexible(true)
            .from_path(path)?;
        std::mem::swap(&mut rdr, &mut rdr2);
        (1..=columns).map(|i| format!("Column {i}")).collect()
    };
    Ok((rdr, headers))
}

/// Open a CSV file, sniffing its delimiter and whether it has a header row.
pub fn open_csv_file(path: &str) -> (csv::Reader<std::fs::File>, Vec<FileHeader>, CsvFormat) {
    let format = sniff_format(path).unwrap_or_default();
    match iterate_csv(path, format) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .enumerate()
                .map(|(id, name)| FileHeader {
                    id,
                    name: name.to_string(),
                    visible: true,
                    ..FileHeader::default()
                })
                .collect::<Vec<_>>();
            (csv_reader, headers, format)
        }
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    }
}

/// What the loader had to do to rows it could not read verbatim.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct LoadStats {
    pub lossy_rows: usize,
    pub skipped_rows: usize,
}

/// Read every row of `reader` verbatim: rows with more or fewer fields than
/// the header are kept as they are, and a row that isn't valid UTF-8 is kept
/// with U+FFFD in place of the bad bytes (counted in `lossy_rows`). Only a row
/// the parser cannot produce at all is skipped (counted in `skipped_rows`).
/// The reader must be flexible (see `iterate_csv`), or ragged rows error.
pub fn read_rows<R: std::io::Read>(reader: &mut csv::Reader<R>) -> (SheetVec, LoadStats) {
    let mut rows = SheetVec::new();
    let mut stats = LoadStats::default();
    let mut raw = csv::ByteRecord::new();
    loop {
        match reader.read_byte_record(&mut raw) {
            Ok(false) => break,
            Ok(true) => {
                let record = StringRecord::from_byte_record(raw.clone()).unwrap_or_else(|_| {
                    stats.lossy_rows += 1;
                    StringRecord::from_byte_record_lossy(raw.clone())
                });
                rows.push((rows.len(), record));
            }
            Err(err) if err.is_io_error() => {
                stats.skipped_rows += 1;
                break;
            }
            Err(_) => stats.skipped_rows += 1,
        }
    }
    (rows, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(idx: usize, fields: &[&str]) -> (usize, StringRecord) {
        (idx, StringRecord::from(fields.to_vec()))
    }

    #[test]
    fn edit_record_updates_by_master_row_not_position() {
        let mut sheet: SheetVec = vec![row(5, &["a", "b"]), row(2, &["c", "d"])];
        assert!(edit_record(&mut sheet, 2, 1, "z"));
        assert_eq!(sheet[1].1.get(1), Some("z"));
        assert_eq!(sheet[0].1.get(1), Some("b"));
    }

    #[test]
    fn edit_record_missing_master_row_returns_false() {
        let mut sheet: SheetVec = vec![row(0, &["a"])];
        assert!(!edit_record(&mut sheet, 99, 0, "z"));
    }

    #[test]
    fn edit_record_out_of_range_col_returns_false() {
        let mut sheet: SheetVec = vec![row(0, &["a"])];
        assert!(!edit_record(&mut sheet, 0, 5, "z"));
    }

    #[test]
    fn csv_quote_wraps_when_special_chars_present() {
        assert_eq!(csv_quote("plain"), "plain");
        assert_eq!(csv_quote("a,b"), "\"a,b\"");
        assert_eq!(csv_quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_quote("a\nb"), "\"a\nb\"");
    }

    #[test]
    fn sort_indices_orders_by_column_and_keeps_master_rows() {
        let sheet: SheetVec = vec![row(0, &["3"]), row(1, &["1"]), row(2, &["2"])];
        let all: View = vec![0, 1, 2];
        assert_eq!(
            sort_indices(&sheet, all.clone(), (0, SortOrder::Asc)),
            vec![1, 2, 0]
        );
        assert_eq!(
            sort_indices(&sheet, all, (0, SortOrder::Dsc)),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn sort_indices_orders_numbers_by_value_before_text() {
        let sheet: SheetVec = ["10", "abc", "2", "-1.5", "", "3"]
            .iter()
            .enumerate()
            .map(|(i, v)| (i, StringRecord::from(vec![*v])))
            .collect();
        let all: View = (0..sheet.len() as u32).collect();
        assert_eq!(
            sort_indices(&sheet, all.clone(), (0, SortOrder::Asc)),
            vec![3, 2, 5, 0, 4, 1]
        );
        assert_eq!(
            sort_indices(&sheet, all, (0, SortOrder::Dsc)),
            vec![1, 4, 0, 5, 2, 3]
        );
    }

    #[test]
    fn sort_indices_only_reorders_the_given_view() {
        let sheet: SheetVec = vec![row(0, &["3"]), row(1, &["1"]), row(2, &["2"])];
        assert_eq!(
            sort_indices(&sheet, vec![2, 0], (0, SortOrder::Asc)),
            vec![2, 0]
        );
    }

    #[test]
    fn filter_indices_returns_matching_positions() {
        let sheet: SheetVec = vec![row(7, &["apple"]), row(8, &["banana"]), row(9, &["grape"])];
        assert_eq!(filter_indices(&sheet, "an"), vec![1]);
    }

    #[test]
    fn filter_indices_empty_filter_matches_everything() {
        let sheet: SheetVec = vec![row(0, &["a"]), row(1, &["b"])];
        assert_eq!(filter_indices(&sheet, ""), vec![0, 1]);
    }

    #[test]
    fn sniffs_common_delimiters() {
        assert_eq!(sniff_delimiter(b"a,b,c\n1,2,3\n"), b',');
        assert_eq!(sniff_delimiter(b"a;b;c\n1;2;3\n"), b';');
        assert_eq!(sniff_delimiter(b"a\tb\tc\n1\t2\t3\n"), b'\t');
        assert_eq!(sniff_delimiter(b"a|b\n1|2\n"), b'|');
        assert_eq!(sniff_delimiter(b"single\ncolumn\n"), b',');
        assert_eq!(sniff_delimiter(b""), b',');
    }

    #[test]
    fn sniff_ignores_delimiters_inside_quotes_and_decimal_commas() {
        assert_eq!(sniff_delimiter(b"a;b\n\"x,y,z\";1,5\n\"p,q\";2,5\n"), b';');
    }

    #[test]
    fn detects_header_row() {
        let rec = |rows: &[&[&str]]| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect())
                .collect()
        };
        assert!(sniff_has_header(&rec(&[
            &["name", "age"],
            &["bob", "3"],
            &["al", "4"]
        ])));
        assert!(!sniff_has_header(&rec(&[
            &["1", "x"],
            &["2", "y"],
            &["3", "z"]
        ])));
        assert!(sniff_has_header(&rec(&[&["a", "b"]])));
        assert!(sniff_has_header(&rec(&[&["id", "n"], &["1", "2"]])));
    }

    #[test]
    fn open_and_write_round_trip_semicolon_headerless() {
        let dir = std::env::temp_dir().join(format!("csv_model_sniff_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.csv");
        let path = path.to_str().unwrap();
        std::fs::write(path, "1;a\n2;b\n3;c\n").unwrap();
        let (mut rdr, headers, format) = open_csv_file(path);
        assert_eq!(
            format,
            CsvFormat {
                delimiter: b';',
                has_header: false
            }
        );
        assert_eq!(
            headers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["Column 1", "Column 2"]
        );
        let data: SheetVec = rdr.records().filter_map(|r| r.ok()).enumerate().collect();
        assert_eq!(data.len(), 3);
        write_csv(path, &headers, &MemorySheet::new(data), format).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "1;a\n2;b\n3;c\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn format_rows_quotes_only_for_the_chosen_delimiter() {
        let rows = vec![vec![
            "Smith, John".to_string(),
            "x\ty".to_string(),
            "q\"".to_string(),
        ]];
        assert_eq!(
            format_rows(&rows, b'\t'),
            "Smith, John\t\"x\ty\"\t\"q\"\"\""
        );
        assert_eq!(format_rows(&rows, b','), "\"Smith, John\",x\ty,\"q\"\"\"");
    }

    #[test]
    fn parse_clipboard_detects_tsv_csv_and_plain_text() {
        let g = |rows: &[&[&str]]| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect())
                .collect()
        };
        assert_eq!(
            parse_clipboard("a\tb\nc\td"),
            g(&[&["a", "b"], &["c", "d"]])
        );
        assert_eq!(parse_clipboard("a\t\"x,y\"\n"), g(&[&["a", "x,y"]]));
        assert_eq!(parse_clipboard("a,b\nc,d"), g(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(parse_clipboard("a;b\nc;d"), g(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(parse_clipboard("Smith, John"), g(&[&["Smith, John"]]));
        assert_eq!(parse_clipboard("one\ntwo"), g(&[&["one"], &["two"]]));
        assert_eq!(parse_clipboard("a\tb\nc"), g(&[&["a", "b"], &["c"]]));
    }

    #[test]
    fn tsv_round_trips_through_format_and_parse() {
        let rows = vec![
            vec!["a,b".to_string(), "c\"d".to_string()],
            vec!["e\nf".to_string(), "".to_string()],
        ];
        assert_eq!(parse_clipboard(&format_rows(&rows, b'\t')), rows);
    }

    fn temp_file(name: &str, bytes: &[u8]) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("csv_model_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.csv");
        std::fs::write(&path, bytes).unwrap();
        let p = path.to_str().unwrap().to_string();
        (dir, p)
    }

    fn lens(rows: &SheetVec) -> Vec<usize> {
        rows.iter().map(|(_, r)| r.len()).collect()
    }

    #[test]
    fn read_rows_keeps_ragged_rows() {
        let (dir, path) = temp_file("ragged", b"a,b,c\n1,2\n3,4,5,6\n7,8,9\n");
        let (mut rdr, _, _) = open_csv_file(&path);
        let (rows, stats) = read_rows(&mut rdr);
        assert_eq!(lens(&rows), vec![2, 4, 3]);
        assert_eq!(
            rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(stats, LoadStats::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_rows_keeps_invalid_utf8_rows_lossily_and_counts_them() {
        let (dir, path) = temp_file("utf8", b"a,b\n1,2\nx,\xff\n3,4\n");
        let (mut rdr, _, _) = open_csv_file(&path);
        let (rows, stats) = read_rows(&mut rdr);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].1.get(1), Some("\u{fffd}"));
        assert_eq!(
            stats,
            LoadStats {
                lossy_rows: 1,
                skipped_rows: 0
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_csv_saves_ragged_rows_unchanged() {
        let (dir, path) = temp_file("ragged_save", b"a,b,c\n1,2\n3,4,5,6\n7,8,9\n");
        let (mut rdr, headers, format) = open_csv_file(&path);
        let (rows, _) = read_rows(&mut rdr);
        write_csv(&path, &headers, &MemorySheet::new(rows), format).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a,b,c\n1,2\n3,4,5,6\n7,8,9\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
