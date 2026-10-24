use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::time::Instant;

const PATH: &str = "/tmp/spike_big.csv";
const MIB: usize = 1 << 20;

fn evict(f: &File) {
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
}

fn gb(bytes: u64, secs: f64) -> f64 { bytes as f64 / 1e9 / secs }

fn gen(target_gb: f64) {
    let mut block = String::new();
    for i in 0..1_000_000usize {
        use std::fmt::Write as _;
        write!(block, "{i},user{i},city{},{}.{:02},2024-{:02}-{:02},{},\"note, with comma {i}\",x{}\n",
            i % 97, i % 1000, i % 100, 1 + i % 12, 1 + i % 28, i % 2, i % 7).unwrap();
    }
    let mut f = BufWriter::with_capacity(8 * MIB, File::create(PATH).unwrap());
    let reps = (target_gb * 1e9 / block.len() as f64).ceil() as usize;
    let t = Instant::now();
    for _ in 0..reps { f.write_all(block.as_bytes()).unwrap(); }
    f.flush().unwrap();
    f.get_ref().sync_all().unwrap();
    let len = f.get_ref().metadata().unwrap().len();
    eprintln!("generated {PATH}: {:.2} GB, {} rows, in {:.1?}", len as f64 / 1e9, reps * 1_000_000, t.elapsed());
}

fn seq_pread(f: &File, block: usize, needle: &memchr::memmem::Finder) -> (u64, usize, f64) {
    let len = f.metadata().unwrap().len();
    let mut buf = vec![0u8; block];
    let (mut at, mut hits) = (0u64, 0usize);
    let t = Instant::now();
    while at < len {
        let n = f.read_at(&mut buf, at).unwrap();
        if n == 0 { break; }
        hits += needle.find_iter(&buf[..n]).count();
        at += n as u64;
    }
    (len, hits, t.elapsed().as_secs_f64())
}

fn s1_s2() {
    let f = File::open(PATH).unwrap();
    let needle = memchr::memmem::Finder::new("zzzzzzzz");
    eprintln!("== S1/S2: sequential read + search (no-match needle), 4 GB file ==");
    for block in [64 * 1024, MIB, 8 * MIB] {
        evict(&f);
        let (len, _, s) = seq_pread(&f, block, &needle);
        eprintln!("cold pread block={:>5} KiB: {:>6.2} GB/s ({:.1}s)", block / 1024, gb(len, s), s);
    }
    for block in [64 * 1024, MIB, 8 * MIB] {
        let (len, _, s) = seq_pread(&f, block, &needle);
        eprintln!("warm pread block={:>5} KiB: {:>6.2} GB/s ({:.1}s)", block / 1024, gb(len, s), s);
    }
    evict(&f);
    let m = unsafe { Mmap::map(&f).unwrap() };
    let t = Instant::now();
    let hits = needle.find_iter(&m[..]).count();
    eprintln!("cold mmap search:         {:>6.2} GB/s ({:.1}s) hits={hits}", gb(m.len() as u64, t.elapsed().as_secs_f64()), t.elapsed().as_secs_f64());
    let t = Instant::now();
    let hits = needle.find_iter(&m[..]).count();
    eprintln!("warm mmap search:         {:>6.2} GB/s ({:.1}s) hits={hits}", gb(m.len() as u64, t.elapsed().as_secs_f64()), t.elapsed().as_secs_f64());

    // random row-sized reads
    let len = f.metadata().unwrap().len();
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut next = move || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
    for (label, warm, n) in [("warm", true, 200_000usize), ("cold", false, 5_000usize)] {
        let offs: Vec<u64> = (0..n).map(|_| next() % (len - 4096)).collect();
        if !warm { evict(&f); }
        let mut buf = [0u8; 256];
        let t = Instant::now();
        let mut sum = 0u64;
        for &o in &offs { f.read_at(&mut buf, o).unwrap(); sum += buf[7] as u64; }
        let d = t.elapsed();
        eprintln!("{label} random 256B pread: {:>7.2} us/read ({n} reads) [{sum}]", d.as_secs_f64() * 1e6 / n as f64);
        if !warm { evict(&f); }
        let t = Instant::now();
        let mut sum = 0u64;
        for &o in &offs { sum += m[o as usize + 7] as u64 + m[o as usize + 200] as u64; }
        let d = t.elapsed();
        eprintln!("{label} random 256B mmap:  {:>7.2} us/read ({n} reads) [{sum}]", d.as_secs_f64() * 1e6 / n as f64);
    }
}

