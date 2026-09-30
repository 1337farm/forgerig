use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use futures_util::{StreamExt, SinkExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use memory::MemoryEngine;
use rig::tool::Tool;

mod tools;
mod wasm;
mod memory;
mod provider;
mod lean;
mod sessions;
mod ingest;
mod gatekeeper;
mod net_fetch;

#[derive(Serialize, Deserialize, Debug)]
struct RpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcResponse {
    jsonrpc: String,
    result: Option<Value>,
    error: Option<RpcError>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RpcError {
    code: i32,
    message: String,
}

/// Push handle for server-initiated JSON-RPC notifications (chat_chunk,
/// chat_tool, chat_phase, chat_done, chat_stopped, chat_error) on the same
/// WebSocket as the request.
type WsPush = Arc<
    tokio::sync::Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
>;

async fn push_notification(push: &WsPush, value: Value) {
    let text = serde_json::to_string(&value).unwrap_or_default();
    if text.is_empty() {
        return;
    }
    let mut sink = push.lock().await;
    let _ = sink
        .send(tokio_tungstenite::tungstenite::Message::Text(text))
        .await;
}

// ── Authentication ───────────────────────────────────────────────────────────
//
// The daemon binds 127.0.0.1:$PORT with an ephemeral per-launch port, but a
// loopback port is trivially discoverable by any other app on the device, and
// the daemon's env carries the decrypted provider keys (see
// ContainerService.kt). The `exec` RPC in particular runs `run_trusted` —
// unsandboxed, 600s, and deliberately NOT behind the command denylist, which
// guards only the model-facing path. So an unauthenticated socket is remote
// code execution as the app's UID plus secret exfiltration.
//
// The app mints a random token per daemon launch (FORGERIG_AUTH_TOKEN) and
// hands it to the UI over the JS bridge. Nothing but `auth` runs before it.
//
// Fail-closed: with no token configured, EVERY method is refused, `auth`
// included — there is nothing to compare against, and a daemon that silently
// ran wide open because someone forgot an env var is the exact failure this
// guards against. Local dev exports its own token.

/// JSON-RPC error code for "you have not authenticated".
const AUTH_REQUIRED: i32 = -32001;

/// Compare two secrets without an early exit on the first differing byte.
/// Length is compared normally: a length mismatch is not secret-dependent, and
/// padding to equal length first would make a 32-byte guess cost 32 bytes of
/// work per attempt.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Per-connection auth state: `true` once this connection presented the right
/// token.
///
/// Per CONNECTION, never per process. One authenticated connection must not
/// vouch for another — a shared flag would mean the first caller to guess or
/// steal the token authenticates every subsequent connection for the daemon's
/// lifetime, which is the same hole with extra steps.
type ConnAuth = Arc<std::sync::atomic::AtomicBool>;

fn conn_auth() -> ConnAuth {
    Arc::new(std::sync::atomic::AtomicBool::new(false))
}

/// Result of the pre-dispatch auth gate.
#[derive(Debug)]
enum AuthGate {
    /// Call may proceed.
    Pass,
    /// `auth` itself was called with the right token: mark the connection and
    /// answer `{"authenticated": true}`.
    Accepted,
    /// Refuse with this code and message.
    Denied(i32, String),
}

/// The auth gate, as a pure function of (configured token, per-connection
/// state, incoming method, offered token) so every branch is testable without
/// a socket, an env var, or a task.
fn auth_gate(configured: Option<&str>, conn: &std::sync::atomic::AtomicBool, method: &str, offered: Option<&str>) -> AuthGate {
    use std::sync::atomic::Ordering;
    // Fail closed when no token is configured. Deliberately first, so it wins
    // even for `auth`. A blank token is not a token: the boot path filters it,
    // but the gate must be fail-closed on its own — otherwise an empty
    // `FORGERIG_AUTH_TOKEN` would authenticate any caller offering "".
    let configured = configured.map(str::trim).filter(|t| !t.is_empty());
    let Some(expected) = configured else {
        return AuthGate::Denied(AUTH_REQUIRED, "daemon has no auth token configured (set FORGERIG_AUTH_TOKEN)".into());
    };
    if method == "auth" {
        return match offered {
            Some(t) if constant_time_eq(t, expected) => {
                conn.store(true, Ordering::SeqCst);
                AuthGate::Accepted
            }
            _ => {
                eprintln!("auth: rejected an authentication attempt");
                AuthGate::Denied(AUTH_REQUIRED, "invalid token".into())
            }
        };
    }
    if conn.load(Ordering::SeqCst) {
        AuthGate::Pass
    } else {
        AuthGate::Denied(AUTH_REQUIRED, "authentication required: call `auth` first".into())
    }
}

/// Per-connection stop flags: the chat future and its chat_stop RPC share
/// the same connection task, so a thread-local registry (not a global map)
/// pairs them without cross-connection races.
thread_local! {
    static STOP: std::cell::RefCell<std::collections::HashMap<String, provider::StopFlag>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Build the session's system message, with permanent project memory appended.
///
/// `session` is `Some` once the session id is known: the absolute workspace
/// path is then injected so the model addresses files in its own directory.
async fn system_message_with_memory(memory: &Arc<MemoryEngine>, session: Option<&str>) -> Value {
    let mut sys = provider::system_message();
    if let Some(sid) = session {
        // Name the exact jail: the tools are bound to this directory, so an
        // absolute path outside it is refused by the gatekeeper.
        let dir = gatekeeper::session_workspace(sid);
        let base = sys.get("content").and_then(|c| c.as_str()).unwrap_or_default();
        sys["content"] = json!(format!(
            "{base}\n\nYour session workspace is `{dir}`. It already exists; `cd` into it and keep every file you create there."
        ));
    }
    if let Ok(mems) = memory.get_macro_memories(5).await {
        if !mems.is_empty() {
            let body = mems
                .iter()
                .map(|(m, c)| format!("## {m}\n{c}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            let base = sys.get("content").and_then(|c| c.as_str()).unwrap_or_default();
            sys["content"] = json!(format!("{base}\n\n### Project memory (permanent)\n{body}"));
        }
    }
    sys
}

async fn handle_rpc(req: RpcRequest, backend: &Arc<provider::Backend>, memory: &Arc<MemoryEngine>, sessions: &Arc<sessions::SessionManager>, push: &WsPush, auth_token: Option<&str>, conn: &std::sync::atomic::AtomicBool) -> RpcResponse {
    fn ok(result: Value, id: Option<Value>) -> RpcResponse {
        RpcResponse { jsonrpc: "2.0".into(), result: Some(result), error: None, id }
    }
    fn err(code: i32, message: String, id: Option<Value>) -> RpcResponse {
        RpcResponse { jsonrpc: "2.0".into(), result: None, error: Some(RpcError { code, message }), id }
    }

    // Authentication gate, ahead of every method — see the note by `auth_gate`.
    let offered = req.params.as_ref().and_then(|p| p.get("token")).and_then(|t| t.as_str());
    match auth_gate(auth_token, conn, &req.method, offered) {
        AuthGate::Pass => {}
        AuthGate::Accepted => return ok(json!({ "authenticated": true }), req.id),
        AuthGate::Denied(code, message) => return err(code, message, req.id),
    }

    match req.method.as_str() {
        "status" => ok(json!({ "provider": backend.describe() }), req.id),
        "exec" => {
            let cmd = req.params.as_ref().and_then(|p| p.get("command").and_then(|c| c.as_str())).map(|s| s.trim().to_string());
            match cmd {
                Some(c) if !c.is_empty() => {
                    if c.len() > gatekeeper::MAX_TOOL_ARG_BYTES {
                        gatekeeper::log_verdict("exec", false, "command-too-long", &c.len().to_string());
                        return err(-32602, "command too long".into(), req.id);
                    }
                    let r = tools::run_trusted(&c).await;
                    // Truncate + scrub before the payload crosses back over RPC.
                    let (stdout, _) = gatekeeper::scrub_secrets(&r.stdout);
                    let (stderr, _) = gatekeeper::scrub_secrets(&r.stderr);
                    let (stdout, _) = gatekeeper::truncate_output(&stdout);
                    let (stderr, _) = gatekeeper::truncate_output(&stderr);
                    ok(json!({ "stdout": stdout, "stderr": stderr, "exit_code": r.exit_code, "timed_out": r.timed_out }), req.id)
                }
                _ => err(-32602, "Missing 'command' in params".into(), req.id),
            }
        }
        "chat" => {
            let prompt = req.params.as_ref().and_then(|p| p.get("prompt").and_then(|p| p.as_str())).map(|s| s.to_string());
            let session_id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).map(|s| s.to_string());
            match prompt {
                Some(p) if !p.trim().is_empty() => {
                    if p.len() > gatekeeper::MAX_PROMPT_BYTES {
                        gatekeeper::log_verdict("chat", false, "prompt-too-long", &p.len().to_string());
                        return err(-32602, "prompt too long".into(), req.id);
                    }
                    // Reuse an existing session's cached thread, or start a new one.
                    let sid = match session_id.filter(|s| sessions.exists(s)) {
                        Some(id) => id,
                        None => sessions.create(system_message_with_memory(memory, None).await).id,
                    };
                    // Commit the user message to the tab immediately (before
                    // the provider is even contacted) so the tab exists, has
                    // a title, and survives a mid-flight tab switch.
                    sessions.append_message(&sid, json!({ "role": "user", "content": p }));
                    let mut messages = sessions.thread(&sid).unwrap_or_else(|| vec![provider::system_message()]);
                    // Keep the session's system message synced with the latest
                    // permanent project memory (spans sessions/projects) and
                    // with this session's workspace path.
                    if let Some(first) = messages.first_mut() {
                        *first = system_message_with_memory(memory, Some(&sid)).await;
                    }
                    eprintln!("chat: session={sid} prompt_len={}", p.len());
                    // Streaming: acknowledge immediately so the UI can paint
                    // progress, then run the agent loop in the background and
                    // forward chat_chunk / chat_tool notifications over this
                    // socket, finishing with chat_done, chat_stopped, or
                    // chat_error.
                    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<provider::StreamEvent>();
                    let bridge_push = Arc::clone(push);
                    let sid_bridge = sid.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = erx.recv().await {
                            push_notification(&bridge_push, ev.into_rpc(&sid_bridge)).await;
                        }
                    });
                    let stop: provider::StopFlag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    STOP.with(|cell| { cell.borrow_mut().insert(sid.clone(), std::sync::Arc::clone(&stop)); });
                    let backend2 = Arc::clone(backend);
                    let memory2 = Arc::clone(memory);
                    let sessions2 = Arc::clone(sessions);
                    let push2 = Arc::clone(push);
                    let sid2 = sid.clone();
                    tokio::spawn(async move {
                        match backend2.chat_session_streaming(&mut messages, &p, &etx, &stop, &sid2).await {
                            Ok(completion) => {
                                eprintln!("chat: completion (len={})", completion.len());
                                let (sc, _) = gatekeeper::scrub_secrets(&completion);
                                let _ = memory2.log_trace(&sid2, &p, &sc).await;
                                // Persist the extended thread as the reusable session cache.
                                sessions2.set_messages(&sid2, messages);
                                // Detached: every 5th trace, evaluate recent traces for
                                // milestones (prune noise + record macro memory). Never
                                // blocks the reply.
                                let backend3 = Arc::clone(&backend2);
                                let memory3 = Arc::clone(&memory2);
                                tokio::spawn(async move {
                                    let recent = memory3.get_recent_traces(1).await.map_err(|e| e.to_string());
                                    if let Ok(recent) = recent {
                                        if let Some((id, _, _)) = recent.first() {
                                            if id % 5 == 0 {
                                                let traces = memory3.get_recent_traces(10).await.map_err(|e| e.to_string());
                                                if let Ok(traces) = traces {
                                                    if let Err(e) = memory::evaluate_and_process(&backend3, &memory3, traces).await {
                                                        eprintln!("Background evaluation failed: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                });
                                push_notification(
                                    &push2,
                                    json!({ "jsonrpc": "2.0", "method": "chat_done",
                                            "params": { "session_id": sid2, "reply": completion } }),
                                )
                                .await;
                            }
                            Err(e) => {
                                eprintln!("chat: error: {}", e);
                                if e == "stopped by user" {
                                    // True stop: the thinking bubble is torn
                                    // down client-side by chat_stopped; the
                                    // partial (if any) was already banked by
                                    // chat_stop. Emit nothing further.
                                    return;
                                }
                                // Failed turns leave the optimistic user
                                // message in place (committed at send time);
                                // only the error bubble is ephemeral.
                                push_notification(
                                    &push2,
                                    json!({ "jsonrpc": "2.0", "method": "chat_error",
                                            "params": { "session_id": sid2, "error": e } }),
                                )
                                .await;
                            }
                        }
                    });
                    ok(json!({ "accepted": true, "session_id": sid }), req.id)
                }
                _ => err(-32602, "Missing 'prompt' in params".into(), req.id),
            }
        }
        "session_list" => ok(json!(sessions.list()), req.id),
        "session_create" => {
            let s = sessions.create(provider::system_message());
            tools::ensure_session_workspace(&s.id).await;
            ok(json!({ "id": s.id, "title": s.title, "messages": s.thread(), "workspace": s.workspace() }), req.id)
        }
        "session_spawn" => {
            let parent = req.params.as_ref().and_then(|p| p.get("parent_session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let role = req.params.as_ref().and_then(|p| p.get("role")).and_then(|s| s.as_str()).unwrap_or("member").to_string();
            let goal = req.params.as_ref().and_then(|p| p.get("goal")).and_then(|s| s.as_str()).unwrap_or("").to_string();
            if parent.is_empty() {
                return err(-32602, "Missing 'parent_session_id' in params".into(), req.id);
            }
            if role.trim().is_empty() || role.len() > 40 {
                return err(-32602, "role must be 1-40 chars".into(), req.id);
            }
            if goal.len() > 2000 {
                return err(-32602, "goal too long (2000 chars max)".into(), req.id);
            }
            // The spawn's own id is what the sub-agent will use, so its system
            // message must not name the parent's workspace. It is filled in on
            // the sub-agent's first turn, once its id is known.
            match sessions.spawn_subagent(&parent, &role, &goal, system_message_with_memory(memory, None).await) {
                Some(s) => {
                    tools::ensure_session_workspace(&s.id).await;
                    gatekeeper::log_verdict("session_spawn", true, "sub-agent spawned", &format!("{parent} -> {}", s.id));
                    ok(json!({ "id": s.id, "title": s.title, "parent_id": s.parent_id, "role": s.role, "goal": s.goal, "workspace": s.workspace() }), req.id)
                }
                None => {
                    gatekeeper::log_verdict("session_spawn", false, "unknown parent or blank role", &parent);
                    err(-32602, format!("cannot spawn under unknown session '{parent}'"), req.id)
                }
            }
        }
        "session_children" => {
            let parent = req.params.as_ref().and_then(|p| p.get("parent_session_id")).and_then(|s| s.as_str()).unwrap_or_default();
            if !sessions.exists(parent) {
                return err(-32602, format!("unknown session '{parent}'"), req.id);
            }
            ok(json!(sessions.subagents(parent)), req.id)
        }
        "session_merge" => {
            let child = req.params.as_ref().and_then(|p| p.get("child_session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            if child.is_empty() {
                return err(-32602, "Missing 'child_session_id' in params".into(), req.id);
            }
            match sessions.merge_child(&child) {
                Some(p) => {
                    gatekeeper::log_verdict("session_merge", true, "sub-agent merged", &format!("{child} -> {}", p.id));
                    ok(json!({ "id": p.id, "title": p.title, "messages": p.thread(), "archived_child": child }), req.id)
                }
                None => {
                    gatekeeper::log_verdict("session_merge", false, "unknown/archived child or missing parent", &child);
                    err(-32602, format!("cannot merge unknown session '{child}'"), req.id)
                }
            }
        }
        "session_history" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default();
            match sessions.get(id) {
                Some(s) => ok(json!({ "id": s.id, "title": s.title, "messages": s.thread(), "nav": sessions.nav(id) }), req.id),
                None => err(-32602, format!("unknown session '{id}'"), req.id),
            }
        }
        "session_fork" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let index = req.params.as_ref().and_then(|p| p.get("message_index")).and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            match sessions.fork(&id, index) {
                Some(f) => ok(json!({ "id": f.id, "title": f.title, "messages": f.thread() }), req.id),
                None => err(-32602, format!("cannot fork unknown session '{id}'"), req.id),
            }
        }
        "session_undo" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.undo(&id) {
                Some(text) => ok(json!({ "user_message": text, "messages": sessions.thread(&id).unwrap_or_default(), "nav": sessions.nav(&id) }), req.id),
                None => err(-32602, format!("cannot undo unknown session '{id}'"), req.id),
            }
        }
        "session_redo" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.redo(&id) {
                Some(messages) => ok(json!({ "messages": messages, "nav": sessions.nav(&id) }), req.id),
                None => err(-32602, format!("nothing to redo in session '{id}'"), req.id),
            }
        }
        "session_goto" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let node = req.params.as_ref().and_then(|p| p.get("node")).and_then(|n| n.as_u64()).unwrap_or(u64::MAX);
            match sessions.goto(&id, node) {
                Some(messages) => ok(json!({ "messages": messages, "nav": sessions.nav(&id) }), req.id),
                None => err(-32602, format!("unknown node in session '{id}'"), req.id),
            }
        }
        "session_nav" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.nav(&id) {
                Some(nav) => ok(json!({ "nav": nav, "messages": sessions.thread(&id).unwrap_or_default() }), req.id),
                None => err(-32602, format!("unknown session '{id}'"), req.id),
            }
        }
        "session_archive" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let archived = req.params.as_ref().and_then(|p| p.get("archived")).and_then(|a| a.as_bool()).unwrap_or(true);
            ok(json!({ "archived": sessions.set_archived(&id, archived) }), req.id)
        }
        "chat_stop" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let partial = req.params.as_ref().and_then(|p| p.get("partial")).and_then(|x| x.as_str()).unwrap_or_default().to_string();
            STOP.with(|cell| {
                if let Some(flag) = cell.borrow().get(&id) {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            });
            // Bank the client's partial text as the transcript position so a
            // later Resume continues from near the cutoff, not from scratch.
            if !partial.trim().is_empty() {
                let _ = sessions.append_message(&id, json!({ "role": "assistant", "content": partial }));
            }
            ok(json!({ "stopped": true }), req.id)
        }
        "session_delete" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            ok(json!({ "deleted": sessions.delete(&id) }), req.id)
        }
        "lean_status" => {
            let st = lean::status().await;
            ok(json!(st), req.id)
        }
        "lean_progress" => {
            ok(json!(lean::progress()), req.id)
        }
        "lean_provision" => {
            let message = lean::kick_off_provision().await;
            ok(json!({ "message": message }), req.id)
        }
        "client_error" => {
            // WebView JS/HTML errors surfaced by the UI. Written to stderr
            // so the app pipes them into the shared Downloads error log —
            // on-device JS failures are otherwise invisible.
            let p = req.params.as_ref();
            let kind = p.and_then(|x| x.get("kind")).and_then(|x| x.as_str()).unwrap_or("js");
            let message = p.and_then(|x| x.get("message")).and_then(|x| x.as_str()).unwrap_or("(no message)");
            let stack = p.and_then(|x| x.get("stack")).and_then(|x| x.as_str()).unwrap_or("");
            let url = p.and_then(|x| x.get("url")).and_then(|x| x.as_str()).unwrap_or("");
            let line = p.and_then(|x| x.get("line")).and_then(|x| x.as_u64()).unwrap_or(0);
            eprintln!("client error [{kind}] {message} ({url}:{line}){stack}");
            ok(json!({ "logged": true }), req.id)
        }
        "lean" => {
            let file = req.params.as_ref().and_then(|p| p.get("file").and_then(|f| f.as_str())).map(|s| s.trim().to_string());
            match file {
                Some(f) if !f.is_empty() => {
                    let r = lean::run_on_file(&f).await;
                    ok(json!({ "stdout": r.stdout, "stderr": r.stderr, "exit_code": r.exit_code, "timed_out": r.timed_out }), req.id)
                }
                _ => err(-32602, "Missing 'file' in params".into(), req.id),
            }
        }
        "wasm_transform" => {
            let args = req.params.as_ref().and_then(|p| {
                let wat = p.get("wat").and_then(|v| v.as_str()).map(|s| s.to_string());
                let base64_wasm = p.get("base64_wasm").and_then(|v| v.as_str()).map(|s| s.to_string());
                let input = p.get("input").and_then(|v| v.as_i64()).map(|v| v as i32);
                if input.is_none() {
                    return None;
                }
                Some(wasm::WasmTransformerArgs { wat, base64_wasm, input: input.unwrap() })
            });
            match args {
                Some(args) => {
                    let tool = wasm::WasmTransformer::default();
                    match tool.call(args).await {
                        Ok(result) => ok(json!({ "output": result.output, "fuel_consumed": result.fuel_consumed }), req.id),
                        Err(e) => err(-32603, format!("Wasm transform error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'input' in params (and either 'wat' or 'base64_wasm')".into(), req.id),
            }
        }
        "net_fetch" => {
            let args = req.params.as_ref().and_then(|p| {
                let url = p.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
                let sha256 = p.get("sha256").and_then(|v| v.as_str()).map(|s| s.to_string());
                let max_bytes = p.get("max_bytes").and_then(|v| v.as_u64()).unwrap_or(2 * 1024 * 1024);
                let scope = p.get("scope").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| "global".to_string());
                if url.is_none() {
                    return None;
                }
                Some((net_fetch::NetFetchArgs { url: url.unwrap(), sha256, max_bytes: max_bytes as usize }, scope))
            });
            match args {
                Some((args, scope)) => {
                    let tool = net_fetch::NetFetchTool::new(memory.clone(), scope);
                    match tool.call(args).await {
                        Ok(result) => ok(json!({ "url": result.url, "status": result.status, "content_type": result.content_type, "body": result.body, "truncated": result.truncated, "sha256": result.sha256 }), req.id),
                        Err(e) => err(-32603, format!("Net fetch error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'url' in params".into(), req.id),
            }
        }
        "ingest" => {
            let path = req.params.as_ref().and_then(|p| p.get("workspace_path").and_then(|f| f.as_str())).map(|s| s.trim().to_string());
            // Jail to the guest workspace before touching the filesystem: the
            // daemon is host-side, so an unjailed root exposes the app's own
            // filesDir to the UI/model. `guest_to_host` then translates, since
            // `ingest_workspace` walks the HOST filesystem directly.
            let path = match path.as_deref().map(gatekeeper::validate_guest_path) {
                Some(Ok(p)) => Some(gatekeeper::guest_to_host(&p)),
                Some(Err(reason)) => {
                    gatekeeper::log_verdict("ingest", false, &reason, path.as_deref().unwrap_or(""));
                    return err(-32603, format!("Ingest blocked by gatekeeper: {reason}"), req.id);
                }
                None => None,
            };
            match path {
                Some(p) if !p.is_empty() => {
                    let params = req.params.as_ref();
                    let opts = ingest::IngestOptions {
                        max_files: params.and_then(|x| x.get("max_files")).and_then(|x| x.as_u64()).unwrap_or(200) as usize,
                        use_path_table: params.and_then(|x| x.get("use_path_table")).and_then(|x| x.as_bool()).unwrap_or(true),
                        use_dedup: params.and_then(|x| x.get("use_dedup")).and_then(|x| x.as_bool()).unwrap_or(true),
                        ..Default::default()
                    };
                    match ingest::ingest_workspace(&p.to_string_lossy(), &opts) {
                        Ok(o) => ok(json!({ "framed": o.framed, "files": o.files, "bytes": o.bytes, "deduped": o.deduped, "truncated": o.truncated }), req.id),
                        Err(e) => err(-32603, format!("Ingest error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'workspace_path' in params".into(), req.id),
            }
        }
        "network_policy_list" => {
            let scope = req.params.as_ref().and_then(|p| p.get("scope").and_then(|s| s.as_str())).unwrap_or("global");
            match memory.list_allowed_domains(scope).await {
                Ok(domains) => ok(json!({ "domains": domains }), req.id),
                Err(e) => err(-32603, format!("NetworkPolicy error: {e}"), req.id),
            }
        }
        "network_policy_add" => {
            let scope = req.params.as_ref().and_then(|p| p.get("scope").and_then(|s| s.as_str())).unwrap_or("global");
            let domain = req.params.as_ref().and_then(|p| p.get("domain").and_then(|s| s.as_str())).map(|s| s.trim().to_string());
            match domain {
                Some(d) if !d.is_empty() => {
                    match memory.allow_domain(scope, &d).await {
                        Ok(_) => ok(json!({ "added": d }), req.id),
                        Err(e) => err(-32603, format!("NetworkPolicy error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'domain' in params".into(), req.id),
            }
        }
        "network_policy_remove" => {
            let scope = req.params.as_ref().and_then(|p| p.get("scope").and_then(|s| s.as_str())).unwrap_or("global");
            let domain = req.params.as_ref().and_then(|p| p.get("domain").and_then(|s| s.as_str())).map(|s| s.trim().to_string());
            match domain {
                Some(d) if !d.is_empty() => {
                    match memory.deny_domain(scope, &d).await {
                        Ok(_) => ok(json!({ "removed": d }), req.id),
                        Err(e) => err(-32603, format!("NetworkPolicy error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'domain' in params".into(), req.id),
            }
        }
        _ => err(-32601, "Method not found".into(), req.id),
    }
}

fn dns_probe(host: &str) {
    // Daemon-side DNS diagnostic: isolates whether the failure is in musl
    // resolution (no system resolver access from a static binary) vs network.
    use std::net::ToSocketAddrs;
    let t0 = std::time::Instant::now();
    match (host, 443u16).to_socket_addrs() {
        Ok(mut addrs) => {
            let n = addrs.len();
            eprintln!("dns probe: {} resolved {} addr(s) in {:?} (first={:?})", host, n, t0.elapsed(), addrs.next());
        }
        Err(e) => eprintln!("dns probe: {} FAILED to resolve in {:?}: {}", host, t0.elapsed(), e),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    if let Err(e) = run().await {
        // Exact startup failure with its chain: the app surfaces this line
        // in the install log, so a dead daemon is diagnosable on-device.
        eprintln!("daemon: FATAL startup error: {e:#}");
        return Err(e);
    }
    Ok(())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {

    // Harvest credentials the app persisted into the guest so they can never
    // be echoed into model context by a tool (`cat /root/.gitconfig`).
    if let Ok(rootfs) = std::env::var("CONTAINER_ROOTFS") {
        if !rootfs.is_empty() {
            gatekeeper::register_gitconfig_secrets(std::path::Path::new(&rootfs));
        }
    }

    dns_probe("github.com");
    // Termux-style fallback: if the daemon cannot resolve, at least surface
    // that fact for every HTTP-dependent feature up-front.

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("127.0.0.1:{}", port);

    let memory_engine = Arc::new(
        MemoryEngine::new("oss_memory.db")
            .await
            .map_err(|e| format!("open memory db oss_memory.db: {e}"))?,
    );
    // Per-launch shared secret; the app mints it and hands it to the UI over
    // the JS bridge. Absent => fail closed (every RPC refused).
    let auth_token = std::env::var("FORGERIG_AUTH_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if auth_token.is_none() {
        eprintln!(
            "FATAL: FORGERIG_AUTH_TOKEN is not set — refusing every RPC. \
             The app always sets it; export one to run the daemon by hand."
        );
    }
    let backend = Arc::new(provider::Backend::resolve(memory_engine.clone()).await);
    let persist_path = std::env::var("FORGERIG_SESSIONS_FILE")
        .map(PathBuf::from)
        .ok();
    let sessions = Arc::new(sessions::SessionManager::with_persist_path(persist_path));
    println!("Listening on: {} ({})", addr, backend.describe());

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

    // Lean is part of the environment install/startup step: begin provisioning
    // automatically on daemon boot so the UI can poll progress as soon as it
    // connects (no manual "Download & install" tap required).
    let _ = lean::kick_off_provision().await;

    while let Ok((stream, _)) = listener.accept().await {
        let backend = Arc::clone(&backend);
        let memory = Arc::clone(&memory_engine);
        let sessions = Arc::clone(&sessions);
        let auth_token = auth_token.clone();

        tokio::spawn(async move {
            // Peek (without consuming) to decide whether this is a WebSocket
            // upgrade or a plain HTTP GET, so a browser/WebView pointed at
            // `http://127.0.0.1:$PORT` gets a working page and can then open
            // the WebSocket on the same port.
            let mut probe = [0u8; 4096];
            let probe_len = match tokio::time::timeout(
                Duration::from_secs(5),
                stream.peek(&mut probe),
            ).await {
                Ok(Ok(n)) => n,
                _ => 0,
            };
            let head = String::from_utf8_lossy(&probe[..probe_len]).to_ascii_lowercase();

            if !head.contains("upgrade: websocket") && !head.contains("sec-websocket-key:") {
                serve_http(stream).await;
                return;
            }

            let ws_stream = match tokio::time::timeout(Duration::from_secs(10), accept_async(stream)).await {
                Ok(Ok(ws)) => ws,
                Ok(Err(e)) => { eprintln!("WebSocket handshake failed: {}", e); return; }
                Err(_) => { eprintln!("WebSocket handshake timed out"); return; }
            };
            println!("New WebSocket connection");

            let (ws_sender, mut ws_receiver) = ws_stream.split();
            let ws_sender = std::sync::Arc::new(tokio::sync::Mutex::new(ws_sender));
            // One auth state per connection, shared by that connection's
            // per-request tasks.
            let conn = conn_auth();

            // Handle each incoming request concurrently so a long chat can't
            // block lean_status/progress polls or another session's chat.
            while let Some(msg) = ws_receiver.next().await {
                if let Ok(msg) = msg {
                    if msg.is_text() {
                        let text = msg.to_text().unwrap().to_string();
                        let backend = Arc::clone(&backend);
                        let memory = Arc::clone(&memory);
                        let sessions = Arc::clone(&sessions);
                        let sender = Arc::clone(&ws_sender);
                        let conn = Arc::clone(&conn);
                        let auth_token = auth_token.clone();
                        tokio::spawn(async move {
                            let response = match serde_json::from_str::<RpcRequest>(&text) {
                                Ok(req) => handle_rpc(req, &backend, &memory, &sessions, &sender, auth_token.as_deref(), &conn).await,
                                Err(_) => RpcResponse {
                                    jsonrpc: "2.0".into(),
                                    result: None,
                                    error: Some(RpcError { code: -32700, message: "Parse error".into() }),
                                    id: None,
                                },
                            };
                            let response_str = serde_json::to_string(&response).unwrap();
                            let mut sink = sender.lock().await;
                            if let Err(e) = sink.send(tokio_tungstenite::tungstenite::Message::Text(response_str)).await {
                                eprintln!("Error sending message: {}", e);
                            }
                        });
                    }
                }
            }
        });
    }

    Ok(())
}

/// Serve a small landing page so `http://127.0.0.1:$PORT` in the app's WebView
/// renders the orchestrator UI instead of a connection error. The page then
/// opens the WebSocket on the same port for JSON-RPC (chat, exec, status).
fn page_html() -> String {
    include_str!("../web/index.html")
        .replace("/*@MARKDOWN_JS@*/", include_str!("../web/markdown.js"))
        .replace("/*@STATE_JS@*/", include_str!("../web/state.js"))
        .replace("/*@APP_JS@*/", include_str!("../web/app.js"))
}

async fn serve_http(mut stream: TcpStream) {
    // Read request line
    let mut buf = [0u8; 4096];
    let n = match tokio::time::timeout(
        Duration::from_secs(5),
        stream.read(&mut buf),
    ).await {
        Ok(Ok(n)) if n > 0 => n,
        _ => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    // API routes
    if path.starts_with("/api/network-policy/") {
        handle_network_policy_api(&mut stream, method, path).await;
        return;
    }

    // Landing page
    let body = page_html();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn handle_network_policy_api(stream: &mut TcpStream, _method: &str, path: &str) {
    // Parse scope and domain from path: /api/network-policy/<scope>/<domain>
    let parts: Vec<&str> = path.trim_start_matches("/api/network-policy/").split('/').collect();
    if parts.is_empty() {
        send_json(stream, 400, json!({ "error": "missing scope" })).await;
        return;
    }
    let _scope = parts[0];

    // For simplicity, we need access to MemoryEngine - but it's not available here.
    // This is a simplified HTTP endpoint that would need MemoryEngine access.
    // For now, return not implemented. The WebSocket RPC is the primary interface.
    let response = json!({ "error": "HTTP API not fully implemented; use WebSocket RPC" });
    send_json(stream, 501, response).await;
}

async fn send_json(stream: &mut TcpStream, status: u16, body: Value) {
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            501 => "Not Implemented",
            _ => "Error",
        },
        body_str.len(),
        body_str
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn denied(gate: AuthGate) -> (i32, String) {
        match gate {
            AuthGate::Denied(code, message) => (code, message),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// Fail-closed is the whole point: a launch that forgets the token must not
    /// silently run wide open, and `auth` must not be a way around it.
    #[test]
    fn no_configured_token_refuses_everything() {
        for method in ["status", "exec", "chat", "auth"] {
            let conn = AtomicBool::new(true); // even a pre-authed connection
            let (code, msg) = denied(auth_gate(None, &conn, method, Some("anything")));
            assert_eq!(code, AUTH_REQUIRED, "{method}");
            assert!(msg.contains("no auth token configured"), "{method}: {msg}");
        }
        // A blank/empty token is not a token.
        let conn = AtomicBool::new(false);
        assert!(matches!(auth_gate(Some(""), &conn, "auth", Some("")), AuthGate::Denied(..)));
    }

    /// Nothing but `auth` runs before authentication.
    #[test]
    fn every_method_is_refused_before_auth() {
        let conn = AtomicBool::new(false);
        for method in ["status", "exec", "chat", "session_list", "ingest", "net_fetch", "lean"] {
            let (code, msg) = denied(auth_gate(Some("tok"), &conn, method, None));
            assert_eq!(code, AUTH_REQUIRED, "{method}");
            assert!(msg.contains("call `auth` first"), "{method}: {msg}");
        }
    }

    /// A wrong or missing token must not flip the connection, and the right one
    /// must.
    #[test]
    fn auth_compares_the_token_and_latches_the_connection() {
        let conn = AtomicBool::new(false);
        assert!(matches!(auth_gate(Some("s3cret"), &conn, "auth", Some("wrong")), AuthGate::Denied(..)));
        assert!(!conn.load(Ordering::SeqCst), "a rejected auth must not latch");
        // Absent token field is a rejection too, not a bypass.
        assert!(matches!(auth_gate(Some("s3cret"), &conn, "auth", None), AuthGate::Denied(..)));
        assert!(!conn.load(Ordering::SeqCst));
        assert!(matches!(auth_gate(Some("s3cret"), &conn, "auth", Some("s3cret")), AuthGate::Accepted));
        assert!(conn.load(Ordering::SeqCst));
        // And now ordinary methods pass on THIS connection.
        assert!(matches!(auth_gate(Some("s3cret"), &conn, "status", None), AuthGate::Pass));
    }

    /// The regression that makes the whole feature a no-op if written naively:
    /// authenticating one connection must not vouch for another.
    #[test]
    fn auth_does_not_leak_across_connections() {
        let a = AtomicBool::new(false);
        let b = AtomicBool::new(false);
        assert!(matches!(auth_gate(Some("tok"), &a, "auth", Some("tok")), AuthGate::Accepted));
        assert!(a.load(Ordering::SeqCst));
        assert!(matches!(auth_gate(Some("tok"), &a, "exec", None), AuthGate::Pass));
        // Second, independent connection: still refused.
        assert!(matches!(auth_gate(Some("tok"), &b, "exec", None), AuthGate::Denied(..)));
        assert!(!b.load(Ordering::SeqCst));
    }

    #[test]
    fn constant_time_eq_matches_equality_semantics() {
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("ab", "abc"));
        // A token that is a prefix of the real one must not pass.
        assert!(!constant_time_eq("tok", "tok-extra"));
    }

    #[test]
    fn served_page_inlines_all_frontend_js() {
        let html = super::page_html();
        // Placeholders must be fully replaced.
        assert!(!html.contains("/*@"));
        // Markup, state, and app wiring must be present.
        assert!(html.contains("id=\"tab-list\""));
        assert!(html.contains("id=\"composer\""));
        assert!(html.contains("id=\"send-btn\""));
        assert!(html.contains("id=\"lean-pct\""));
        assert!(html.contains("id=\"setup-notice\""));
        assert!(html.contains("function render"));
        assert!(html.contains("function visibleTurns"));
        assert!(html.contains("function forkRawIndex"));
        assert!(html.contains("function sendMessage"));
    }
}