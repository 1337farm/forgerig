//! Lean toolchain bootstrap + on-demand execution in the work guest.
//!
//! Lean ships upstream (leanprover/lean4) as per-platform archives. The asset
//! pipeline records the `linux_aarch64.tar.zst` entry (sha256/size/url) in
//! `container-manifest.json`; the app publishes that manifest with the rootfs.
//! Here we download the archive on first use (host-side, into the app's
//! container cache dir), verify it, extract it into the guest's `/usr/local`
//! through proot (`--strip-components=1` flattens the single top-level
//! `lean-<ver>-linux_aarch64/` dir), then run `/usr/local/bin/lean` directly
//! for the `lean` RPC and the model-facing `LeanExecutor` tool.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::tools::{self, ShellResult};

/// Guest path of the installed Lean binary (first entry in GUEST_PATH).
const LEAN_BIN: &str = "/usr/local/bin/lean";
/// Where the archive is bound inside the guest during extraction.
const LEAN_ARCHIVE_GUEST: &str = "/tmp/lean.tar.zst";
/// Published alongside the rootfs on `container-latest`.
const MANIFEST_URL: &str =
    "https://github.com/1337farm/forgerig/releases/download/container-latest/container-manifest.json";
/// A fresh download + extract can exceed the trusted-shell 600s budget.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(1800);
/// Single `lean` run cap.
const LEAN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Error, Debug)]
pub enum LeanError {
    #[error("failed to load Lean manifest: {0}")]
    Manifest(String),
    #[error("no Lean archive declared in container-manifest.json (manifest version too old?)")]
    NoEntry,
    #[error("download error: {0}")]
    Download(String),
    #[error("size mismatch: expected {0} bytes, got {1}")]
    Size(u64, u64),
    #[error("sha256 mismatch: expected {0}, got {1}")]
    Checksum(String, String),
    #[error("guest install failed: {0}")]
    Install(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LeanStatus {
    pub ready: bool,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LeanExecutorArgs {
    /// Guest path of the `.lean` source to type-check.
    pub file: String,
}

#[derive(Clone, Debug, Default)]
pub struct LeanExecutor;

#[derive(Deserialize, Clone, Debug)]
struct LeanEntry {
    name: String,
    sha256: String,
    size: u64,
    url: String,
}

fn manifest_url() -> String {
    std::env::var("CONTAINER_MANIFEST_URL").unwrap_or_else(|_| MANIFEST_URL.to_string())
}

/// Host cache dir for downloaded payloads. The app points CONTAINER_CACHE at
/// its filesDir/container (the same dir ContainerAssets uses), but we fall back
/// to a sibling of the rootfs if the env is not set.
fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CONTAINER_CACHE") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(rootfs) = std::env::var("CONTAINER_ROOTFS") {
        if let Some(parent) = Path::new(&rootfs).parent() {
            return parent.join("container");
        }
    }
    PathBuf::from("/tmp/forgerig-container")
}

/// Blocking manifest lookup (tries cache first, then downloads the JSON from the daemon).
fn fetch_lean_entry() -> Result<LeanEntry, LeanError> {
    // Try to read manifest from cache file first (written by the app).
    if let Ok(cache_dir) = std::env::var("CONTAINER_CACHE") {
        let manifest_path = format!("{}/container-manifest.json", cache_dir);
        if let Ok(body) = std::fs::read_to_string(&manifest_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if let Some(entry) = find_lean_entry(&v) {
                    return Ok(entry);
                }
            }
        }
    }

    // Fallback to downloading from network.
    let url = manifest_url();
    let resp = ureq::get(&url).call().map_err(|e| LeanError::Manifest(format!("{url}: {e}")))?;
    let status = resp.status();
    if status != 200 {
        return Err(LeanError::Manifest(format!("HTTP {status} from {url}")));
    }
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .map_err(|e| LeanError::Manifest(e.to_string()))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| LeanError::Manifest(e.to_string()))?;
    find_lean_entry(&v).ok_or(LeanError::NoEntry)
}