fn lean_scan(chunk: &[u8], finder: &memchr::memmem::Finder) -> (usize, usize) {
    let mut rdr = csv_core::ReaderBuilder::new().build();
    let mut out = vec![0u8; 64 * 1024];
    let mut ends = vec![0usize; 1024];
    let (mut rows, mut hits) = (0usize, 0usize);
    let mut input = chunk;
    loop {
        let (res, nin, nout, _ne) = rdr.read_record(input, &mut out, &mut ends);
        input = &input[nin..];
        match res {
            csv_core::ReadRecordResult::Record => { rows += 1; if finder.find(&out[..nout]).is_some() { hits += 1; } }
            csv_core::ReadRecordResult::End => break,
            csv_core::ReadRecordResult::InputEmpty => { if input.is_empty() { /* flush */ let (r2, _, no2, _) = rdr.read_record(&[], &mut out, &mut ends); if let csv_core::ReadRecordResult::Record = r2 { rows += 1; if finder.find(&out[..no2]).is_some() { hits += 1; } } break; } }
            _ => {}
        }
    }
    (rows, hits)
}

fn s3() {
    let f = File::open(PATH).unwrap();
    let m = unsafe { Mmap::map(&f).unwrap() };
    let _ = memchr::memmem::find_iter(&m[..], b"zzzzzzzz").count(); // warm
    eprintln!("== S3: parallel scaling (warm, 4 vCPUs) ==");
    let needle = memchr::memmem::Finder::new("zzzzzzzz");
    for threads in [1usize, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let t = Instant::now();
        let hits: usize = pool.install(|| m.par_chunks(64 * MIB).map(|c| needle.find_iter(c).count()).sum());
        let s = t.elapsed().as_secs_f64();
        eprintln!("memmem  {threads} thread(s): {:>6.2} GB/s  hits={hits}", gb(m.len() as u64, s));
    }
    // lean csv_core scan over row-aligned 64 MiB chunks of the first 1 GiB
    let limit = 1usize << 30;
    let mut cuts = vec![0usize];
    let mut p = 64 * MIB;
    while p < limit {
        let nl = memchr::memchr(b'\n', &m[p..]).unwrap();
        cuts.push(p + nl + 1);
        p += 64 * MIB;
    }
    cuts.push(limit - memchr::memrchr(b'\n', &m[..limit]).map(|i| limit - i - 1).unwrap());
    let chunks: Vec<&[u8]> = cuts.windows(2).map(|w| &m[w[0]..w[1]]).collect();
    let user = memchr::memmem::Finder::new("user1234567");
    for threads in [1usize, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let t = Instant::now();
        let (rows, hits) = pool.install(|| chunks.par_iter().map(|c| lean_scan(c, &user)).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1)));
        let s = t.elapsed().as_secs_f64();
        eprintln!("csv_core scan {threads} thread(s): {:>6.2} M rows/s  {:>6.2} GB/s  rows={rows} hits={hits}", rows as f64 / 1e6 / s, gb(cuts[cuts.len()-1] as u64, s));
    }
}

fn key_num(field: &[u8]) -> u64 {
    let v: f64 = std::str::from_utf8(field).ok().and_then(|s| s.parse().ok()).unwrap_or(f64::NAN);
    let b = v.to_bits();
    if b >> 63 == 1 { !b } else { b | (1 << 63) }
}
fn key_text(field: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = field.len().min(8);
    a[..n].copy_from_slice(&field[..n]);
    u64::from_be_bytes(a)
}

