# ForgeRig

| Component | Core Technology | Primary Responsibility |
|---|---|---|
| Native Host & Container | Kotlin, PRoot (ARM64), Ubuntu Base Core rootfs | Isolated Linux userland running on unrooted Android devices with zero-drop foreground service lifecycle. |
| Agent Orchestration Daemon | Rust (tokio, rig-core, static MUSL aarch64) | Type-safe LLM tool routing, process execution, WebSocket API, and autonomous execution loop. |
| Context & Task Tracking | Markdown / JSONL / LMDB | Visual, human-steerable Kanban board and surgical CLI context extraction. |
| Dynamic Memory Engine | llama.cpp (1B–2B GGUF) or cheap cloud LLM pass | Heuristic evaluation of agent progress, context pruning, and automated macro-milestone retrospectives. |
| Workspace Frontend | HTML5 / TypeScript / CSS (WebView) | Decoupled client communicating strictly over localhost WebSockets for cross-platform portability. |

## Phase 1: Native Android Host & Container Runtime
### Native Package Assets & Automated Unpacking
 * Bundle the pre-compiled proot ARM64 binary and the minimal ubuntu-rootfs.tar.gz archive directly within the Android application package assets (assets/ or res/raw/).
 * Implement the native Kotlin AssetExtractor utilizing GZIPInputStream wrapped with Apache Commons TarArchiveInputStream.
 * Extract the root filesystem directly into the app's internal sandboxed directory (context.filesDir.absolutePath + "/ubuntu_rootfs") on the initial "Install Now" tap.
 * Set executable permissions (outputFile.setExecutable(true, false)) on container binaries during stream inflation.

### Container Lifecycle & Connection Persistence
 * Wrap the PRoot container process inside an Android Foreground Service equipped with PARTIAL_WAKE_LOCK and explicit Wi-Fi locks to eliminate OS background thread suspension.
 * Configure the embedded Android WebView client to prevent background sleep cycles, ensuring persistent WebSocket communication between the UI and local daemons.
 * Implement native GitHub OAuth orchestration using custom URI callbacks (forgerig://oauth-callback), automatically exchanging the auth code for a token and injecting it into /root/.gitconfig.

## Phase 2: Custom Rust Agent Orchestration Daemon
### Core Architecture & Provider Integration (Completed Steps)
 * Initialized an in-house static Rust binary project (`daemon`) configured for deployment inside the container.
 * Integrated `rig-core` (using `rig::tool::Tool`) to manage unified API completions and model orchestration.
 * Implemented an asynchronous JSON-RPC / WebSocket transport over `tokio` binding to the injected `PORT` environment variable to serve the frontend client.

### Tool Execution Engine (Completed Steps)
 * Derived tools using typed Rust structs (`rig::tool::Tool`) for compile-time schema validation and zero-cost serialization.
 * Host CLI Dispatcher: Created a `BashExecutor` tool that directly invokes native Linux utilities via `tokio::process::Command` inside the environment, returning structured `stdout`/`stderr`.
 * *Pending:* Embed `wasmi` or `wasmtime` inside the Rust daemon with strict fuel consumption and memory limits to execute ad-hoc, untrusted agent data-transformation scripts.

## Phase 3: Dynamic Macro-Memory & Milestone Distillation
### Dynamic Heuristic Evaluator
 * Reject rigid turn counters; deploy a lightweight 1B to 2B model (e.g., Llama-3.2-1B or Qwen-2.5-1.5B via llama.cpp ARM NEON builds) or a cheap cloud LLM pass.
 * Stream ongoing agent execution traces to this evaluator, which acts as a semantic gatekeeper to detect architectural shifts, resolved blockers, or errors.
 * Prune noisy terminal retries and compilation noise from the primary context window once evaluated.

### Milestone Retrospectives
 * Trigger an automated summarization pass whenever a task hits a designated milestone on the work board.
 * Condense session context into macro-level architectural decisions and system invariants, appending them to a persistent AGENTS.md file or a local LanceDB columnar store.
 * Enforce surgical retrieval during active development: require the primary agent to query ripgrep or indexed macro-memory files rather than stuffing entire raw files into context.

## Phase 4: Hybrid Kanban Steering Board & CLI Pipeline
### Board State Engine
 * Model tasks, blockers, in-review states, and completions as structured Markdown files with YAML frontmatter stored in a .kanban/ directory or an append-only LMDB metadata store.
 * Provide lock-free, zero-copy concurrent reads for both the web interface and background CLI processes.

### Steering & Autonomous Execution
 * Autonomous Worker Mode: The Rust daemon polls the board, picks up pending tasks, creates isolated copy-on-write workspace branches, and begins execution.
 * Human-in-the-Loop Steering: Expose visual injection points in the frontend UI allowing developers to re-order cards, pause active runs, edit constraints, or inject prompt steers directly into the agent's active execution loop.
 * Status Transitions: The agent automatically updates task metadata from In Progress to In Review upon passing localized test scripts inside PRoot.

## Phase 5: Client-Server Decoupling & Cross-Platform Roadmap
### Phase Breakdown & Milestones
| Phase | Milestone Target | Core Deliverables |
|---|---|---|
| Phase 1 | Container & Asset Bootstrap | Kotlin asset extractor, PRoot rootfs unpacker, Foreground Service wake-lock implementation, OAuth credential injector. |
| Phase 2 | Native Rust Orchestrator | rig-core loop, CLI process executor (ripgrep/git), WebSocket server, and structured tool handling. |
| Phase 3 | Kanban & Workspace UI | Visual drag-and-drop board, terminal output stream, and manual human-steering injection panel. |
| Phase 4 | Macro-Memory Engine | Local llama.cpp daemon, heuristic evaluation gatekeeper, and automated AGENTS.md milestone retros. |
| Phase 5 | Desktop & Cross-Platform | Decouple UI via Tauri for desktop (macOS/Linux/Windows) and establish remote-workspace protocol for iOS. |

### Platform Portability Strategy
 * Maintain a strict boundary: the frontend client communicates exclusively through localhost WebSockets, utilizing relative paths without Android-specific API bridges.
 * Deploy the identical web interface across Desktop via Tauri or Electron by launching the Rust daemon as a native background process.
 * Support server/cloud hosting via Docker containers for remote browser access.
 * For iOS deployment, run the interface as a thin client connecting over secure WebSockets/SSH to a remote daemon, avoiding Apple App Store restrictions against local user-space container emulation.

## Screens
### Screen 1: Launch & Setup
 * Logo & Monogram: Replaced the interlocking "OC" neon loop with a three-node geometric neon OSS glyph.
 * Branding & Labels: Primary text displays ForgeRig with the standalone descriptor; all legacy branding and secondary sub-labels removed.
 * Action Element: Retains the single-tap container extraction trigger mapped directly to the local PRoot unpacker.

### Screen 2: Environment Boot
 * Internal Service Names: Container initialization and daemon logs reference oss-daemon and oss-core rather than forgerig-server.
 * Zero-OAuth Flow: Bypasses third-party auth gates entirely, streaming unpacking progress directly until the local WebSocket endpoint signals readiness.

### Screen 3: Unified Workspace & Agent Interface
 * Header Re-architecture: Completely removed the user profile avatar, name label, and external "ONLINE" status indicator. The top bar now strictly reports local engine telemetry (ForgeRig header paired with Daemon: aarch64-android).
 * Shell & Tooling Namespaces: Shell prompt updated to oss@localhost:~$, and native execution calls operate through the oss-agent / oss-daemon binary interface.
 * Telemetry HUD: The bottom-right floating overlay tracks local system load, memory usage, and autonomous agent status without external network dependencies.
