use serde::{Deserialize, Serialize};
use rig::tool::Tool;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use thiserror::Error;
use serde_json::json;

/// CPU-seconds cap for sandboxed (untrusted) commands.
const SANDBOX_CPU_SEC: u64 = 120;
/// Virtual-memory cap (KiB) for sandboxed commands (512 MiB).
const SANDBOX_MEM_KIB: u64 = 524_288;
/// Wall-clock cap for sandboxed commands.
const SANDBOX_TIMEOUT: Duration = Duration::from_secs(120);
/// Wall-clock cap for trusted (`!`) commands.
const TRUSTED_TIMEOUT: Duration = Duration::from_secs(600);

const SANDBOX_WORKDIR: &str = "/root/workspace";

/// `PATH` for commands run inside the proot guest: the daemon's own env is the
/// Android/Termux one, whose paths don't exist in the guest, so the guest would
/// otherwise fail to find `/usr/bin/ls` etc.
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Error, Debug)]
pub enum BashExecutorError {
    #[error("Failed to execute command: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BashExecutorArgs {
    pub command: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ShellResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Quote a value for POSIX sh (single quotes), safe for arbitrary paths/args.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Resolve the host scratch dir proot needs (PROOT_TMP_DIR), creating it.
///
/// Loud on failure: a missing/unwritable scratch dir surfaces downstream as
/// cryptic proot "can't chmod ... proot-tmp" + execve failures deep inside
/// installs, so log exactly what went wrong instead of silently falling back
/// to proot's built-in default (a Termux $PREFIX/tmp path that exists nowhere
/// on a device).
fn ensure_proot_tmp_dir() -> Option<String> {
    let cache = std::env::var("CONTAINER_CACHE").ok().filter(|s| !s.is_empty());
    let cache = match cache {
        Some(c) => c,
        None => {
            eprintln!("proot tmp: CONTAINER_CACHE unset, PROOT_TMP_DIR not set (proot falls back to its built-in default)");
            return None;
        }
    };
    let tmp = format!("{cache}/proot-tmp");
    match std::fs::create_dir_all(&tmp) {
        Ok(()) => Some(tmp),
        Err(e) => {
            eprintln!("proot tmp: cannot create {tmp}: {e} (guest commands are likely to fail; check app storage)");
            None
        }
    }
}

/// Cheap guest-shell readiness check through the same proot wrapping installs
/// use. Lets callers fail fast with an actionable message (incomplete rootfs
/// / broken proot scratch) instead of burning a ~550 MB download and then
/// dumping raw proot errors.
pub async fn guest_shell_ready() -> Result<(), String> {
    let r = run_trusted_limited("true", Duration::from_secs(30)).await;
    if r.timed_out {
        return Err("guest shell probe timed out after 30s".to_string());
    }
    if r.exit_code == Some(0) {
        return Ok(());
    }
    // Truncate: proot dumps multi-line help text on failure.
    let first: Vec<&str> = r.stderr.lines().take(4).collect();
    let detail = if first.is_empty() {
        format!("exit {:?}", r.exit_code)
    } else {
        format!("exit {:?}: {}", r.exit_code, first.join(" | "))
    };
    Err(detail)
}

#[derive(Clone, Debug)]
pub struct BashExecutor {
    /// Guest directory the command starts in. `Default` is the shared
    /// workspace (trusted/tests callers); the model-facing path binds one
    /// session's own directory via `for_session`.
    workdir: String,
}

impl Default for BashExecutor {
    fn default() -> Self {
        Self { workdir: SANDBOX_WORKDIR.to_string() }
    }
}

impl BashExecutor {
    /// Bind this tool to one session's isolated workspace.
    pub fn for_session(session_id: &str) -> Self {
        Self { workdir: crate::gatekeeper::session_workspace(session_id) }
    }
}

/// Build the host `Command` that runs `command` in the work guest.
///
/// When `CONTAINER_PROOT` + `CONTAINER_ROOTFS` are set (the daemon running
/// host-side), the command is wrapped in proot against the work rootfs;
/// otherwise it runs directly via `sh` (the daemon itself living in-guest, or
/// local dev). `sandbox` prepends resource limits and a disposable workdir so
/// model-generated commands stay contained. `binds` adds extra `-b host:guest`
/// proot bindings (e.g. the Lean archive the bootstrap stages).
/// The `sh -c` line carried into the guest.
///
/// Sandbox mode is the security boundary for model-generated commands, so the
/// resource limits and the workdir live here — a pure function so they can be
/// asserted without spawning a container. `workdir` is the jail the command
/// starts in: the shared workspace for trusted daemon-internal callers, or a
/// session's own directory for model-facing tools.
fn sandbox_line(command: &str, sandbox: bool, workdir: &str) -> String {
    if sandbox {
        format!(
            "ulimit -t {} -v {} 2>/dev/null; mkdir -p {} 2>/dev/null; cd {} 2>/dev/null || true; {}",
            SANDBOX_CPU_SEC, SANDBOX_MEM_KIB, workdir, workdir, command
        )
    } else {
        command.to_string()
    }
}

fn build_cmd_binds(command: &str, sandbox: bool, binds: &[String], workdir: &str) -> Command {
    let line = sandbox_line(command, sandbox, workdir);

    let proot = std::env::var("CONTAINER_PROOT").ok();
    let rootfs = std::env::var("CONTAINER_ROOTFS").ok();
    if let (Some(proot), Some(rootfs)) = (proot, rootfs) {
        // Preserve LD_LIBRARY_PATH before env_clear() — proot needs it to find
        // libtalloc.so.2 and libandroid-shmem.so (proot itself is dynamically linked).
        let saved_ld_library_path = std::env::var("LD_LIBRARY_PATH").ok().filter(|s| !s.is_empty());
        let mut cmd = Command::new(&proot);
        cmd.kill_on_drop(true);
        // proot itself is dynamically linked (DT_NEEDED libtalloc.so.2 +
        // libandroid-shmem.so with a Termux RUNPATH that does not exist on
        // device). The daemon inherits the app's LD_LIBRARY_PATH; we must
        // preserve it across env_clear().
        if let Some(ld) = std::env::var("LD_LIBRARY_PATH").ok().filter(|s| !s.is_empty()) {
            cmd.env("LD_LIBRARY_PATH", ld);
        }
        cmd
            // Never leak host secrets into the guest: the daemon inherits
            // FORGERIG_* provider keys, but guest shells only get an
            // allowlisted env. Brokered network (net_fetch) attaches keys
            // host-side instead.
            .env_clear()
            // Re-apply LD_LIBRARY_PATH after env_clear() so proot can find
            // its DT_NEEDED libs (libtalloc.so.2, libandroid-shmem.so).
            .env("LD_LIBRARY_PATH", std::env::var("LD_LIBRARY_PATH").unwrap_or_default())
            .arg("-r")
            .arg(&rootfs)
            .arg("-0")
            .arg("-w")
            .arg("/root")
            .env("PATH", GUEST_PATH)
            .env("HOME", "/root");
        // proot's own scratch (glue rootfs, f2fs probe) lives on the HOST, so
        // guest /tmp is irrelevant to it — and the Termux-built proot falls
        // back to a compiled-in $PREFIX/tmp default that exists nowhere on a
        // device. Point PROOT_TMP_DIR at a real dir in the app's cache so the
        // warnings disappear and binds of large archives always have scratch.
        // TMPDIR=/tmp is for tools running INSIDE the guest (Ubuntu has /tmp).
        // Failures are logged loudly by the helper (a missing scratch dir
        // breaks every guest exec with a cryptic proot dump).
        if let Some(tmp) = ensure_proot_tmp_dir() {
            cmd.env("PROOT_TMP_DIR", &tmp);
        }
        cmd.env("TMPDIR", "/tmp");
        // Bind the host-generated resolv.conf so guest tools (apt/git/gh) can
        // resolve. Generated by the app and pointed at via env.
        if let Ok(resolv) = std::env::var("CONTAINER_RESOLV_CONF") {
            cmd.arg("-b").arg(format!("{}:/etc/resolv.conf", resolv));
        }
        for bind in binds {
            cmd.arg("-b").arg(bind);
        }
        cmd.arg("/bin/sh").arg("-c").arg(&line);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        // Local-dev fallback has no proot boundary: strip secrets explicitly.
        for v in crate::gatekeeper::SECRET_ENV_VARS {
            cmd.env_remove(v);
        }
        cmd.kill_on_drop(true).arg("-c").arg(&line);
        cmd
    }
}

async fn run_shell(command: &str, sandbox: bool, limit: Duration) -> ShellResult {
    run_shell_binds(command, sandbox, limit, &[]).await
}

async fn run_shell_binds(command: &str, sandbox: bool, limit: Duration, binds: &[String]) -> ShellResult {
    run_shell_binds_in(SANDBOX_WORKDIR, command, sandbox, limit, binds).await
}

/// As `run_shell_binds`, but starting in `workdir` instead of the shared
/// workspace. Model-facing tools pass their session's own directory so two
/// concurrent sessions cannot see or clobber each other's files.
async fn run_shell_binds_in(workdir: &str, command: &str, sandbox: bool, limit: Duration, binds: &[String]) -> ShellResult {
    let fut = async {
        let output = build_cmd_binds(command, sandbox, binds, workdir).output().await?;
        Ok::<_, std::io::Error>(output)
    };
    match timeout(limit, fut).await {
        Ok(Ok(output)) => {
            // Fail-closed on size: truncate unbounded tool output so a noisy
            // `lean` stderr or `cat` can never OOM the daemon / WebView.
            let (stdout, _) = crate::gatekeeper::truncate_output(&String::from_utf8_lossy(&output.stdout));
            let (stderr, _) = crate::gatekeeper::truncate_output(&String::from_utf8_lossy(&output.stderr));
            // Then strip credentials we KNOW are secret (the guest gitconfig
            // OAuth token). `cat /root/.gitconfig` would otherwise hand a live
            // GitHub token to a cloud model. Redacting the known literal — not
            // pattern-guessing — keeps file paths and sha256 digests intact.
            ShellResult {
                stdout: crate::gatekeeper::redact_known(&stdout),
                stderr: crate::gatekeeper::redact_known(&stderr),
                exit_code: output.status.code(),
                timed_out: false,
            }
        }
        Ok(Err(e)) => ShellResult {
            stdout: String::new(),
            stderr: format!("failed to spawn: {e}"),
            exit_code: None,
            timed_out: false,
        },
        Err(_) => ShellResult {
            stdout: String::new(),
            stderr: format!("command timed out after {}s", limit.as_secs()),
            exit_code: None,
            timed_out: true,
        },
    }
}

/// Model-facing, sandboxed shell (resource limits + disposable workdir).
/// Fail-closed: oversized or exfil-shaped commands are rejected before proot.
pub async fn run_sandboxed(command: &str) -> ShellResult {
    run_sandboxed_in(SANDBOX_WORKDIR, command).await
}

/// As `run_sandboxed`, but starting in `workdir`. This is what binds a model
/// session to its own files: the cwd decides which session's workspace a
/// relative path resolves against.
pub async fn run_sandboxed_in(workdir: &str, command: &str) -> ShellResult {
    if let Err(reason) = crate::gatekeeper::validate_command(command) {
        crate::gatekeeper::log_verdict("bash_executor", false, &reason, command);
        return ShellResult {
            stdout: String::new(),
            stderr: format!("blocked by gatekeeper: {reason}"),
            exit_code: None,
            timed_out: false,
        };
    }
    run_shell_binds_in(workdir, command, true, SANDBOX_TIMEOUT, &[]).await
}

/// Trusted shell for user `!` commands (no limits, longer timeout).
pub async fn run_trusted(command: &str) -> ShellResult {
    run_shell(command, false, TRUSTED_TIMEOUT).await
}

/// Trusted shell with an explicit wall-clock limit (payload installs like Lean).
pub async fn run_trusted_limited(command: &str, limit: Duration) -> ShellResult {
    run_shell(command, false, limit).await
}

/// Trusted shell with extra proot `-b` bindings and an explicit timeout.
pub async fn run_trusted_binds_limited(command: &str, binds: &[String], limit: Duration) -> ShellResult {
    run_shell_binds(command, false, limit, binds).await
}

/// Best-effort creation of one team member's isolated guest workspace.
/// Called on session create/spawn so the directory exists before any tool
/// runs there. Never fails the caller: a missing dir surfaces as an ordinary
/// tool error later, and the gatekeeper still jails every path.
/// Skipped when the daemon runs without a proot guest (local dev / tests):
/// there the "guest" paths don't exist and mkdir would pollute the host.
pub async fn ensure_session_workspace(session_id: &str) {
    if std::env::var("CONTAINER_ROOTFS").ok().filter(|v| !v.is_empty()).is_none() {
        return;
    }
    let dir = crate::gatekeeper::session_workspace(session_id);
    // The dir is gatekeeper-derived (sanitized), but quote anyway.
    let r = run_trusted_limited(
        &format!("mkdir -p {}", sh_quote(&dir)),
        Duration::from_secs(30),
    )
    .await;
    if r.exit_code != Some(0) {
        eprintln!("ensure_session_workspace: mkdir {} exit {:?}", dir, r.exit_code);
    }
}

/// Lean typecheck runner: jailed to the workspace like sandboxed shells, but
/// without the 512 MiB `ulimit -v` cap (Lean loads ~500 MB of shared libs and
/// would die under it). Wall-clock still bounded; secrets never enter the guest.
pub async fn run_lean_jailed(command: &str, limit: Duration) -> ShellResult {
    run_lean_jailed_in(SANDBOX_WORKDIR, command, limit).await
}

/// As `run_lean_jailed`, but starting in `workdir` (a session's own workspace
/// for model-facing typechecks).
pub async fn run_lean_jailed_in(workdir: &str, command: &str, limit: Duration) -> ShellResult {
    let line = format!(
        "mkdir -p {} 2>/dev/null; cd {} 2>/dev/null || true; {}",
        workdir, workdir, command
    );
    run_shell_binds_in(workdir, &line, false, limit, &[]).await
}

impl Tool for BashExecutor {
    const NAME: &'static str = "bash_executor";