fn extract(chunk: &[u8], col: usize, numeric: bool, base: u32) -> Vec<(u64, u32)> {
    let mut rdr = csv_core::ReaderBuilder::new().build();
    let mut out = vec![0u8; 64 * 1024];
    let mut ends = vec![0usize; 1024];
    let mut keys = Vec::new();
    let mut input = chunk;
    let mut pos = base;
    loop {
        let (res, nin, nout, ne) = rdr.read_record(input, &mut out, &mut ends);
        input = &input[nin..];
        match res {
            csv_core::ReadRecordResult::Record => {
                let s = if col == 0 { 0 } else { ends[col - 1] };
                let e = if col < ne { ends[col] } else { s };
                let _ = nout;
                keys.push((if numeric { key_num(&out[s..e]) } else { key_text(&out[s..e]) }, pos));
                pos += 1;
            }
            csv_core::ReadRecordResult::End => break,
            csv_core::ReadRecordResult::InputEmpty if input.is_empty() => break,
            _ => {}
        }
    }
    keys
}

fn s4() {
    let f = File::open(PATH).unwrap();
    let m = unsafe { Mmap::map(&f).unwrap() };
    let _ = memchr::memchr_iter(b'\n', &m[..1 << 30]).count(); // warm first GiB
    eprintln!("== S4: sort ==");
    let limit = 760 * 1_000_000usize; // ~10M rows
    let end = limit - memchr::memrchr(b'\n', &m[..limit]).map(|i| limit - i - 1).unwrap();
    // split into 4 row-aligned chunks to extract in parallel; also time single-thread extract
    for (label, col, numeric) in [("numeric col 3 (f64)", 3usize, true), ("text col 1 (8-byte prefix)", 1usize, false)] {
        let t = Instant::now();
        let keys1 = extract(&m[..end], col, numeric, 0);
        let ex1 = t.elapsed();
        let n = keys1.len();
        let mut k = keys1.clone();
        let t = Instant::now();
        k.sort_unstable();
        let s1 = t.elapsed();
        let mut k = keys1.clone();
        let t = Instant::now();
        k.par_sort_unstable();
        let sp = t.elapsed();
        eprintln!("{label}: {n} rows | extract 1 thread {ex1:.2?} ({:.1} M rows/s) | sort 1 thread {s1:.2?} | par_sort (4) {sp:.2?}", n as f64 / 1e6 / ex1.as_secs_f64());
    }
    // synthetic key sorts at 10 GB scale
    for n in [50_000_000usize, 131_000_000] {
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut keys: Vec<(u64, u32)> = (0..n).map(|i| { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; (rng, i as u32) }).collect();
        let t = Instant::now();
        keys.par_sort_unstable();
        eprintln!("synthetic {n} random (u64,u32) keys: par_sort (4 threads) {:.1?}, memory {:.2} GB", t.elapsed(), (n * 16) as f64 / 1e9);
    }
}

