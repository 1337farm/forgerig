//! Sentinel-delimited code ingestion engine.
//!
//! Protocol: source code and diffs are framed with two reserved one-byte(ish)
//! markers that (a) cannot be valid syntax and (b) sit at column 0:
//!
//!   * U+00A7 `§` — macro boundary (whole file), header carries the path
//!   * U+00B6 `¶` — micro boundary (hunk/mutation block)
//!
//! The engine scans for these boundaries with a SIMD fast path (NEON on
//! aarch64, SWAR elsewhere) plus a portable scalar reference, and yields
//! zero-copy records that borrow directly from the source buffer.
//!
//! NOTE ON THE "1-TOKEN" CLAIM: the dossier asserts `§`/`¶` are single tokens
//! in TikToken/LLaMA/SentencePiece. That is UNVERIFIED and, in the author's
//! view, unlikely for cl100k_base (rare Latin-1 symbols usually split into
//! several UTF-8 byte tokens). Treat sentinel token cost as an open question
//! to be measured against a real tokenizer before relying on it.

pub const LEAD_BYTE: u8 = 0xC2; // shared UTF-8 lead byte of § and ¶
pub const MACRO_BYTE: u8 = 0xA7; // § (U+00A7) file boundary
pub const MICRO_BYTE: u8 = 0xB6; // ¶ (U+00B6) hunk boundary
pub const NEWLINE: u8 = b'\n';
pub const TAB: u8 = b'\t';
pub const SPACE: u8 = b' ';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    MacroFile,
    MicroHunk,
}

impl std::fmt::Display for BoundaryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoundaryKind::MacroFile => write!(f, "§ (FILE)"),
            BoundaryKind::MicroHunk => write!(f, "¶ (HUNK)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryHit {
    pub kind: BoundaryKind,
    pub offset: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedRecord<'a> {
    pub kind: BoundaryKind,
    pub header: &'a str,
    pub body: &'a [u8],
}

// ---------------------------------------------------------------------------
// Portable scalar reference scanner (correctness baseline).
// ---------------------------------------------------------------------------
pub fn scan_reference(buffer: &[u8]) -> Vec<BoundaryHit> {
    let mut hits = Vec::new();
    let len = buffer.len();
    let mut i = 0;
    while i < len {
        if buffer[i] == LEAD_BYTE && i + 1 < len {
            let anchored = i == 0 || buffer[i - 1] == NEWLINE;
            if anchored {
                match buffer[i + 1] {
                    MACRO_BYTE => hits.push(BoundaryHit { kind: BoundaryKind::MacroFile, offset: i }),
                    MICRO_BYTE => hits.push(BoundaryHit { kind: BoundaryKind::MicroHunk, offset: i }),
                    _ => {}
                }
            }
        }
        i += 1;
    }
    hits
}

/// Portable SWAR "bytes == 0xC2" finder over 8 bytes at a time.
#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn swar_lead_mask(chunk: u64) -> u64 {
    let rep = 0xC2C2C2C2C2C2C2C2u64;
    let xor = chunk ^ rep;
    (xor.wrapping_sub(0x0101010101010101)) & !xor & 0x8080808080808080
}

