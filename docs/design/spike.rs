use csv_model::{FileSheet, RowIndexBuilder, Sheet, VecSource};
use std::sync::Arc;
use std::time::Instant;

fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap();
    s.split_whitespace().nth(1).unwrap().parse::<f64>().unwrap() * 4096.0 / 1e6
}

fn main() {
    let rows = 3_000_000usize;
    let mut text = String::with_capacity(rows * 100);
    for i in 0..rows {
        use std::fmt::Write;
        write!(text, "{i},user{i},city{},{}.{:02},2024-{:02}-{:02},{},\"note, with comma {i}\",x{}\n",
            i % 97, i % 1000, i % 100, 1 + i % 12, 1 + i % 28, i % 2, i % 7).unwrap();
    }
    let bytes = text.into_bytes();
    let mb = bytes.len() as f64 / 1e6;
    eprintln!("file: {rows} rows, {mb:.0} MB ({:.0} B/row)", mb * 1e6 / rows as f64);

    // A. index build
    let t = Instant::now();
    let mut b = RowIndexBuilder::new(b',', false);
    for c in bytes.chunks(1 << 20) { b.feed(c); }
    let idx = b.finish();
    let d = t.elapsed();
    eprintln!("A index build:        {d:>9.2?}  = {:>6.0} MB/s, {:.2} us/row  (rows={})", mb / d.as_secs_f64(), d.as_secs_f64() * 1e6 / rows as f64, idx.len());

    // B. current FileSheet cell read, no row cache
    let sheet = FileSheet::with_cache_capacity(Arc::new(VecSource(bytes.clone())), b',', false, 0).unwrap();
    let n = 100_000;
    let t = Instant::now();
    for p in 0..n { let _ = sheet.cell(p * 20, 1); }
    let d = t.elapsed();
    eprintln!("B current row read:   {:.2} us/row  ({} rows)", d.as_secs_f64() * 1e6 / n as f64, n);

    // C. lean parse: one csv_core reader, reused buffers, no allocation per row
    let ranges: Vec<_> = (0..idx.len()).map(|i| idx.row_range(i).unwrap()).collect();
    let mut rdr = csv_core::ReaderBuilder::new().build();
    let mut out = vec![0u8; 64 * 1024];
    let mut ends = vec![0usize; 1024];
    let t = Instant::now();
    let mut hits = 0usize;
    let m = 1_000_000;
    for r in ranges.iter().take(m) {
        rdr.reset();
        let slice = &bytes[r.start as usize..r.end as usize];
        let (_res, _nin, nout, nends) = rdr.read_record(slice, &mut out, &mut ends);
        let mut s = 0;
        for &e in &ends[..nends] {
            if out[s..e.min(nout)].windows(8).any(|w| w == b"zzzzzzzz") { hits += 1; }
            s = e;
        }
    }
    let d = t.elapsed();
    eprintln!("C lean parse+scan:    {:.2} us/row  -> full 3M rows would be {:.1}s (hits={hits})", d.as_secs_f64() * 1e6 / m as f64, d.as_secs_f64() * 1e6 / m as f64 * rows as f64 / 1e6);

    // D. whole-buffer substring search (rare and common needle)
    for needle in ["zzzzzzzz", "user12345", "city9"] {
        let f = memchr::memmem::Finder::new(needle);
        let t = Instant::now();
        let count = f.find_iter(&bytes).count();
        let d = t.elapsed();
        eprintln!("D memmem {needle:<10}: {d:>9.2?} = {:>5.1} GB/s, {count} matches", mb / 1e3 / d.as_secs_f64());
    }

    // E. memory of Vec<StringRecord> for 1M rows
    let before = rss_mb();
    let mut rd = csv::ReaderBuilder::new().has_headers(false).from_reader(&bytes[..]);
    let recs: Vec<(usize, csv::StringRecord)> = rd.records().take(1_000_000).map(|r| r.unwrap()).enumerate().collect();
    let after = rss_mb();
    let slice_bytes: usize = ranges.iter().take(1_000_000).map(|r| (r.end - r.start) as usize).sum();
    eprintln!("E StringRecord rows:  {:.0} MB RSS for {:.0} MB of file text = {:.1}x  ({:.0} B/row)", after - before, slice_bytes as f64 / 1e6, (after - before) * 1e6 / slice_bytes as f64, (after - before) * 1e6 / 1e6);
    drop(recs);
    eprintln!("F row index:          {:.0} MB for {rows} rows (8 B/row)", rows as f64 * 8.0 / 1e6);
}