    type Error = BashExecutorError;
    type Args = BashExecutorArgs;
    type Output = ShellResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Execute a shell command (POSIX sh) in the Linux work container and return stdout, stderr, and exit code. Commands run in a disposable /root/workspace with CPU, memory, and time limits.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                }
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(run_sandboxed_in(&self.workdir, &args.command).await)
    }
}

#[derive(Error, Debug)]
pub enum CodeIngestError {
    #[error("ingest failed: {0}")]
    Failed(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CodeIngestArgs {
    pub workspace_path: String,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_true")]
    pub use_path_table: bool,
    #[serde(default = "default_true")]
    pub use_dedup: bool,
}

fn default_max_files() -> usize {
    200
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CodeIngestResult {
    pub framed: String,
    pub files: usize,
    pub bytes: usize,
    pub deduped: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct CodeIngest {
    /// The jail `workspace_path` is validated against. `Default` is the shared
    /// workspace; a session-bound instance rejects sibling sessions' dirs.
    workdir: String,
}

impl Default for CodeIngest {
    fn default() -> Self {
        Self { workdir: SANDBOX_WORKDIR.to_string() }
    }
}

impl CodeIngest {
    /// Bind this tool to one session's isolated workspace.
    pub fn for_session(session_id: &str) -> Self {
        Self { workdir: crate::gatekeeper::session_workspace(session_id) }
    }
}

impl Tool for CodeIngest {
    const NAME: &'static str = "code_ingest";

    type Error = CodeIngestError;
    type Args = CodeIngestArgs;
    type Output = CodeIngestResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Ingest a workspace directory into sentinel-framed (§/¶) code context with path-table IDs and content-hash dedup. Returns framed text to paste into the prompt.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "workspace_path": {
                        "type": "string",
                        "description": "Absolute path of the workspace directory to ingest"
                    },
                    "max_files": {
                        "type": "number",
                        "description": "Max files to include (default 200)"
                    },
                    "use_path_table": {
                        "type": "boolean",
                        "description": "Emit §paths table + FILE:id refs (default true)"
                    },
                    "use_dedup": {
                        "type": "boolean",
                        "description": "Replace repeated bodies with §# hash anchors (default true)"
                    }
                },
                "required": ["workspace_path"]
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Jail the ingest root to THIS session's workspace. The daemon runs
        // HOST-side, so an unjailed workspace_path would let the model frame up
        // any host-readable directory (including the app's own filesDir, which
        // holds the container rootfs and its gitconfig with an OAuth token) —
        // and against the shared root it could read every OTHER session's
        // files. Per-file symlink checks inside ingest_workspace are
        // meaningless when the root itself is free.
        let requested = if args.workspace_path.trim().is_empty() {
            self.workdir.clone()
        } else {
            args.workspace_path.clone()
        };
        let safe = match crate::gatekeeper::validate_guest_path_in(&self.workdir, &requested) {
            Ok(p) => p,
            Err(reason) => {
                crate::gatekeeper::log_verdict(Self::NAME, false, &reason, &requested);
                return Err(CodeIngestError::Failed(format!("blocked by gatekeeper: {reason}")));
            }
        };
        let opts = crate::ingest::IngestOptions {
            max_files: args.max_files.max(1).min(2000),
            use_path_table: args.use_path_table,
            use_dedup: args.use_dedup,
            ..Default::default()
        };
        // ingest_workspace reads the filesystem directly, so it needs the HOST
        // path the validated guest path maps to.
        let host = crate::gatekeeper::guest_to_host(&safe).to_string_lossy().into_owned();
        crate::ingest::ingest_workspace(&host, &opts)
            .map(|o| CodeIngestResult {
                framed: o.framed,
                files: o.files,
                bytes: o.bytes,
                deduped: o.deduped,
                truncated: o.truncated,
            })
            .map_err(|e| {
                crate::gatekeeper::log_verdict(Self::NAME, false, &e, &safe);
                CodeIngestError::Failed(e)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatekeeper::SECRET_ENV_VARS;

    /// `sh_quote` builds the `sh -c` line that carries a model-supplied path
    /// into the guest, so a quoting slip here is shell injection. Proved by
    /// round-tripping each payload through a real `sh`: the guest must see the
    /// bytes verbatim as ONE argument, with no second command running.
    #[tokio::test]
    async fn sh_quote_survives_shell_metacharacters() {
        for payload in [
            "plain.lean",
            "it's.lean",
            "a b; rm -rf /",
            "x\nrm -rf /",
            "$HOME `id` ${PATH}",
            "\"; touch /tmp/pwned; echo \"",
            "a'$(id)'b",
            "*",
            "--flag=$(whoami)",
        ] {
            // `printf %s` emits the argument with no interpretation, so stdout
            // must equal the payload exactly. A quoting bug shows up either as
            // mangled bytes or as output from the injected command.
            let line = sandbox_line(&format!("printf %s {}", sh_quote(payload)), false, SANDBOX_WORKDIR);
            let out = run_trusted(&line).await;
            assert_eq!(out.stdout, payload, "quoting broke for {payload:?}");
            assert!(!out.stderr.contains("pwned"), "{payload:?} escaped: {}", out.stderr);
        }
    }

    #[test]
    fn sh_quote_matches_posix_escape() {
        assert_eq!(sh_quote("plain.lean"), "'plain.lean'");
        // The canonical POSIX escape: close, escaped quote, reopen.
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        // Single quotes defer expansion.
        assert!(sh_quote("$HOME `id` ${PATH}").starts_with('\''));
    }

    /// The sandbox wrapper must constrain CPU/memory and land in the disposable
    /// workdir.
    #[test]
    fn sandbox_line_limits_resources_and_enters_workspace() {
        let line = sandbox_line("ls -la", true, SANDBOX_WORKDIR);
        assert!(line.contains(&format!("ulimit -t {}", SANDBOX_CPU_SEC)), "{line}");
        assert!(line.contains(&format!("-v {}", SANDBOX_MEM_KIB)), "{line}");
        assert!(line.contains(&format!("cd {}", SANDBOX_WORKDIR)), "{line}");
        assert!(line.trim_end().ends_with("ls -la"), "{line}");
    }

    /// A session-bound sandbox must start in that session's directory, not the
    /// shared root — the cwd is what isolates two sessions' relative paths.
    #[test]
    fn sandbox_line_honours_a_session_workdir() {
        let ws = crate::gatekeeper::session_workspace("sess-42");
        let line = sandbox_line("ls -la", true, &ws);
        assert!(line.contains(&format!("mkdir -p {ws}")), "{line}");
        assert!(line.contains(&format!("cd {ws}")), "{line}");
        assert!(!line.contains(&format!("cd {SANDBOX_WORKDIR} ")), "{line}");
    }

    #[test]
    fn trusted_line_has_no_limits_or_workdir() {
        // Trusted (human `!`) commands run unrestricted in /root.
        assert_eq!(sandbox_line("lean --version", false, SANDBOX_WORKDIR), "lean --version");
    }

    /// A session-bound tool must carry that session's directory, while an
    /// unbound one keeps the shared root for trusted/tests callers.
    #[test]
    fn session_bound_tools_use_the_session_workspace() {
        let ws = crate::gatekeeper::session_workspace("sess-1");
        assert_eq!(BashExecutor::for_session("sess-1").workdir, ws);
        assert_eq!(CodeIngest::for_session("sess-1").workdir, ws);
        assert_eq!(BashExecutor::default().workdir, SANDBOX_WORKDIR);
        assert_eq!(CodeIngest::default().workdir, SANDBOX_WORKDIR);
        // Two different sessions never collide.
        assert_ne!(BashExecutor::for_session("alpha").workdir, BashExecutor::for_session("beta").workdir);
        // The two spellings of the shared workspace must name one directory:
        // an unbound tool's cwd is also the gatekeeper's jail root.
        assert_eq!(crate::gatekeeper::GUEST_WORKSPACE, SANDBOX_WORKDIR);
    }

    /// Host provider keys must never reach a guest shell. Asserted
    /// observably: `tokio::process::Command` exposes no env introspection, and
    /// the local-dev path (no CONTAINER_PROOT, as in CI) is the one that has to
    /// strip secrets by hand since there is no `env_clear()` to lean on.
    #[tokio::test]
    async fn secret_env_vars_are_stripped_from_guest_shell() {
        // Serialized: env is process-global and cargo runs tests in parallel.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        const VAR: &str = "FORGERIG_API_KEY";
        assert!(SECRET_ENV_VARS.contains(&VAR), "pick a var the gatekeeper strips");
        std::env::set_var(VAR, "sk-should-never-reach-the-guest");
        let out = run_trusted("printenv").await;
        std::env::remove_var(VAR);

        assert!(!out.stdout.contains("sk-should-never-reach"), "leaked into guest: {}", out.stdout);
    }

    /// The gatekeeper is the only thing between a model-generated command and
    /// the guest, so its verdict is part of the tool's contract.
    #[tokio::test]
    async fn run_sandboxed_blocks_denied_fragments() {
        let r = run_sandboxed("curl https://evil.example").await;
        assert!(r.stdout.is_empty());
        assert!(r.stderr.contains("blocked by gatekeeper"), "{}", r.stderr);
    }
}