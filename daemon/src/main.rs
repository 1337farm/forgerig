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

mod tools;
mod wasm;
mod memory;
mod provider;
mod lean;
mod sessions;
mod ingest;

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
/// chat_tool, chat_done, chat_error) on the same WebSocket as the request.
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

async fn system_message_with_memory(memory: &Arc<MemoryEngine>) -> Value {
    let mut sys = provider::system_message();
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

async fn handle_rpc(req: RpcRequest, backend: &Arc<provider::Backend>, memory: &Arc<MemoryEngine>, sessions: &Arc<sessions::SessionManager>, push: &WsPush) -> RpcResponse {
    fn ok(result: Value, id: Option<Value>) -> RpcResponse {
        RpcResponse { jsonrpc: "2.0".into(), result: Some(result), error: None, id }
    }
    fn err(code: i32, message: String, id: Option<Value>) -> RpcResponse {
        RpcResponse { jsonrpc: "2.0".into(), result: None, error: Some(RpcError { code, message }), id }
    }

    match req.method.as_str() {
        "status" => ok(json!({ "provider": backend.describe() }), req.id),
        "exec" => {
            let cmd = req.params.as_ref().and_then(|p| p.get("command").and_then(|c| c.as_str())).map(|s| s.trim().to_string());
            match cmd {
                Some(c) if !c.is_empty() => {
                    let r = tools::run_trusted(&c).await;
                    ok(json!({ "stdout": r.stdout, "stderr": r.stderr, "exit_code": r.exit_code, "timed_out": r.timed_out }), req.id)
                }
                _ => err(-32602, "Missing 'command' in params".into(), req.id),
            }
        }
        "chat" => {
            let prompt = req.params.as_ref().and_then(|p| p.get("prompt").and_then(|p| p.as_str())).map(|s| s.to_string());
            let session_id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).map(|s| s.to_string());
            match prompt {
                Some(p) if !p.trim().is_empty() => {
                    // Reuse an existing session's cached thread, or start a new one.
                    let sid = match session_id.filter(|s| sessions.exists(s)) {
                        Some(id) => id,
                        None => sessions.create(system_message_with_memory(memory).await).id,
                    };
                    let mut messages = sessions.get(&sid).map(|s| s.messages).unwrap_or_else(|| vec![provider::system_message()]);
                    // Keep the session's system message synced with the latest
                    // permanent project memory (spans sessions/projects).
                    if let Some(first) = messages.first_mut() {
                        *first = system_message_with_memory(memory).await;
                    }
                    eprintln!("chat: session={sid} prompt_len={}", p.len());
                    // Streaming: acknowledge immediately so the UI can paint
                    // progress, then run the agent loop in the background and
                    // forward chat_chunk / chat_tool notifications over this
                    // socket, finishing with chat_done (or chat_error).
                    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<provider::StreamEvent>();
                    let bridge_push = Arc::clone(push);
                    let sid_bridge = sid.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = erx.recv().await {
                            push_notification(&bridge_push, ev.into_rpc(&sid_bridge)).await;
                        }
                    });
                    let backend2 = Arc::clone(backend);
                    let memory2 = Arc::clone(memory);
                    let sessions2 = Arc::clone(sessions);
                    let push2 = Arc::clone(push);
                    let sid2 = sid.clone();
                    tokio::spawn(async move {
                        match backend2.chat_session_streaming(&mut messages, &p, &etx).await {
                            Ok(completion) => {
                                eprintln!("chat: completion (len={})", completion.len());
                                let _ = memory2.log_trace(&p, &completion).await;
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
                                // Deliberately NOT persisted: the client puts the
                                // failed text back in the composer for edit+retry,
                                // so persisting here would duplicate it on resend.
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
            ok(json!({ "id": s.id, "title": s.title, "messages": s.messages }), req.id)
        }
        "session_history" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default();
            match sessions.get(id) {
                Some(s) => ok(json!({ "id": s.id, "title": s.title, "messages": s.messages }), req.id),
                None => err(-32602, format!("unknown session '{id}'"), req.id),
            }
        }
        "session_fork" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let index = req.params.as_ref().and_then(|p| p.get("message_index")).and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            match sessions.fork(&id, index) {
                Some(f) => ok(json!({ "id": f.id, "title": f.title, "messages": f.messages }), req.id),
                None => err(-32602, format!("cannot fork unknown session '{id}'"), req.id),
            }
        }
        "session_delete" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            ok(json!({ "deleted": sessions.delete(&id) }), req.id)
        }
        "session_undo" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.undo(&id) {
                Some(text) => ok(json!({ "user_message": text }), req.id),
                None => err(-32602, format!("cannot undo unknown session '{id}'"), req.id),
            }
        }
        "session_rename" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            let title = req.params.as_ref().and_then(|p| p.get("title")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.set_title(&id, &title) {
                Some(s) => ok(json!({ "id": s.id, "title": s.title }), req.id),
                None => err(-32602, format!("cannot rename unknown session '{id}' (title must be non-blank)"), req.id),
            }
        }
        "session_closed" => ok(json!(sessions.trash_list()), req.id),
        "session_restore" => {
            let id = req.params.as_ref().and_then(|p| p.get("session_id")).and_then(|s| s.as_str()).unwrap_or_default().to_string();
            match sessions.restore(&id) {
                Some(s) => ok(json!({ "id": s.id, "title": s.title, "messages": s.messages }), req.id),
                None => err(-32602, format!("unknown closed session '{id}'"), req.id),
            }
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
        "ingest" => {
            let path = req.params.as_ref().and_then(|p| p.get("workspace_path").and_then(|f| f.as_str())).map(|s| s.trim().to_string());
            match path {
                Some(p) if !p.is_empty() => {
                    let params = req.params.as_ref();
                    let opts = ingest::IngestOptions {
                        max_files: params.and_then(|x| x.get("max_files")).and_then(|x| x.as_u64()).unwrap_or(200) as usize,
                        use_path_table: params.and_then(|x| x.get("use_path_table")).and_then(|x| x.as_bool()).unwrap_or(true),
                        use_dedup: params.and_then(|x| x.get("use_dedup")).and_then(|x| x.as_bool()).unwrap_or(true),
                        ..Default::default()
                    };
                    match ingest::ingest_workspace(&p, &opts) {
                        Ok(o) => ok(json!({ "framed": o.framed, "files": o.files, "bytes": o.bytes, "deduped": o.deduped, "truncated": o.truncated }), req.id),
                        Err(e) => err(-32603, format!("Ingest error: {e}"), req.id),
                    }
                }
                _ => err(-32602, "Missing 'workspace_path' in params".into(), req.id),
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

    dns_probe("github.com");
    // Termux-style fallback: if the daemon cannot resolve, at least surface
    // that fact for every HTTP-dependent feature up-front.

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("127.0.0.1:{}", port);

    let backend = Arc::new(provider::Backend::resolve().await);
    let memory_engine = Arc::new(
        MemoryEngine::new("oss_memory.db")
            .await
            .map_err(|e| format!("open memory db oss_memory.db: {e}"))?,
    );
    let persist_path = std::env::var("FORGERIG_SESSIONS_FILE")
        .map(PathBuf::from)
        .ok();
    let sessions = Arc::new(sessions::SessionManager::with_persist_path(persist_path));
    println!("Listening on: {} ({})", addr, backend.describe());

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

    while let Ok((stream, _)) = listener.accept().await {
        let backend = Arc::clone(&backend);
        let memory = Arc::clone(&memory_engine);
        let sessions = Arc::clone(&sessions);

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
                        tokio::spawn(async move {
                            let response = match serde_json::from_str::<RpcRequest>(&text) {
                                Ok(req) => handle_rpc(req, &backend, &memory, &sessions, &sender).await,
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

/// Bundled UI font (Intel One Mono, latin subset) served at /font.woff2.
static FONT_WOFF2: &[u8] = include_bytes!("../web/IntelOneMono-Regular.woff2");

async fn read_http_path(stream: &mut TcpStream) -> String {
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.peek(&mut buf))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string()
}

async fn serve_http(mut stream: TcpStream) {
    if read_http_path(&mut stream).await == "/font.woff2" {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nContent-Length: {}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n",
            FONT_WOFF2.len(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.write_all(FONT_WOFF2).await;
        let _ = stream.shutdown().await;
        return;
    }
    let body = page_html();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
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