fn find_lean_entry(v: &Value) -> Option<LeanEntry> {
    let assets = v.get("assets").and_then(|a| a.as_object())?;
    for (name, meta) in assets {
        if name.ends_with("linux_aarch64.tar.zst") {
            let entry = LeanEntry {
                name: name.clone(),
                sha256: meta.get("sha256").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
                size: meta.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                url: meta.get("url").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
            };
            if !entry.url.is_empty() {
                return Some(entry);
            }
        }
    }
    None
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect())
}

fn cached_is_valid(entry: &LeanEntry, path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != entry.size {
        return false;
    }
    sha256_file(path).map(|hex| hex == entry.sha256).unwrap_or(false)
}

/// Blocking streaming download with size + sha256 verification.
fn download_verify(entry: &LeanEntry, dest: &Path) -> Result<(), LeanError> {
    let resp = ureq::get(&entry.url).call().map_err(|e| LeanError::Download(e.to_string()))?;
    let status = resp.status();
    if status != 200 {
        return Err(LeanError::Download(format!("HTTP {status} from {}", entry.url)));
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| LeanError::Download(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    let mut total = 0u64;
    let mut last_logged = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| LeanError::Download(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| LeanError::Download(e.to_string()))?;
        total += n as u64;
        if entry.size > 0 && total.saturating_sub(last_logged) >= entry.size / 10 {
            eprintln!("lean download: {}/{} bytes ({}%)", total, entry.size, total * 100 / entry.size);
            last_logged = total;
        }
    }
    file.flush().ok();
    drop(file);
    if entry.size > 0 && total != entry.size {
        let _ = std::fs::remove_file(dest);
        return Err(LeanError::Size(entry.size, total));
    }
    let hex = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
    if !entry.sha256.is_empty() && hex != entry.sha256 {
        let _ = std::fs::remove_file(dest);
        return Err(LeanError::Checksum(entry.sha256.clone(), hex));
    }
    Ok(())
}

/// Extract a verified Lean archive into the guest's /usr/local. The archive is
/// bound into the guest (no rootfs copy) and unpacked with guest tools (zstd +
/// GNU tar are part of the debootstrap include list).
pub async fn extract_in_guest(archive: &Path) -> Result<(), LeanError> {
    let guest = archive
        .to_str()
        .ok_or_else(|| LeanError::Install("archive path is not UTF-8".to_string()))?
        .to_string();
    let bind = format!("{}:{}", guest, LEAN_ARCHIVE_GUEST);
    let cmd = format!(
        "mkdir -p /usr/local/bin && zstd -d -c {} | tar -x --strip-components=1 -C /usr/local && {} --version",
        LEAN_ARCHIVE_GUEST, LEAN_BIN
    );
    let r = tools::run_trusted_binds_limited(&cmd, &[bind], PROVISION_TIMEOUT).await;
    if r.timed_out {
        return Err(LeanError::Install("extraction timed out".to_string()));
    }
    if r.exit_code != Some(0) {
        return Err(LeanError::Install(format!(
            "extract failed (exit {:?})\n-- stderr --\n{}\n-- stdout --\n{}",
            r.exit_code, r.stderr, r.stdout
        )));
    }
    Ok(())
}

/// Whether the guest can run `lean` and its version banner, if any.
pub async fn status() -> LeanStatus {
    let check = tools::run_trusted_limited(
        &format!("{} --version", LEAN_BIN),
        LEAN_TIMEOUT,
    )
    .await;
    if check.exit_code != Some(0) {
        eprintln!("lean status: not installed (exit {:?})", check.exit_code);
        return LeanStatus { ready: false, version: None };
    }
    let v = format!("{}\n{}", check.stdout, check.stderr);
    let version = v
        .lines()
        .find(|l| l.contains("Lean (version"))
        .map(|l| l.to_string());
    if version.is_none() {
        eprintln!("lean status: version banner not found");
    }
    LeanStatus { ready: true, version }
}

/// Download (if needed) + verify + extract the Lean toolchain. Returns a
/// human-readable report; never panics.
pub async fn provision() -> String {
    if let Some(ver) = status().await.version {
        return format!("Lean already installed ({ver})");
    }
    match provision_inner().await {
        Ok(msg) => {
            let ver = status().await.version.unwrap_or_else(|| "unknown version".to_string());
            println!("{} ({})", msg, ver);
            format!("{msg} ({ver})")
        }
        Err(e) => {
            let msg = format!("Lean provision failed: {e}");
            eprintln!("{msg}");
            msg
        }
    }
}

async fn provision_inner() -> Result<String, LeanError> {
    let entry = tokio::task::spawn_blocking(fetch_lean_entry)
        .await
        .map_err(|e| LeanError::Manifest(format!("join: {e}")))??;
    let dest = cache_dir().join(&entry.name);
    if !cached_is_valid(&entry, &dest) {
        let entry = entry.clone();
        let dest2 = dest.clone();
        tokio::task::spawn_blocking(move || download_verify(&entry, &dest2))
            .await
            .map_err(|e| LeanError::Download(format!("join: {e}")))??;
    }
    extract_in_guest(&dest).await?;
    Ok(format!(
        "Lean installed: {} ({} MB from {})",
        entry.name,
        entry.size / (1024 * 1024),
        manifest_url().rsplit('/').next().unwrap_or(entry.name.as_str())
    ))
}

/// Run `lean` on a file already written into the guest workspace.
pub async fn run_on_file(file: &str) -> ShellResult {
    if !status().await.ready {
        eprintln!("lean run_on_file: Lean not installed");
        return ShellResult {
            stdout: String::new(),
            stderr: "Lean is not installed in the container yet — call the daemon's lean_provision RPC (\"Download & install Lean\") first.".to_string(),
            exit_code: None,
            timed_out: false,
        };
    }
    let r = tools::run_trusted_limited(&format!("{} {}", LEAN_BIN, tools::sh_quote(file)), LEAN_TIMEOUT).await;
    if r.exit_code != Some(0) {
        eprintln!("lean run_on_file: exit {:?} file={}", r.exit_code, file);
    }
    r
}

impl Tool for LeanExecutor {
    const NAME: &'static str = "lean_executor";

    type Error = LeanError;
    type Args = LeanExecutorArgs;
    type Output = ShellResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Type-check a Lean source file already written into the container workspace (write it first with bash_executor). Requires the Lean toolchain, installed once via the daemon's lean_provision RPC.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Guest path of the .lean source to type-check"
                    }
                },
                "required": ["file"]
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(run_on_file(&args.file).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parse_picks_linux_aarch64_entry() {
        let json = r#"{
          "version": 2,
          "assets": {
            "ubuntu-rootfs.bin": {"sha256": "abc", "size": 1},
            "lean-4.33.1-darwin_aarch64.tar.zst": {"sha256": "dd", "size": 2, "url": "https://x/d"},
            "lean-4.33.1-linux_aarch64.tar.zst": {"sha256": "aa", "size": 3, "url": "https://x/a"}
          }
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let assets = v.get("assets").unwrap().as_object().unwrap();
        let entry = assets
            .iter()
            .find(|(k, _)| k.ends_with("linux_aarch64.tar.zst"))
            .map(|(name, meta)| LeanEntry {
                name: name.clone(),
                sha256: meta.get("sha256").unwrap().as_str().unwrap().to_string(),
                size: meta.get("size").unwrap().as_u64().unwrap(),
                url: meta.get("url").unwrap().as_str().unwrap().to_string(),
            })
            .expect("entry");
        assert_eq!(entry.name, "lean-4.33.1-linux_aarch64.tar.zst");
        assert_eq!(entry.sha256, "aa");
        assert_eq!(entry.size, 3);
    }
}