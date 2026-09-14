//! Benchmark / verification binary for the sentinel engine.
//!
//! Builds a synthetic corpus, checks the transforms, and reports scan
//! throughput for parallel processing. Primarily a sanity harness; the actual
//! correctness guarantees live in the unit tests in lib.rs.
//!
//! Usage:
//!   cargo run --release -- [OPTIONS]
//!   --files N          Number of synthetic files (default: 100000)
//!   --chunk-size N     Scan chunk size in bytes (default: 2097152)
//!   --threads N        Thread count (default: available parallelism)
//!   --verify           Verify correctness during benchmark
//!   --transforms       Benchmark transforms (tab_zip, block_span)
//!   --help             Show this help message

use sentinel_engine::*;
use std::env;
use std::time::Instant;

fn print_usage() {
    eprintln!("Usage: sentinel_engine [OPTIONS]");
    eprintln!("  --files N          Number of synthetic files (default: 100000)");
    eprintln!("  --chunk-size N     Scan chunk size in bytes (default: 2097152)");
    eprintln!("  --threads N        Thread count (default: available parallelism)");
    eprintln!("  --verify           Verify correctness during benchmark");
    eprintln!("  --transforms       Benchmark transforms (tab_zip, block_span)");
    eprintln!("  --help             Show this help message");
}

fn parse_args() -> (usize, usize, usize, bool, bool) {
    let args: Vec<String> = env::args().collect();
    let mut files = 100_000usize;
    let mut chunk_size = 2_097_152usize;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut thread_count = threads;
    let mut verify = false;
    let mut transforms = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--help" || arg == "-h" {
            print_usage();
            std::process::exit(0);
        }
        // Handle `--flag=value` and `-f=value` forms.
        if let Some(rest) = arg.strip_prefix("--files=")
            .or_else(|| arg.strip_prefix("-f="))
        {
            files = rest.parse().unwrap_or(files);
        } else if let Some(rest) = arg.strip_prefix("--chunk-size=")
            .or_else(|| arg.strip_prefix("-c="))
        {
            chunk_size = rest.parse().unwrap_or(chunk_size);
        } else if let Some(rest) = arg.strip_prefix("--threads=")
            .or_else(|| arg.strip_prefix("-t="))
        {
            thread_count = rest.parse().unwrap_or(thread_count);
        } else if arg == "--files" || arg == "-f" {
            files = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(files);
            i += 2;
        } else if arg == "--chunk-size" || arg == "-c" {
            chunk_size = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(chunk_size);
            i += 2;
        } else if arg == "--threads" || arg == "-t" {
            thread_count = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(thread_count);
            i += 2;
        } else if arg == "--verify" {
            verify = true;
        } else if arg == "--transforms" {
            transforms = true;
        } else if arg == "--" {
            break;
        } else if !arg.starts_with('-') {
            // ignore positional args
        }
        i += 1;
    }

    (files, chunk_size, thread_count, verify, transforms)
}

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
    let (files, chunk_size, thread_count, verify, transforms) = parse_args();

    println!("=== Sentinel Engine Benchmark ===");
    let corpus = synthetic(files);
    println!(
        "corpus: {:.2} MB, {} files, {} threads, chunk_size {} bytes",
        corpus.len() as f64 / 1_048_576.0,
        files,
        thread_count,
        chunk_size
    );

    let t0 = Instant::now();
    let hits = scan_parallel(&corpus, chunk_size, thread_count);
    let dt = t0.elapsed();
    println!(
        "scan: {} boundaries in {:.3} ms ({:.2} GB/s)",
        hits.len(),
        dt.as_millis(),
        (corpus.len() as f64 / 1e9) / dt.as_secs_f64()
    );

    let macro_count = hits.iter().filter(|h| h.kind == BoundaryKind::MacroFile).count();
    let micro_count = hits.iter().filter(|h| h.kind == BoundaryKind::MicroHunk).count();
    println!(
        "macro={} micro={} (expect macro={} micro={})",
        macro_count, micro_count, files, files * 2
    );

    let records: Vec<_> = ZeroCopyRecordIterator::new(&corpus, hits).collect();
    let bytes: usize = records.iter().map(|r| r.body.len()).sum();
    println!(
        "records={} payload_bytes={} (zero-copy slices)",
        records.len(), bytes
    );

    if transforms {
        println!("--- transform benchmark ---");
        let mut zip_total = 0usize;
        let t_zip = Instant::now();
        for r in &records {
            zip_total += tab_zip(r.body).len();
        }
        let dt_zip = t_zip.elapsed();
        println!(
            "tab_zip: {} bytes in {:.3} ms ({:.2} GB/s)",
            zip_total, dt_zip.as_millis(),
            (zip_total as f64 / 1e9) / dt_zip.as_secs_f64()
        );

        let mut span_total = 0usize;
        let t_span = Instant::now();
        for r in &records {
            span_total += diff_to_block_span(std::str::from_utf8(r.body).unwrap_or("")).len();
        }
        let dt_span = t_span.elapsed();
        println!(
            "block_span: {} bytes in {:.3} ms ({:.2} GB/s)",
            span_total, dt_span.as_millis(),
            (span_total as f64 / 1e9) / dt_span.as_secs_f64()
        );
    }

    if verify || transforms {
        println!("--- correctness verify ---");
        let mut ok = true;
        for r in &records {
            let zipped = tab_zip(r.body);
            if zipped.len() > r.body.len() { ok = false; }
            let _ = diff_to_block_span(std::str::from_utf8(r.body).unwrap_or(""));
        }
        println!("verify: {}", if ok { "PASS" } else { "FAIL" });
    }

    println!("=== benchmark complete ===");
}
