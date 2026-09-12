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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::tools::{self, ShellResult};

/// Guest path of the installed Lean binary (first entry in GUEST_PATH).
const LEAN_BIN: &str = "/usr/local/bin/lean";
/// Guest dir holding Lean's shared libs (libInit_shared.so, ...). It is NOT
/// on the loader's default search path: installs run ldconfig over it, and
/// every lean invocation also exports it via LD_LIBRARY_PATH.
const LEAN_LIB: &str = "/usr/local/lib/lean";
/// Where the archive is bound inside the guest during extraction.
const LEAN_ARCHIVE_GUEST: &str = "/tmp/lean.tar.zst";
/// Published alongside the rootfs on `container-latest`.
const MANIFEST_URL: &str =
    "https://github.com/1337farm/forgerig/releases/download/container-latest/container-manifest.json";
/// Single `lean` run cap (typechecks are slow: runtime load + checking).
const LEAN_TIMEOUT: Duration = Duration::from_secs(600);
/// Network retries for the large Lean archive download.
const LEAN_DOWNLOAD_ATTEMPTS: u32 = 3;
/// Unpacking the ~550MB archive (1.5GB unpacked) under proot can be slow.
const LEAN_EXTRACT_TIMEOUT: Duration = Duration::from_secs(900);
/// Hard cap for the install-time `lean --version` probe, kept short so install
/// can never stall. The version banner is derived from the archive name anyway.
const LEAN_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Page-cache preload of the big shared libs (background, post-install).
const LEAN_PRELOAD_TIMEOUT: Duration = Duration::from_secs(150);
/// Cap for the background warm probe (cache-warm run; long enough to finish a
/// genuinely-slow-but-working lean invocation and capture the banner).
const LEAN_WARM_PROBE_TIMEOUT: Duration = Duration::from_secs(300);

/// Live download progress, polled by the UI via `lean_status`.
static LEAN_DOWNLOADING: AtomicBool = AtomicBool::new(false);
static LEAN_DOWNLOADED: AtomicU64 = AtomicU64::new(0);
static LEAN_TOTAL: AtomicU64 = AtomicU64::new(0);
static LEAN_PROVISIONING: AtomicBool = AtomicBool::new(false);
static LEAN_LAST_MESSAGE: Mutex<String> = Mutex::new(String::new());
/// Cursor to the version banner captured at install (or derived from the
/// archive headline). status() does NOT re-run lean to read it.
static LEAN_VERSION: Mutex<Option<String>> = Mutex::new(None);
/// Whether the background warm-up (page-cache preload + warm probe) has been
/// kicked off this process.
static LEAN_WARMED: AtomicBool = AtomicBool::new(false);

/// RAII guard: marks the download active on creation and inactive on drop,
///
/// so every exit path (including `?` early-returns) clears the progress flag.
struct DownloadProgress;
impl DownloadProgress {
    fn start(total: u64) -> Self {
        LEAN_DOWNLOADING.store(true, Ordering::Relaxed);
        LEAN_TOTAL.store(total, Ordering::Relaxed);
        LEAN_DOWNLOADED.store(0, Ordering::Relaxed);
        DownloadProgress
    }
}
impl Drop for DownloadProgress {
    fn drop(&mut self) {
        LEAN_DOWNLOADING.store(false, Ordering::Relaxed);
    }
}

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
    #[serde(default)]
    pub downloading: bool,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub provisioning: bool,
    #[serde(default)]
    pub message: Option<String>,
}

fn snapshot(ready: bool, version: Option<String>) -> LeanStatus {
    LeanStatus {
        ready,
        version,
        downloading: LEAN_DOWNLOADING.load(Ordering::Relaxed),
        downloaded: LEAN_DOWNLOADED.load(Ordering::Relaxed),
        total: LEAN_TOTAL.load(Ordering::Relaxed),
        provisioning: LEAN_PROVISIONING.load(Ordering::Relaxed),
        message: LEAN_LAST_MESSAGE
            .lock()
            .ok()
            .map(|m| m.clone())
            .filter(|m| !m.is_empty()),
    }
}

