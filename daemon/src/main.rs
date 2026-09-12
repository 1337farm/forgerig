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

async fn build_context(memory: &Arc<MemoryEngine>, prompt: &str) -> String {
    match memory.get_macro_memories(5).await {
        Ok(mems) if !mems.is_empty() => {
            let body = mems
                .into_iter()
                .map(|(m, c)| format!("## {}\n{}", m, c))
                .collect::<Vec<_>>()
                .join("\n\n");
            format!("### Architectural memory (prior milestones)\n{}\n\n### Current task\n{}", body, prompt)
        }
        _ => prompt.to_string(),
    }
}

async fn handle_rpc(req: RpcRequest, backend: &Arc<provider::Backend>, memory: &Arc<MemoryEngine>) -> RpcResponse {
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
            match prompt {
                Some(p) if !p.trim().is_empty() => {
                    let full = build_context(memory, &p).await;
                    match backend.chat(&full).await {
                        Ok(completion) => {
                            if let Err(e) = memory.log_trace(&p, &completion).await {
                                eprintln!("Failed to log trace to memory: {}", e);
                            }
                            // Periodically evaluate traces (every 5th) to detect
                            // milestones and prune noise; runs detached.
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
                            ok(json!(completion), req.id)
                        }
                        Err(e) => err(-32603, format!("Agent error: {}", e), req.id),
                    }
                }
                _ => err(-32602, "Missing 'prompt' in params".into(), req.id),
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

    let backend = Arc::new(provider::Backend::resolve());
    let memory_engine = Arc::new(MemoryEngine::new("oss_memory.db").await?);
    println!("Listening on: {} ({})", addr, backend.describe());

    let listener = TcpListener::bind(&addr).await?;

    while let Ok((stream, _)) = listener.accept().await {
        let backend = Arc::clone(&backend);
        let memory = Arc::clone(&memory_engine);

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

            let (mut ws_sender, mut ws_receiver) = ws_stream.split();

            while let Some(msg) = ws_receiver.next().await {
                if let Ok(msg) = msg {
                    if msg.is_text() {
                        let text = msg.to_text().unwrap();
                        let response = match serde_json::from_str::<RpcRequest>(text) {
                            Ok(req) => handle_rpc(req, &backend, &memory).await,
                            Err(_) => RpcResponse {
                                jsonrpc: "2.0".into(),
                                result: None,
                                error: Some(RpcError { code: -32700, message: "Parse error".into() }),
                                id: None,
                            },
                        };

                        let response_str = serde_json::to_string(&response).unwrap();
                        if let Err(e) = ws_sender.send(tokio_tungstenite::tungstenite::Message::Text(response_str)).await {
                            eprintln!("Error sending message: {}", e);
                            break;
                        }
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
async fn serve_http(mut stream: TcpStream) {
    let body = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark">
<title>ForgeRig</title>
<style>
  body { font-family: sans-serif; max-width: 720px; margin: 2rem auto; padding: 0 1rem; background: #0f1115; color: #e6e6e6; }
  h1 { font-size: 1.4rem; }
  #status { color: #7cf787; font-size: .9rem; }
  #provider { color: #8ab4f8; font-size: .8rem; font-family: monospace; }
  input, pre { width: 100%; box-sizing: border-box; }
  input { padding: .6rem; margin: .5rem 0; font-size: 1rem; background: #1b1f27; color: #e6e6e6; border: 1px solid #333; border-radius: 6px; }
  pre { background: #1b1f27; border-radius: 6px; padding: .8rem; white-space: pre-wrap; word-break: break-word; min-height: 4rem; }
  .hint { color: #777; font-size: .75rem; margin-top: .25rem; }
</style>
</head>
<body>
<div style="display:flex; justify-content:space-between; align-items:center;">
<h1 style="margin:0;">ForgeRig</h1>
<button id="settingsBtn" onclick="openSettings()" style="background:#333;color:#e6e6e6;border:1px solid #555;border-radius:6px;padding:.4rem .7rem;cursor:pointer;">⚙ Settings</button>
</div>
<p id="status">Connecting…</p>
<p id="provider"></p>
<div>
  <input id="prompt" placeholder="Ask, or type !command to run in the container…" autofocus>
  <div class="hint">Messages starting with ! run directly in the container and never reach the model — put auth tokens/secrets here. Use !lean <dst>/<file> after installing the Lean toolchain below.</div>
</div>
<div style="margin-top:.6rem;font-size:.8rem;">
  <span id="leanStatus" style="color:#e6c07b;">Lean: —</span>
  <button id="leanBtn" onclick="provisionLean()" style="background:#333;color:#e6e6e6;border:1px solid #555;border-radius:6px;padding:.3rem .8rem;cursor:pointer;margin-left:.5rem;">Download &amp; install Lean (~550 MB)</button>
</div>
<div id="leanBar" style="display:none;margin:.4rem 0;height:16px;background:#1b1f27;border:1px solid #555;border-radius:8px;overflow:hidden;max-width:420px;"><div id="leanFill" style="height:100%;width:0;background:#3498db;color:#fff;font-size:.65rem;line-height:16px;text-align:center;white-space:nowrap;">0%</div></div>
<pre id="out">Ready.</pre>
<script>
  var ws=null, label=document.getElementById('status'), out=document.getElementById('out'), prov=document.getElementById('provider');
  var pending=null, reqId=0, leanTimer=null, leanPollId=0, leanWasProvisioning=false, POLL_MS=500;
  function connect(){
    label.textContent='Connecting…';
    ws=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/');
    ws.onopen=function(){ label.textContent='Connected to ForgeRig daemon'; send('status',{}); refreshLean(); pollTick(); };
    ws.onclose=function(){ label.textContent='Disconnected — retrying…'; setTimeout(connect,1000); };
    ws.onmessage=function(e){
      var d; try { d=JSON.parse(e.data); } catch(_) { return; }
      if (d.error) { out.textContent='Error: '+d.error.message; return; }
      if (leanPollId && d.id===leanPollId) { leanPollId=0; onLeanPoll(d.result); return; }
      var r=d.result;
      var leanStatus=document.getElementById('leanStatus'), leanBtn=document.getElementById('leanBtn');
      if (pending==='status') { prov.textContent='Provider: '+(r && r.provider ? r.provider : 'unknown'); }
      else if (pending==='exec') {
        var t='';
        if (r && r.stdout) t+=r.stdout;
        if (r && r.stderr) t+='\n[stderr]\n'+r.stderr;
        if (r && (r.exit_code!==0 || r.timed_out)) t+='\n[exit '+r.exit_code+(r.timed_out?' timed out':'')+']';
        out.textContent=t||'(no output)';
      } else if (pending==='lean_status') {
        updateLeanBar(r);
        if (r && (r.provisioning || r.downloading)) {
          // Still working: keep the button hidden and the poll driving updates;
          // never flip to 'not installed' / re-show the button mid-download.
          leanBtn.style.display='none';
          if (!leanTimer) leanTimer=setInterval(pollTick, POLL_MS);
        } else if (r && r.ready) {
          leanStatus.textContent='Lean: ready ('+(r.version||'?')+')'; leanBtn.style.display='none';
        } else {
          leanStatus.textContent='Lean: '+((r && r.message) ? r.message : 'not installed'); leanBtn.disabled=false; leanBtn.style.display='';
        }
      } else if (pending==='lean_provision') {
        leanBtn.style.display='none';
      } else { out.textContent=typeof r==='string' ? r : JSON.stringify(r,null,2); }
      pending=null;
    };
  }
  function refreshLean(){
    if (ws && ws.readyState===1) {
      ws.send(JSON.stringify({jsonrpc:'2.0',method:'lean_status',params:{},id:++reqId}));
      pending='lean_status';
    }
  }
  function pollTick(){
    if (ws && ws.readyState===1) {
      leanPollId=++reqId;
      ws.send(JSON.stringify({jsonrpc:'2.0',method:'lean_progress',params:{},id:leanPollId}));
    }
  }
  function updateLeanBar(r){
    var bar=document.getElementById('leanBar'), fill=document.getElementById('leanFill');
    if (!bar || !fill) return;
    if (r && r.downloading && r.total>0) {
      var pct=Math.floor(r.downloaded*100/r.total);
      bar.style.display='block'; fill.style.width=pct+'%'; fill.textContent=pct+'%';
    } else if (r && !r.ready && (r.downloading || r.provisioning)) {
      bar.style.display='block'; fill.style.width='100%'; fill.textContent='working…';
    } else { bar.style.display='none'; }
  }
  function onLeanPoll(r){
    updateLeanBar(r);
    var lb=document.getElementById('leanBtn');
    if (r && (r.provisioning || r.downloading)) {
      lb.style.display='none';
      leanWasProvisioning=!!r.provisioning;
      if (!leanTimer) leanTimer=setInterval(pollTick, POLL_MS);
      return;
    }
    if (leanTimer) { clearInterval(leanTimer); leanTimer=null; }
    leanWasProvisioning=false;
    refreshLean();
  }
  function provisionLean(){
    if (!ws || ws.readyState!==1) { out.textContent='Not connected to daemon yet.'; return; }
    var leanStatus=document.getElementById('leanStatus'), leanBtn=document.getElementById('leanBtn');
    var bar=document.getElementById('leanBar'), fill=document.getElementById('leanFill');
    leanStatus.textContent='Lean: downloading + installing (~550 MB, may take several minutes)…';
    leanBtn.style.display='none';
    bar.style.display='block'; fill.style.width='100%'; fill.textContent='starting…';
    pending='lean_provision'; reqId++;
    ws.send(JSON.stringify({jsonrpc:'2.0',method:'lean_provision',params:{},id:reqId}));
    if (leanTimer) clearInterval(leanTimer);
    leanWasProvisioning=true;
    leanTimer=setInterval(pollTick, POLL_MS);
  }
  function send(method, params){
    if (!ws || ws.readyState!==1) { out.textContent='Not connected to daemon yet.'; return; }
    pending=method; reqId++;
    ws.send(JSON.stringify({jsonrpc:'2.0',method:method,params:params,id:reqId}));
    if (method!=='status') out.textContent=method==='chat'?'Thinking…':'Running…';
  }
  function openSettings(){
    if (window.NativeHost) { try { window.NativeHost.openSettings(); } catch(e){} }
    else { out.textContent='Configure provider settings in the ForgeRig app.'; }
  }
  document.getElementById('prompt').addEventListener('keydown',function(e){
    if (e.key!=='Enter') return;
    var p=document.getElementById('prompt').value;
    if (!p) return;
    document.getElementById('prompt').value='';
    if (p.charAt(0)==='!') send('exec',{command:p.slice(1).trim()});
    else send('chat',{prompt:p});
  });
  connect();
</script>
</body>
</html>"#;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.as_bytes().len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}