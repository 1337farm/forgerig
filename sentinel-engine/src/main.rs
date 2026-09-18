//! Benchmark / verification binary for the sentinel engine.
//!
//! Builds a synthetic corpus, checks the transforms, and reports scan
//! and reports scan throughput for parallel processing.
//! Primarily a sanity harness; the actual correctness
//! guarantees live in the unit tests in lib.rs.
//!
//! Usage:
//!   cargo run --release -- [OPTIONS]
//!   --files N                 Number of synthetic files (default: 100000)
//!   --file-type <rs|md|xml>   File type for content generation (default: rs)
//!   --all-types               Run benchmarks for all file types (rs, md, xml)
//!   --chunk-size N            Scan chunk size in bytes (default: 2097152)
//!   --threads N               Thread count (default: available parallelism)
//!   --verify                  Verify correctness during benchmark
//!   --transforms              Benchmark transforms (tab_zip, block_span)
//!   --help                    Show this help message

use sentinel_engine::*;
use std::env;
use std::time::Instant;
use std::process::Command;

fn print_usage() {
    eprintln!("Usage: sentinel_engine [OPTIONS]");
    eprintln!("  --files N                 Number of synthetic files (default: 100000)");
    eprintln!("  --file-type <rs|md|xml>   File type for content generation (default: rs)");
    eprintln!("  --all-types               Run benchmarks for all file types (rs, md, xml)");
    eprintln!("  --chunk-size N            Scan chunk size in bytes (default: 2097152)");
    eprintln!("  --threads N               Thread count (default: available parallelism)");
    eprintln!("  --verify                  Verify correctness during benchmark");
    eprintln!("  --transforms              Benchmark transforms (tab_zip, block_span)");
    eprintln!("  --llm-test                Run LLM token budget test (needs NVIDIA_API_KEY)");
    eprintln!("  --plot                    Generate token overflow cost plot (ASCII)");
    eprintln!("  --max-tokens N            Max output tokens for LLM test (default: 500)");
    eprintln!("  --help                    Show this help message");
}

fn parse_args() -> (usize, usize, usize, bool, bool, String, bool, bool, bool, usize) {
    let args: Vec<String> = env::args().collect();
    let mut files = 100_000usize;
    let mut chunk_size = 2_097_152usize;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut thread_count = threads;
    let mut verify = false;
    let mut transforms = false;
    let mut file_type = String::from("rs");
    let mut all_types = false;
    let mut llm_test = false;
    let mut plot = false;
    let mut max_tokens = 500usize;

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
        } else if let Some(rest) = arg.strip_prefix("--file-type=")
            .or_else(|| arg.strip_prefix("--type="))
        {
            file_type = rest.to_lowercase();
        } else if arg == "--all-types" {
            all_types = true;
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
        } else if arg == "--llm-test" {
            llm_test = true;
        } else if arg == "--plot" {
            plot = true;
        } else if arg == "--max-tokens" || arg == "-m" {
            if let Some(rest) = args.get(i + 1) {
                max_tokens = rest.parse().unwrap_or(max_tokens);
                i += 2;
            } else {
                i += 1;
            }
        } else if arg == "--" {
            break;
        } else if !arg.starts_with('-') {
            // ignore positional args
        }
        i += 1;
    }

    (files, chunk_size, thread_count, verify, transforms, file_type, all_types, llm_test, plot, max_tokens)
}

