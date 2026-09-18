# Token Optimization Path

## Current State
- §/¶ = 1 token each (verified in cl100k_base/o200k_base)
- Saves 8-21% vs XML by eliminating tag overhead
- Compatibility layer available for LLM attention

## Optimization Path

### Level 1: Basic §/¶ encoding (DONE)
- Replace `<file path="...">` with `§path\n¶`
- Savings: 8-21% vs XML

### Level 2: Path table + IDs (DONE)
```
§paths
0:src/main.rs
1:src/Button.tsx
¶
FILE:0
content...
¶
FILE:1
content...
¶
```
- Savings: 50-60% additional (eliminates path repetition)
- LLM safe: `FILE:0` is unambiguous
- API: `encode_path_table(&[&str])`, `format_file_ref(id)`

### Level 3: Shortest unique prefix
- Truncate `src/components/Button.tsx` → `Button.tsx` if unique
- Requires path uniqueness check

### Level 4: Content deduplication (DONE)
- Store identical content once, reference by hash
- Best for libraries, boilerplate, licenses
- API: `ContentStore`, `hash_anchor()` (FNV-1a 64), `format_hash_anchor()` (12 hex),
  `parse_hash_anchor()`; anchors ride `§#`/`¶#` boundaries

## LLM Safety
- Tested: LLM correctly identifies `§path\n¶` as markers, not code
- Recommendation: Use `FILE:ID` format for unambiguous references
- Avoid bare numbers (risk of hallucination as code)

## Benchmark Results
- 10k files: rs 0.70 GB/s scan, md 4.06 GB/s, xml 4.47 GB/s, html 2.46 GB/s
- All transforms + verify: PASS
- 13/13 tests pass

## Boundary grammar (v2)
- `§` + header — file record; `¶` + header — hunk record
- `§#<hex>` / `¶#<hex>` — hash anchor (up to 12 hex chars, any case)
- `§@` / `¶@` — state marker (stale/dirty)
- `¶+` / `¶-` — block-span sign (from `diff_to_block_span`)
- Scanner: SIMD fast path + scalar reference; anchors verified at column 0 only