/// Progress-only status: reads the shared atomics WITHOUT spawning a proot
/// child, so the UI can poll it every second while a download/extract runs.
/// `ready`/`version` are left unset here; the full `status()` sets them (it
/// has to run `lean --version` in the guest, which the progress path avoids).
pub fn progress() -> LeanStatus {
    snapshot(false, None)
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
    download_stream(resp.into_reader(), dest, entry)
}

/// Stream a verified payload into `dest`, updating the shared progress
/// atomics (`DownloadProgress` guard + `LEAN_DOWNLOADED`) as bytes land.
/// Split out of `download_verify` so a unit test can feed an in-memory
/// reader and assert the counters the progress bar reads.
fn download_stream<R: Read>(mut reader: R, dest: &Path, entry: &LeanEntry) -> Result<(), LeanError> {
    let mut file = std::fs::File::create(dest).map_err(|e| LeanError::Download(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    let mut total = 0u64;
    let mut last_logged = 0u64;
    let _progress = DownloadProgress::start(entry.size);
    eprintln!("lean download: started ({} bytes)", entry.size);
    loop {
        let n = reader.read(&mut buf).map_err(|e| LeanError::Download(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| LeanError::Download(e.to_string()))?;
        total += n as u64;
        LEAN_DOWNLOADED.store(total, Ordering::Relaxed);
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
///
/// The work is split into named, timed steps so the install log shows exactly
/// where time goes — previously `lean --version` ran as the tail of one big
/// pipeline and, because it can take minutes under proot, the whole install
/// looked stuck at "extracting" for 30 minutes.
pub async fn extract_in_guest(archive: &Path) -> Result<(), LeanError> {
    let guest = archive
        .to_str()
        .ok_or_else(|| LeanError::Install("archive path is not UTF-8".to_string()))?
        .to_string();
    let bind = format!("{}:{}", guest, LEAN_ARCHIVE_GUEST);

    // ---- Step 1/3: unpack ----
    eprintln!("lean install step 1/3: unpacking archive into /usr/local");
    let t0 = Instant::now();
    let cmd = format!(
        "mkdir -p /usr/local/bin && zstd -d -c {} | tar -x --strip-components=1 -C /usr/local",
        LEAN_ARCHIVE_GUEST
    );
    let r = tools::run_trusted_binds_limited(&cmd, &[bind], LEAN_EXTRACT_TIMEOUT).await;
    if r.timed_out {
        return Err(LeanError::Install(format!(
            "step 1/3 unpack timed out after {:.0}s",
            LEAN_EXTRACT_TIMEOUT.as_secs_f64()
        )));
    }
    if r.exit_code != Some(0) {
        return Err(LeanError::Install(format!(
            "step 1/3 unpack failed (exit {:?}) after {:.1}s\n-- stderr --\n{}",
            r.exit_code,
            t0.elapsed().as_secs_f64(),
            r.stderr.trim()
        )));
    }
    eprintln!("lean install step 1/3: unpacked in {:.1}s", t0.elapsed().as_secs_f64());

    // ---- Step 2/3: ldconfig so the loader finds libInit_shared.so etc. ----
    eprintln!("lean install step 2/3: running ldconfig");
    let t1 = Instant::now();
    let r = tools::run_trusted_limited(
        &format!("ldconfig {} 2>/dev/null || true", LEAN_LIB),
        Duration::from_secs(120),
    )
    .await;
    eprintln!(
        "lean install step 2/3: ldconfig done in {:.1}s (exit {:?})",
        t1.elapsed().as_secs_f64(),
        r.exit_code
    );

    // ---- Step 3/3: bounded `lean --version` probe (named) ----
    // Diagnostic: show what lean will have to load, so a slow probe is
    // explained by the actual shared-lib sizes on disk.
    let r3 = tools::run_trusted_limited(
        &format!("ls -la {LEAN_BIN} 2>/dev/null; ls -l {LEAN_LIB} 2>/dev/null"),
        Duration::from_secs(30),
    )
    .await;
    if !r3.stdout.trim().is_empty() {
        eprintln!("lean install step 3/3: extracted lean tree:\n{}", r3.stdout.trim());
    }
    probe_lean_version().await;
    Ok(())
}

async fn probe_lean_version() -> Option<String> {
    probe_lean_version_limited(LEAN_PROBE_TIMEOUT).await
}

/// Run `lean --version` under a hard cap and log the timing. Lean initializes
/// its runtime by loading hundreds of MB of shared libs through its own ELF
/// loader; under proot's ptrace interception that is slow enough to look like
/// a hang, so this must never run unbounded. The captured version banner is
/// cached for status(). Returns None on timeout/failure.
async fn probe_lean_version_limited(timeout: Duration) -> Option<String> {
    let t = Instant::now();
    let cmd = format!("LD_LIBRARY_PATH={} {} --version", LEAN_LIB, LEAN_BIN);
    let r = tools::run_trusted_limited(&cmd, timeout).await;
    let dur = t.elapsed().as_secs_f64();
    eprintln!(
        "lean probe: lean --version finished in {dur:.1}s (exit {:?}, timed_out={}, cap={:.0}s)",
        r.exit_code, r.timed_out, timeout.as_secs_f64()
    );
    if r.timed_out {
        eprintln!(
            "lean probe: lean --version exceeded {:.0}s — proot overhead loading ~500MB of shared libs is suspected",
            timeout.as_secs_f64()
        );
        return None;
    }
    if !r.stdout.trim().is_empty() || !r.stderr.trim().is_empty() {
        eprintln!(
            "lean probe: lean --version stdout=\"{}\" stderr=\"{}\"",
            r.stdout.trim(),
            r.stderr.trim()
        );
    }
    if r.exit_code == Some(0) {
        let v = format!("{}\n{}", r.stdout, r.stderr);
        let version = v
            .lines()
            .find(|l| l.contains("Lean (version"))
            .map(|l| l.trim().to_string());
        if let Some(v) = version.as_ref() {
            if let Ok(mut cached) = LEAN_VERSION.lock() {
                *cached = Some(v.clone());
            }
        }
        return version;
    }
    None
}

/// Background warm-up, kicked off once per daemon process once lean is present:
/// first populate the page cache with the big shared libs (so the runtime
/// loader's mmaps fault in from RAM instead of cold storage — the likely bulk
/// of the cold 60s+ cost), then run a warm `lean --version` with a generous cap
/// to capture a real banner and measure the cache-warm cost. This tells us on
/// the log whether the bottleneck is storage (warm run is fast) or ptrace.
async fn warm_lean() {
    let t = Instant::now();
    let r = tools::run_trusted_limited(
        "cat /usr/local/lib/lean/*.so* > /dev/null 2>&1 || true",
        LEAN_PRELOAD_TIMEOUT,
    )
    .await;
    eprintln!(
        "lean warm: page-cache preload done in {:.1}s (exit {:?})",
        t.elapsed().as_secs_f64(),
        r.exit_code
    );
    let _ = probe_lean_version_limited(LEAN_WARM_PROBE_TIMEOUT).await;
}

/// Whether the guest has a Lean binary and its cached version banner, if any.
///
/// IMPORTANT: does NOT run `lean --version` here. That probe loads hundreds
/// of MB of shared libs and took >300s (timed out) under proot on device —
/// running it on every status poll made every check hang. Readiness is a cheap
/// `test -x`; the version is whatever the bounded install probe captured.
pub async fn status() -> LeanStatus {
    let check = tools::run_trusted_limited(&format!("test -x {}", LEAN_BIN), Duration::from_secs(30)).await;
    if check.exit_code != Some(0) {
        // test -x returns 1 for missing; keep routine polling quiet.
        return snapshot(false, None);
    }
    // One-time background warm-up: preloads the libs into page cache and
    // captures a warm --version banner + timing (see warm_lean). Non-blocking.
    if !LEAN_WARMED.swap(true, Ordering::Relaxed) {
        tokio::spawn(async move {
            warm_lean().await;
        });
    }
    let version = cached_lean_version();
    snapshot(true, version)
}

/// Download (if needed) + verify + extract the Lean toolchain. Returns a
/// human-readable report; never panics.
pub async fn provision() -> String {
    // Gate on binary presence (status() is a cheap test -x, NOT a lean run):
    // the version banner may be uncached (probe timed out), so keying on it
    // would reinstall a working toolchain every time.
    if status().await.ready {
        let ver = status().await.version.unwrap_or_else(|| "unknown version".to_string());
        println!("lean provision: already installed ({ver})");
        return format!("Lean already installed ({ver})");
    }
    println!("lean provision: starting download + install");
    match provision_inner().await {
        Ok(msg) => {
            // One bounded probe populates the cached banner before this reads it,
            // but tolerate it being uncached if the probe was slow/timed out.
            let ver = LEAN_VERSION.lock().ok().and_then(|v| v.clone()).unwrap_or_else(|| "unknown version".to_string());
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

/// Fire-and-forget provisioning: returns immediately so the UI can poll
/// `lean_status` (downloading/downloaded/total) for a live progress bar
/// while the ~550 MB archive downloads in the background.
pub async fn kick_off_provision() -> String {
    if LEAN_PROVISIONING.swap(true, Ordering::SeqCst) {
        eprintln!("lean provision: button pressed but provisioning already running");
        return "Lean provisioning already running".to_string();
    }
    eprintln!("lean provision: button pressed — starting download + install");
    tokio::spawn(async {
        let msg = provision().await;
        if let Ok(mut last) = LEAN_LAST_MESSAGE.lock() {
            last.clear();
            last.push_str(&msg);
        }
        LEAN_PROVISIONING.store(false, Ordering::SeqCst);
    });
    "Lean provisioning started".to_string()
}

async fn provision_inner() -> Result<String, LeanError> {
    let entry = tokio::task::spawn_blocking(fetch_lean_entry)
        .await
        .map_err(|e| LeanError::Manifest(format!("join: {e}")))??;
    let dest = cache_dir().join(&entry.name);
    if !cached_is_valid(&entry, &dest) {
        eprintln!(
            "lean provision: downloading {} ({} bytes) to {}",
            entry.name,
            entry.size,
            dest.display()
        );
        let mut attempt = 0;
        loop {
            attempt += 1;
            let entry = entry.clone();
            let dest2 = dest.clone();
            let result = tokio::task::spawn_blocking(move || download_verify(&entry, &dest2))
                .await
                .map_err(|e| LeanError::Download(format!("join: {e}")))?;
            match result {
                Ok(()) => break,
                Err(LeanError::Download(message)) if attempt < LEAN_DOWNLOAD_ATTEMPTS => {
                    eprintln!("lean download attempt {attempt} failed: {message}; retrying");
                    tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
    extract_in_guest(&dest).await?;
    // The version is known from the manifest headline without ever running
    // `lean` — running it to print a banner is far too slow under proot.
    if let Some(ver) = lean_version_from_name(&entry.name) {
        if let Ok(mut cached) = LEAN_VERSION.lock() {
            *cached = Some(ver);
        }
    }
    Ok(format!(
        "Lean installed: {} ({} MB from {})",
        entry.name,
        entry.size / (1024 * 1024),
        manifest_url().rsplit('/').next().unwrap_or(entry.name.as_str())
    ))
}

/// Derive the Lean version from the upstream archive headline, e.g.
/// `lean-4.34.0-rc2-linux_aarch64.tar.zst` -> `4.34.0-rc2`. Avoids running
/// `lean --version`, which loads the whole runtime and takes >60s under proot.
fn lean_version_from_name(name: &str) -> Option<String> {
    name.strip_prefix("lean-")
        .and_then(|s| s.strip_suffix("-linux_aarch64.tar.zst"))
        .map(|s| s.to_string())
}

/// Best-effort version without executing lean: prefer the banner cached at
/// install, else read the archive headline in the cache dir. Never blocks on
/// lean itself.
fn cached_lean_version() -> Option<String> {
    if let Ok(v) = LEAN_VERSION.lock() {
        if let Some(ver) = v.clone() {
            return Some(ver);
        }
    }
    // Fall back to the archive filename in the shared cache dir (CONTAINER_CACHE).
    let cache = cache_dir();
    if let Ok(entries) = std::fs::read_dir(&cache) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(ver) = lean_version_from_name(&name) {
                return Some(ver);
            }
        }
    }
    None
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
    let r = tools::run_trusted_limited(&format!("LD_LIBRARY_PATH={} {} {}", LEAN_LIB, LEAN_BIN, tools::sh_quote(file)), LEAN_TIMEOUT).await;
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

    #[test]
    fn download_progress_guard_sets_and_clears_flag() {
        assert!(!LEAN_DOWNLOADING.load(Ordering::Relaxed));
        {
            let _guard = DownloadProgress::start(581751016);
            assert!(LEAN_DOWNLOADING.load(Ordering::Relaxed));
            assert_eq!(LEAN_TOTAL.load(Ordering::Relaxed), 581751016);
            assert_eq!(LEAN_DOWNLOADED.load(Ordering::Relaxed), 0);
            let snap = snapshot(false, None);
            assert!(snap.downloading);
            assert_eq!(snap.total, 581751016);
        }
        assert!(!LEAN_DOWNLOADING.load(Ordering::Relaxed));
    }

    #[test]
    fn download_stream_updates_progress_and_verifies_bytes() {
        let data: Vec<u8> = (0..(256 * 1024)).map(|i| (i % 251) as u8).collect();
        let sha = Sha256::digest(&data)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        let entry = LeanEntry {
            name: "t.tar.zst".to_string(),
            sha256: sha.clone(),
            size: data.len() as u64,
            url: String::new(),
        };
        let dir = std::env::temp_dir().join(format!("lean-progress-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("t.tar.zst");

        // Sanity: a byte-exact stream verifies and the counters end at total.
        download_stream(std::io::Cursor::new(data.clone()), &dest, &entry).unwrap();
        assert_eq!(LEAN_DOWNLOADED.load(Ordering::Relaxed), data.len() as u64);
        assert!(!LEAN_DOWNLOADING.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(&dest).unwrap(), data);

        let snap = snapshot(false, None);
        assert_eq!(snap.downloaded, data.len() as u64);
        assert_eq!(snap.total, data.len() as u64);
        assert!(!snap.downloading);

        // A truncated stream must NOT verify: size mismatch, no success snapshot.
        LEAN_DOWNLOADED.store(0, Ordering::Relaxed);
        let short = &data[..data.len() - 1];
        assert!(download_stream(std::io::Cursor::new(short.to_vec()), &dest, &entry).is_err());
        assert!(!LEAN_DOWNLOADING.load(Ordering::Relaxed));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_derived_from_archive_name_without_running_lean() {
        assert_eq!(
            lean_version_from_name("lean-4.34.0-rc2-linux_aarch64.tar.zst").as_deref(),
            Some("4.34.0-rc2")
        );
        assert_eq!(
            lean_version_from_name("lean-4.33.1-linux_aarch64.tar.zst").as_deref(),
            Some("4.33.1")
        );
        assert_eq!(lean_version_from_name("ubuntu-rootfs.bin"), None);
    }
}