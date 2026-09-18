use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
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

async fn handle_rpc(req: RpcRequest, backend: &Arc<provider::Backend>, memory: &Arc<MemoryEngine>, sessions: &Arc<sessions::SessionManager>) -> RpcResponse {
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
                    match backend.chat_session(&mut messages, &p).await {
                        Ok(completion) => {
                            eprintln!("chat: completion (len={})", completion.len());
                            let _ = memory.log_trace(&p, &completion).await;
                            // Persist the extended thread as the reusable session cache.
                            sessions.set_messages(&sid, messages);
                            // Detached: every 5th trace, evaluate recent traces for
                            // milestones (prune noise + record macro memory). Never
                            // blocks the reply.
                            let backend2 = Arc::clone(backend);
                            let memory2 = Arc::clone(memory);
                            tokio::spawn(async move {
                                let recent = memory2.get_recent_traces(1).await.map_err(|e| e.to_string());
                                if let Ok(recent) = recent {
                                    if let Some((id, _, _)) = recent.first() {
                                        if id % 5 == 0 {
                                            let traces = memory2.get_recent_traces(10).await.map_err(|e| e.to_string());
                                            if let Ok(traces) = traces {
                                                if let Err(e) = memory::evaluate_and_process(&backend2, &memory2, traces).await {
                                                    eprintln!("Background evaluation failed: {}", e);
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                            ok(json!({ "reply": completion, "session_id": sid }), req.id)
                        }
                        Err(e) => { eprintln!("chat: error: {}", e); err(-32603, format!("Agent error: {}", e), req.id) }
                    }
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

    dns_probe("github.com");
    // Termux-style fallback: if the daemon cannot resolve, at least surface
    // that fact for every HTTP-dependent feature up-front.

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("127.0.0.1:{}", port);

    let backend = Arc::new(provider::Backend::resolve().await);
    let memory_engine = Arc::new(MemoryEngine::new("oss_memory.db").await?);
    let sessions = Arc::new(sessions::SessionManager::new());
    println!("Listening on: {} ({})", addr, backend.describe());

    let listener = TcpListener::bind(&addr).await?;

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
                                Ok(req) => handle_rpc(req, &backend, &memory, &sessions).await,
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
        assert!(html.contains("function render"));
        assert!(html.contains("function visibleTurns"));
        assert!(html.contains("function forkRawIndex"));
        assert!(html.contains("function sendMessage"));
    }
}