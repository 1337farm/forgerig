use serde::{Deserialize, Serialize};

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", "build", ".gradle", "__pycache__"];
const INCLUDE_EXT: &[&str] = &[
    "rs", "md", "toml", "kt", "kts", "js", "ts", "tsx", "py", "lean", "txt", "json",
    "html", "css", "c", "h", "cpp", "sh", "yaml", "yml",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestOutput {
    pub framed: String,
    pub files: usize,
    pub bytes: usize,
    pub deduped: usize,
    pub truncated: bool,
}

pub struct IngestOptions {
    pub max_files: usize,
    pub max_bytes_per_file: usize,
    pub use_path_table: bool,
    pub use_dedup: bool,
    pub tab_zip: bool,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self { max_files: 200, max_bytes_per_file: 200_000, use_path_table: true, use_dedup: true, tab_zip: true }
    }
}

fn is_text_sample(buf: &[u8]) -> bool {
    let n = buf.len().min(8000);
    !buf[..n].contains(&0)
}

fn collect_files(root: &std::path::Path, max_files: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            if out.len() >= max_files {
                return out;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let ftype = entry.file_type().ok();
            if ftype.map(|t| t.is_dir()).unwrap_or(false) {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if ftype.map(|t| t.is_file()).unwrap_or(true) {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if INCLUDE_EXT.contains(&ext.as_str()) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out.truncate(max_files);
    out
}

pub fn ingest_workspace(root: &str, opts: &IngestOptions) -> Result<IngestOutput, String> {
    let root_path = std::path::Path::new(root);
    if !root_path.is_dir() {
        return Err(format!("not a directory: {root}"));
    }
    let files = collect_files(root_path, opts.max_files);
    let mut store = sentinel_engine::ContentStore::new();
    let mut bodies: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
    let mut truncated = files.len() >= opts.max_files;
    for path in &files {
        let rel = path.strip_prefix(root_path).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let mut data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if data.len() > opts.max_bytes_per_file {
            data.truncate(opts.max_bytes_per_file);
            truncated = true;
        }
        if !is_text_sample(&data) {
            continue;
        }
        let body = if opts.tab_zip { sentinel_engine::tab_zip(&data) } else { data };
        bodies.push((rel, body));
    }
    let mut out = Vec::new();
    let mut deduped = 0usize;
    let mut bytes = 0usize;
    if opts.use_path_table {
        let paths: Vec<&str> = bodies.iter().map(|(p, _)| p.as_str()).collect();
        out.extend_from_slice(&sentinel_engine::encode_path_table(&paths));
        for (id, (path, body)) in bodies.iter().enumerate() {
            if opts.use_dedup {
                let h = sentinel_engine::hash_anchor(body);
                if store.get(h).is_some() {
                    deduped += 1;
                    out.extend_from_slice(sentinel_engine::format_file_ref(id).as_bytes());
                    out.extend_from_slice(b"\n");
                    out.extend_from_slice(format!("§#{}\n", sentinel_engine::format_hash_anchor(h)).as_bytes());
                    continue;
                }
                store.insert(body);
            }
            out.extend_from_slice(sentinel_engine::format_file_ref(id).as_bytes());
            out.extend_from_slice(b"\n");
            let _ = path;
            out.extend_from_slice(body);
            if !body.ends_with(b"\n") {
                out.push(b'\n');
            }
            bytes += body.len();
        }
    } else {
        for (path, body) in bodies.iter() {
            if opts.use_dedup {
                let h = sentinel_engine::hash_anchor(body);
                if store.get(h).is_some() {
                    deduped += 1;
                    out.extend_from_slice(format!("§#{}\n", sentinel_engine::format_hash_anchor(h)).as_bytes());
                    continue;
                }
                store.insert(body);
            }
            out.push(0xC2);
            out.push(0xA7);
            out.extend_from_slice(path.as_bytes());
            out.push(b'\n');
            out.extend_from_slice(body);
            if !body.ends_with(b"\n") {
                out.push(b'\n');
            }
            bytes += body.len();
        }
    }
    Ok(IngestOutput {
        framed: String::from_utf8_lossy(&out).into_owned(),
        files: bodies.len(),
        bytes,
        deduped,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_output_round_trips_through_scanner() {
        let dir = std::env::temp_dir().join("forgerig-ingest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("b.md"), "# hi\n").unwrap();
        let opts = IngestOptions { use_path_table: false, use_dedup: false, ..Default::default() };
        let out = ingest_workspace(dir.to_str().unwrap(), &opts).unwrap();
        assert_eq!(out.files, 2);
        let hits = sentinel_engine::scan(out.framed.as_bytes());
        assert_eq!(hits.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedup_emits_hash_anchor_for_repeat_body() {
        let dir = std::env::temp_dir().join("forgerig-ingest-dedup-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "same\n").unwrap();
        std::fs::write(dir.join("b.rs"), "same\n").unwrap();
        let opts = IngestOptions { use_path_table: false, use_dedup: true, ..Default::default() };
        let out = ingest_workspace(dir.to_str().unwrap(), &opts).unwrap();
        assert_eq!(out.deduped, 1);
        assert!(out.framed.contains("§#"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