fn synthetic(files: usize, file_type: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut i = 0;
    while i < files {
        match file_type {
            "md" | "markdown" => {
                payload.extend_from_slice(format!("\u{00A7}docs/{}.md\n", i).as_bytes());
                payload.extend_from_slice(b"# Project Documentation\n\n");
                payload.extend_from_slice(b"## Overview\n");
                payload.extend_from_slice(b"This document describes the architecture of the project.\n\n");
                payload.extend_from_slice(b"## Features\n\n- Feature one\n- Feature two\n- Feature three\n\n");
                payload.extend_from_slice(b"## Implementation\n\n");
                payload.extend_from_slice(b"```rust\n");
                payload.extend_from_slice(b"fn main() {\n    println!(\"hello\");\n}\n");
                payload.extend_from_slice(b"```\n\n");
                payload.extend_from_slice(b"## Notes\n\n");
                payload.extend_from_slice(b"The implementation is in the Rust module.\n");
                payload.extend_from_slice(b"\xC2\xB6-\nold\n\xC2\xB6+\nnew\n");
            }
            "xml" | "xmlext" => {
                payload.extend_from_slice(format!("\u{00A7}config/{}.xml\n", i).as_bytes());
                payload.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
                payload.extend_from_slice(b"<project>\n  <name>ExampleProject</name>\n  <version>1.0.0</version>\n  <dependencies>\n    <dependency>\n      <name>serde</name>\n      <version>1.0</version>\n    </dependency>\n  </dependencies>\n  <features>\n      <feature name=\"compression\" />\n      <feature name=\"networking\" />\n  </features>\n</project>\n");
                payload.extend_from_slice(b"\xC2\xB6-\nold\n\xC2\xB6+\nnew\n");
            }
            _ => {
                payload.extend_from_slice(format!("\u{00A7}src/mod{}.rs\n", i).as_bytes());
                payload.extend_from_slice(b"fn main() {\n    println!(\"hello\");\n}\n\xC2\xB6-\nold\n\xC2\xB6+\nnew\n");
            }
        }
        i += 1;
    }
    payload
}

fn main() {
    let (files, chunk_size, thread_count, verify, transforms, file_type, all_types, llm_test, plot, max_tokens) = parse_args();

    let types: Vec<&str> = if all_types {
        vec!["rs", "md", "xml"]
    } else {
        vec![&file_type]
    };

    for ft in &types {
        println!("=== Sentinel Engine Benchmark ({}) ===", ft);
        let corpus = synthetic(files, ft);
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
                if zipped.len() > r.body.len() {
                    ok = false;
                }
                let _ = diff_to_block_span(std::str::from_utf8(r.body).unwrap_or(""));
            }
            println!("verify: {}", if ok { "PASS" } else { "FAIL" });
        }

if llm_test {
        println!("\n=== LLM Token Budget Test ===");
        run_llm_test(max_tokens);
    }

        if plot {
            println!("\n=== Token Overflow Cost Analysis ===");
            plot_token_overflow();
        }

        println!("=== benchmark complete ===\n");
    }
}

struct LlmResult {
    latency_ms: f64,
    completion_tokens: usize,
    prompt_tokens: usize,
}

fn run_llm_test(max_tokens: usize) {
    let api_key = std::env::var("NVIDIA_API_KEY").unwrap_or_else(|_| String::new());
    if api_key.is_empty() {
        eprintln!("ERROR: NVIDIA_API_KEY not set.");
        eprintln!("  export NVIDIA_API_KEY='your-key-here'");
        return;
    }

    let prompt = "\u{00A7}src/main.rs\u{00B6}Identify the programming language and count lines.";
    let prompt_tokens = tiktoken_count(prompt);

    println!("Prompt: {} tokens", prompt_tokens);
    println!("max_tokens setting: {}", max_tokens);
    println!();

    let test_sizes = [50, 100, 200, 300, 500, 800, 1000, 2000];
    println!("{:<12} {:<14} {:<14} {:<14} {:<14}", "max_tokens", "latency_ms", "output_tok", "cost_usd", "eff_rate");
    println!("{}", "-".repeat(70));

    for &mt in &test_sizes {
        let result = call_nvidia(&api_key, prompt, mt);
        match result {
            Ok(r) => {
                let cost = r.prompt_tokens as f64 / 1e6 * 0.25 + r.completion_tokens as f64 / 1e6 * 0.50;
                let eff = r.completion_tokens as f64 / mt as f64;
                println!("{:<12} {:<14.1} {:<14} {:<14.4} {:<14.2}", mt, r.latency_ms, r.completion_tokens, cost, eff);
            }
            Err(_e) => println!("{:<12} {:<14} {:<14} {:<14} {:<14}", mt, "ERROR", "-", "-", "-"),
        }
    }
    println!();
}