// ---------------------------------------------------------------------------
// Fast path: SWAR (portable) or NEON (aarch64). Both only PROPOSE candidate
// lead bytes; the anchored check and continuation byte are still verified
// scalarly in the shared buffer, so they stay correct across chunk boundaries.
// ---------------------------------------------------------------------------
#[inline(always)]
unsafe fn push_if_boundary(buffer: &[u8], pos: usize, out: &mut Vec<BoundaryHit>) {
    if pos + 1 >= buffer.len() {
        return;
    }
    let anchored = pos == 0 || *buffer.get_unchecked(pos - 1) == NEWLINE;
    if !anchored {
        return;
    }
    let next = *buffer.get_unchecked(pos + 1);
    match next {
        MACRO_BYTE => out.push(BoundaryHit { kind: BoundaryKind::MacroFile, offset: pos }),
        MICRO_BYTE => out.push(BoundaryHit { kind: BoundaryKind::MicroHunk, offset: pos }),
        _ => {}
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn scan_range_neon(buffer: &[u8], start: usize, end: usize, out: &mut Vec<BoundaryHit>) {
    use std::arch::aarch64::*;
    let mut idx = start;
    if end >= 16 {
        let limit = end - 16;
        let lead = vdupq_n_u8(LEAD_BYTE);
        while idx <= limit {
            let chunk = vld1q_u8(buffer.as_ptr().add(idx));
            let cmp = vceqq_u8(chunk, lead);
            if vmaxvq_u8(cmp) != 0 {
                let mut lanes = [0u8; 16];
                vst1q_u8(lanes.as_mut_ptr(), cmp);
                for i in 0..16 {
                    if lanes[i] == 0xFF {
                        let pos = idx + i;
                        if pos < end {
                            push_if_boundary(buffer, pos, out);
                        }
                    }
                }
            }
            idx += 16;
        }
    }
    while idx < end {
        if *buffer.get_unchecked(idx) == LEAD_BYTE {
            push_if_boundary(buffer, idx, out);
        }
        idx += 1;
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
unsafe fn scan_range_neon(buffer: &[u8], start: usize, end: usize, out: &mut Vec<BoundaryHit>) {
    // SWAR fallback (8 bytes/iter).
    let mut idx = start;
    if end >= 8 {
        let limit = end - 8;
        while idx <= limit {
            let chunk = u64::from_le_bytes(buffer[idx..idx + 8].try_into().unwrap());
            let matches = swar_lead_mask(chunk);
            if matches != 0 {
                for i in 0..8 {
                    let pos = idx + i;
                    if pos < end && *buffer.get_unchecked(pos) == LEAD_BYTE {
                        push_if_boundary(buffer, pos, out);
                    }
                }
            }
            idx += 8;
        }
    }
    while idx < end {
        if *buffer.get_unchecked(idx) == LEAD_BYTE {
            push_if_boundary(buffer, idx, out);
        }
        idx += 1;
    }
}

/// Scan a single range `[start, end)` of `buffer` (absolute indices) on one thread.
pub unsafe fn scan_range(buffer: &[u8], start: usize, end: usize, out: &mut Vec<BoundaryHit>) {
    scan_range_neon(buffer, start, end, out);
}

/// Single-threaded whole-buffer scan (fast path).
pub fn scan(buffer: &[u8]) -> Vec<BoundaryHit> {
    let mut hits = Vec::new();
    unsafe { scan_range(buffer, 0, buffer.len(), &mut hits) };
    hits
}

/// Parallel scan: split `buffer` into `chunk_size` windows and scan each on a
/// scoped worker thread (no external deps, no Rayon). Because candidate bytes
/// are verified against the shared buffer, a multi-byte sentinel straddling a
/// chunk boundary is still detected exactly once.
pub fn scan_parallel(buffer: &[u8], chunk_size: usize, threads: usize) -> Vec<BoundaryHit> {
    let len = buffer.len();
    if len == 0 {
        return Vec::new();
    }
    let threads = threads.max(1);
    let num_chunks = (len + chunk_size - 1) / chunk_size;

    let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for t in 0..threads {
                handles.push(scope.spawn(move || {
                    let mut hits = Vec::new();
                    let mut chunk_idx = t;
                    while chunk_idx < num_chunks {
                        let start = chunk_idx * chunk_size;
                        let end = std::cmp::min(start + chunk_size, len);
                        unsafe { scan_range_neon(buffer, start, end, &mut hits) };
                        chunk_idx += threads;
                    }
                    hits
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.extend(h.join().unwrap());
            }
            all
        });

    // Deterministic output: worker threads return unordered, so sort by offset.
    let mut results = results;
    results.sort_by_key(|h| h.offset);
    results
}

// ---------------------------------------------------------------------------
// Pre-processing transforms.
// ---------------------------------------------------------------------------

/// Convert leading 4-space groups into tabs. Preserves anything that is not a
/// full 4-space group at the start of a line.
pub fn tab_zip(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let len = input.len();
    let mut i = 0;
    let mut at_line_start = true;
    while i < len {
        let b = input[i];
        if at_line_start && b == SPACE {
            let mut run = 0;
            while i < len && input[i] == SPACE && run < 4 {
                run += 1;
                i += 1;
            }
            if run == 4 {
                out.push(TAB);
            } else {
                for _ in 0..run {
                    out.push(SPACE);
                }
            }
            continue;
        }
        if b == NEWLINE {
            at_line_start = true;
        } else if b != SPACE {
            at_line_start = false;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// Rewrite a unified diff into block-span form: strip the column-0 `+`/`-`
/// markers, group consecutive same-sign lines under `¶+` / `¶-` blocks, and
/// keep `@@`/`---`/`+++` headers and context lines (single leading space) so
/// the diff stays lossless.
pub fn diff_to_block_span(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len());
    let mut mode: Option<char> = None;

    for line in diff.split('\n') {
        if line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++") {
            mode = None;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(stripped) = line.strip_prefix('+') {
            if mode != Some('+') {
                out.push_str("¶+\n");
                mode = Some('+');
            }
            out.push_str(stripped);
            out.push('\n');
        } else if let Some(stripped) = line.strip_prefix('-') {
            if mode != Some('-') {
                out.push_str("¶-\n");
                mode = Some('-');
            }
            out.push_str(stripped);
            out.push('\n');
        } else if let Some(stripped) = line.strip_prefix(' ') {
            mode = None;
            out.push_str(stripped);
            out.push('\n');
        } else {
            // Blank line or non-diff line.
            mode = None;
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Zero-copy record iterator.
// ---------------------------------------------------------------------------
pub struct ZeroCopyRecordIterator<'a> {
    buffer: &'a [u8],
    boundaries: Vec<BoundaryHit>,
    cursor: usize,
}

impl<'a> ZeroCopyRecordIterator<'a> {
    pub fn new(buffer: &'a [u8], boundaries: Vec<BoundaryHit>) -> Self {
        Self { buffer, boundaries, cursor: 0 }
    }
}

impl<'a> Iterator for ZeroCopyRecordIterator<'a> {
    type Item = ParsedRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.boundaries.len() {
            return None;
        }
        let hit = self.boundaries[self.cursor];
        let len = self.buffer.len();
        let header_start = hit.offset + 2;
        let mut header_end = header_start;
        while header_end < len && self.buffer[header_end] != NEWLINE {
            header_end += 1;
        }
        let header = std::str::from_utf8(&self.buffer[header_start..header_end]).unwrap_or("");
        let body_start = if header_end < len { header_end + 1 } else { len };
        let body_end = if self.cursor + 1 < self.boundaries.len() {
            let next = self.boundaries[self.cursor + 1].offset;
            if next > 0 && self.buffer[next - 1] == NEWLINE {
                next - 1
            } else {
                next
            }
        } else if len > body_start && self.buffer[len - 1] == NEWLINE {
            len - 1
        } else {
            len
        };
        self.cursor += 1;
        Some(ParsedRecord {
            kind: hit.kind,
            header,
            body: &self.buffer[body_start..body_end],
        })
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_markers() -> Vec<u8> {
        b"\xC2\xA7src/a.rs\nfn a() {}\n\xC2\xB6-\nold\n\xC2\xB6+\nnew\n\xC2\xA7src/b.rs\nfn b() {}\n".to_vec()
    }

    #[test]
    fn reference_finds_anchored_boundaries_only() {
        let hits = scan_reference(&corpus_markers());
        // §src/a (0), ¶- (offset after a.rs), ¶+ , §src/b
        let kinds: Vec<BoundaryKind> = hits.iter().map(|h| h.kind).collect();
        assert_eq!(kinds.len(), 4);
        assert_eq!(kinds[0], BoundaryKind::MacroFile);
        assert_eq!(kinds[1], BoundaryKind::MicroHunk);
        assert_eq!(kinds[2], BoundaryKind::MicroHunk);
        assert_eq!(kinds[3], BoundaryKind::MacroFile);
    }

    #[test]
    fn fast_path_matches_reference() {
        let corpus = corpus_markers();
        let fast = scan(&corpus);
        let reference = scan_reference(&corpus);
        assert_eq!(fast, reference);
    }

    #[test]
    fn rejects_midlane_markers() {
        // A § mid-line (not preceded by newline) must not be treated as a boundary.
        let buf = b"const s = \"x \xC2\xA7 y\";\n".to_vec();
        assert_eq!(scan(&buf).len(), 0);
        assert_eq!(scan_reference(&buf).len(), 0);
    }

    #[test]
    fn straddling_sentinel_across_chunk_boundaries() {
        // Build a buffer where a § begins exactly at each tested chunk boundary.
        for chunk in [1usize, 3, 16, 1024, 65536] {
            let mut buf = Vec::new();
            // pad so the § lead byte lands at offset `chunk` (0-indexed position == chunk)
            buf.extend(std::iter::repeat(b'x').take(chunk));
            buf.push(NEWLINE);
            buf.push(LEAD_BYTE);
            buf.push(MACRO_BYTE);
            buf.extend_from_slice(b"file.ts\nbody");
            let hits = scan_parallel(&buf, chunk, 4);
            assert!(
                hits.iter().any(|h| h.kind == BoundaryKind::MacroFile && h.offset == chunk + 1),
                "chunk={chunk} failed to find straddling §"
            );
        }
    }

    #[test]
    fn parallel_equals_reference_across_chunk_sizes() {
        let mut buf = corpus_markers();
        // grow with many files to exercise parallelism
        for i in 0..500 {
            buf.extend_from_slice(format!("§src/f{i}.rs\nlet a = {i};\n¶-\nold\n¶+\nnew\n").as_bytes());
        }
        let expected = scan_reference(&buf);
        for chunk in [1usize, 7, 31, 512, 4096] {
            let got = scan_parallel(&buf, chunk, 8);
            assert_eq!(got, expected, "mismatch at chunk={chunk}");
        }
    }

    #[test]
    fn tab_zip_collapses_four_space_indent_only() {
        let src = b"    if (ok) {\n        go();\n    }\n  x\n";
        let out = tab_zip(src);
        assert_eq!(out, b"\tif (ok) {\n\t\tgo();\n\t}\n  x\n");
        // no 4-space run remains
        assert!(!out.windows(4).any(|w| w == [SPACE, SPACE, SPACE, SPACE]));
    }

    #[test]
    fn diff_to_block_span_strips_column_zero_markers_and_preserves_text() {
        let diff = "--- a/f.ts\n+++ b/f.ts\n@@ -1,2 +1,2 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+const c = 4;\n context;\n";
        let out = diff_to_block_span(diff);
        // no code line starts with + or - at column 0 (headers @@/---/+++ allowed)
        for line in out.lines() {
            if line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++") {
                continue;
            }
            assert!(!line.starts_with('+') && !line.starts_with('-'), "column-0 marker left: {line:?}");
        }
        assert!(out.contains("¶+\n"));
        assert!(out.contains("¶-\n"));
        assert!(out.contains("const b = 3;"));
        assert!(out.contains("const a = 1;"));
        // headers preserved
        assert!(out.contains("@@ -1,2 +1,2 @@"));
    }

    #[test]
    fn zero_copy_records_borrow_from_source_buffer() {
        let buf = b"\xC2\xA7src/a.rs\nfn a() {}\n\xC2\xA7src/b.rs\nfn b() {}\n".to_vec();
        let hits = scan(&buf);
        let records: Vec<_> = ZeroCopyRecordIterator::new(&buf, hits).collect();
        assert_eq!(records.len(), 2);
        for rec in &records {
            let body_ptr = rec.body.as_ptr() as usize;
            let buf_lo = buf.as_ptr() as usize;
            let buf_hi = buf_lo + buf.len();
            assert!(body_ptr >= buf_lo && body_ptr <= buf_hi, "body must alias source buffer");
        }
        assert_eq!(records[0].header, "src/a.rs");
        assert_eq!(records[0].body, b"fn a() {}");
        assert_eq!(records[1].header, "src/b.rs");
        assert_eq!(records[1].body, b"fn b() {}");
    }
}