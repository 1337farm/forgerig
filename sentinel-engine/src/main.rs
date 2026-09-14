//! Benchmark / verification binary for the sentinel engine.
//!
//! Builds a synthetic corpus, checks the transforms, and reports scan
//! throughput for parallel processing. Primarily a sanity harness; the actual
//! correctness guarantees live in the unit tests in lib.rs.

use sentinel_engine::*;
use std::time::Instant;

fn synthetic(files: usize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(files * 400);
    let diff = b"\xC2\xB6-\n\tuser: T | null;\n\xC2\xB6+\n\tuser: T | undefined;\n";
    for i in 0..files {
        payload.extend_from_slice(format!("\u{00A7}src/mod{i}.rs\n").as_bytes());
        payload.extend_from_slice(b"fn f() {}\n");
        payload.extend_from_slice(diff);
    }
    payload
}

fn main() {
    let files = 100_000;
    let corpus = synthetic(files);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    println!("corpus: {:.2} MB, {} files, {} threads", corpus.len() as f64 / 1_048_576.0, files, threads);

    let t0 = Instant::now();
    let hits = scan_parallel(&corpus, 2_097_152, threads);
    let dt = t0.elapsed().as_secs_f64();
    println!("scan: {} boundaries in {:.3} ms ({:.2} GB/s)", hits.len(), dt * 1000.0, (corpus.len() as f64 / 1e9) / dt);

    let macro_count = hits.iter().filter(|h| h.kind == BoundaryKind::MacroFile).count();
    let micro_count = hits.iter().filter(|h| h.kind == BoundaryKind::MicroHunk).count();
    println!("macro={macro_count} micro={micro_count} (expect macro={files} micro={})", files * 2);

    // Zero-copy pass.
    let records: Vec<_> = ZeroCopyRecordIterator::new(&corpus, hits).collect();
    let bytes: usize = records.iter().map(|r| r.body.len()).sum();
    println!("records={} payload_bytes={bytes} (zero-copy slices)", records.len());
}