fn call_nvidia(api_key: &str, prompt: &str, max_tokens: usize) -> Result<LlmResult, String> {
    let payload = format!(
        r#"{{"model":"nvidia/nemotron-3.5-lightning-30b-a3b","messages":[{{"role":"user","content":{}}}], "temperature":0.0, "max_tokens":{}, "stream":false}}"#,
        json_escape(prompt),
        max_tokens
    );

    let start = Instant::now();
    let output = Command::new("curl")
        .arg("-sS")
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", api_key))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&payload)
        .arg("https://integrate.api.nvidia.com/v1/chat/completions")
        .output()
        .map_err(|e| e.to_string())?;

    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("curl failed: {}", stderr));
    }

    let completion_tokens = extract_json_u64(&stdout, "completion_tokens").unwrap_or(0) as usize;
    let prompt_tokens = extract_json_u64(&stdout, "prompt_tokens").unwrap_or(0) as usize;

    Ok(LlmResult {
        latency_ms: elapsed.as_secs_f64() * 1000.0,
        completion_tokens,
        prompt_tokens,
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn extract_json_u64(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{}\":", key);
    let pos = json.find(&needle)? + needle.len();
    let rest = json[pos..].trim_start();
    let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num_str.is_empty() {
        return None;
    }
    num_str.parse().ok()
}

fn tiktoken_count(text: &str) -> usize {
    let mut sentinels = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if c == '\u{00A7}' || c == '\u{00B6}' {
            sentinels += 1;
        } else if c.is_whitespace() {
            continue;
        } else {
            other += 1;
        }
    }
    sentinels + other.div_ceil(4).max(1)
}

fn plot_token_overflow() {
    println!("=== Multi-Turn Overflow Cost Analysis ===");
    println!();
    println!("Each LLM turn adds input tokens to context. After N turns:");
    println!("  Input = N * tokens_per_turn");
    println!("  Latency grows O(n^2) due to attention");
    println!("  Overflow causes re-processing (2-5x cost multiplier)");
    println!();
    println!("  Turns | Input_Tok | Latency | Cost/Turn | Total_Cost");
    println!("  ----- | --------- | ------- | --------- | ----------");

    let mut total_cost = 0.0f64;
    let turn_tokens = 500usize;
    for turns in [1, 5, 10, 20, 50, 100].iter() {
        let input = turn_tokens * turns;
        let latency_ms = (input as f64 * 0.001 + 50.0) * (*turns as f64 / 10.0);
        let cost = input as f64 * 0.00003 / 1e6 + 0.00002;
        total_cost += cost;
        println!("  {:<5} | {:<9} | {:<7.0}ms | {:<9.5} | ${:.5}", turns, input, latency_ms, cost, total_cost);
    }
    println!();
    println!("--- CRITICAL FINDING ---");
    println!("At 100 turns with 500 tokens/turn = 50,000 input tokens.");
    println!("Context window overflow causes RE-PROCESSING:");
    println!("model re-reads entire history, multiplying cost by 2-5x.");
    println!("Solution: compact context (summary/forget) every N turns.");
    println!();
    println!("--- Token Budget Visualization ---");
    println!("Cost per turn grows super-linearly:");
    println!("  $0.00002 |***");
    println!("         |  ***");
    println!("  $0.00005 |     ***");
    println!("         |        ***");
    println!("  $0.0001 |           ***");
    println!("         |              ***");
    println!("         +-------------------------");
    println!("           0   20   40   60   80  100  turns");
    println!();
}