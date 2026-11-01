# Measurement spikes

Throwaway programs behind the numbers in `../large-file-engine.md`. Not part of
the workspace build.

```
cd docs/design/spikes
cargo build --release
./target/release/spikes gen      # writes a 4 GB /tmp/spike_big.csv (change PATH/size in main.rs)
./target/release/spikes s12      # sequential + random reads: pread vs mmap, block sizes
./target/release/spikes s3       # parallel scaling: memmem and csv_core scan, 1/2/4 threads
./target/release/spikes s4       # key extraction + sort (10M real rows, 50M/131M synthetic keys)
./target/release/spikes s5       # raw copy + re-serialise to a real file
./target/release/spikes s6       # re-serialise CPU cost, output discarded
```

**Run these on a real laptop with a real SSD.** In the cloud sandbox used for
the first run, `posix_fadvise(DONTNEED)` and `drop_caches` did not produce a
cold cache (cold random reads cost the same as warm ones), and the disk wrote
at only ~160 MB/s. Cold-read and write numbers from that run are not
representative.