fn s5() {
    eprintln!("== S5: write ==");
    let src = File::open(PATH).unwrap();
    let len = src.metadata().unwrap().len();
    let dst_path = "/tmp/spike_out.csv";
    let mut dst = File::create(dst_path).unwrap();
    let mut buf = vec![0u8; MIB];
    let (mut at, t) = (0u64, Instant::now());
    while at < len { let n = src.read_at(&mut buf, at).unwrap(); dst.write_all(&buf[..n]).unwrap(); at += n as u64; }
    let w = t.elapsed();
    dst.sync_all().unwrap();
    let total = t.elapsed();
    eprintln!("raw copy 4 GB:   write loop {:.1?} ({:.2} GB/s), incl. fsync {:.1?} ({:.2} GB/s)", w, gb(len, w.as_secs_f64()), total, gb(len, total.as_secs_f64()));
    std::fs::rename(dst_path, "/tmp/spike_out2.csv").unwrap();
    // re-serialise first 1 GiB
    let m = unsafe { Mmap::map(&src).unwrap() };
    let limit = 1usize << 30;
    let end = limit - memchr::memrchr(b'\n', &m[..limit]).map(|i| limit - i - 1).unwrap();
    let mut rdr = csv_core::ReaderBuilder::new().build();
    let mut out = vec![0u8; 64 * 1024];
    let mut ends = vec![0usize; 1024];
    let mut w = BufWriter::with_capacity(4 * MIB, File::create(dst_path).unwrap());
    let mut input = &m[..end];
    let (mut rows, t) = (0usize, Instant::now());
    loop {
        let (res, nin, _nout, ne) = rdr.read_record(input, &mut out, &mut ends);
        input = &input[nin..];
        match res {
            csv_core::ReadRecordResult::Record => {
                let mut s = 0;
                for (i, &e) in ends[..ne].iter().enumerate() {
                    if i > 0 { w.write_all(b",").unwrap(); }
                    let field = &out[s..e];
                    if field.iter().any(|&b| b == b',' || b == b'"' || b == b'\n' || b == b'\r') {
                        w.write_all(b"\"").unwrap();
                        for &b in field { if b == b'"' { w.write_all(b"\"\"").unwrap() } else { w.write_all(&[b]).unwrap() } }
                        w.write_all(b"\"").unwrap();
                    } else { w.write_all(field).unwrap(); }
                    s = e;
                }
                w.write_all(b"\n").unwrap();
                rows += 1;
            }
            csv_core::ReadRecordResult::End => break,
            csv_core::ReadRecordResult::InputEmpty if input.is_empty() => break,
            _ => {}
        }
    }
    w.flush().unwrap();
    let d = t.elapsed();
    eprintln!("re-serialise 1 GiB: {rows} rows in {d:.1?} = {:.2} M rows/s, {:.2} GB/s (naive per-byte quoting)", rows as f64 / 1e6 / d.as_secs_f64(), gb(end as u64, d.as_secs_f64()));
    let _ = std::fs::remove_file(dst_path);
    let _ = std::fs::remove_file("/tmp/spike_out2.csv");
}

fn s6() {
    eprintln!("== S6: re-serialise CPU cost (output discarded) ==");
    let f = File::open(PATH).unwrap();
    let m = unsafe { Mmap::map(&f).unwrap() };
    let limit = 1usize << 30;
    let end = limit - memchr::memrchr(b'\n', &m[..limit]).map(|i| limit - i - 1).unwrap();
    let _ = memchr::memchr_iter(b'\n', &m[..end]).count();
    let mut rdr = csv_core::ReaderBuilder::new().build();
    let mut out = vec![0u8; 64 * 1024];
    let mut ends = vec![0usize; 1024];
    let mut w = BufWriter::with_capacity(4 * MIB, std::io::sink());
    let mut input = &m[..end];
    let (mut rows, t) = (0usize, Instant::now());
    loop {
        let (res, nin, _nout, ne) = rdr.read_record(input, &mut out, &mut ends);
        input = &input[nin..];
        match res {
            csv_core::ReadRecordResult::Record => {
                let mut s = 0;
                for (i, &e) in ends[..ne].iter().enumerate() {
                    if i > 0 { w.write_all(b",").unwrap(); }
                    let field = &out[s..e];
                    if memchr::memchr3(b',', b'"', b'\n', field).is_some() || memchr::memchr(b'\r', field).is_some() {
                        w.write_all(b"\"").unwrap();
                        let mut last = 0;
                        for q in memchr::memchr_iter(b'"', field) { w.write_all(&field[last..=q]).unwrap(); w.write_all(b"\"").unwrap(); last = q + 1; }
                        w.write_all(&field[last..]).unwrap();
                        w.write_all(b"\"").unwrap();
                    } else { w.write_all(field).unwrap(); }
                    s = e;
                }
                w.write_all(b"\n").unwrap();
                rows += 1;
            }
            csv_core::ReadRecordResult::End => break,
            csv_core::ReadRecordResult::InputEmpty if input.is_empty() => break,
            _ => {}
        }
    }
    let d = t.elapsed();
    eprintln!("re-serialise 1 GiB to sink: {rows} rows in {d:.2?} = {:.2} M rows/s, {:.2} GB/s", rows as f64 / 1e6 / d.as_secs_f64(), gb(end as u64, d.as_secs_f64()));
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("gen") => gen(4.0),
        Some("s12") => s1_s2(),
        Some("s3") => s3(),
        Some("s4") => s4(),
        Some("s5") => s5(),
        Some("s6") => s6(),
        _ => eprintln!("usage: gen|s12|s3|s4|s5"),
    }
}
