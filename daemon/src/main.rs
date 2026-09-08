use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use futures_util::{StreamExt, SinkExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use rig::providers::openai::Client;
use rig::completion::Prompt;
use tools::BashExecutor;
use wasm::WasmTransformer;
use memory::{MemoryEngine, HeuristicEvaluator, EvaluationResult};

mod tools;
mod wasm;
mod memory;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("127.0.0.1:{}", port);

    let openai_api_key = env::var("OPENAI_API_KEY").unwrap_or_else(|_| "dummy-key".to_string());
    let openai_client = Client::new(&openai_api_key);

    let agent = openai_client
        .agent("gpt-4")
        .preamble("You are an autonomous orchestrator daemon running on a Linux userland.")
        .tool(BashExecutor::default())
        .tool(WasmTransformer::default())
        .build();
    let agent = Arc::new(agent);

    let memory_engine = Arc::new(MemoryEngine::new("oss_memory.db").await?);

    // Initialize the Heuristic Evaluator
    let evaluator = Arc::new(HeuristicEvaluator::new(
        openai_client.extractor::<EvaluationResult>("gpt-4").build()
    ));

    println!("Listening on: {}", addr);
    let listener = TcpListener::bind(&addr).await?;

    while let Ok((stream, _)) = listener.accept().await {
        let agent = Arc::clone(&agent);
        let memory = Arc::clone(&memory_engine);
        let evaluator = Arc::clone(&evaluator);

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
                        println!("Received: {}", text);

                        let response = match serde_json::from_str::<RpcRequest>(text) {
                            Ok(req) => {
                                println!("Parsed JSON-RPC request: {:?}", req);

                                if req.method == "chat" {
                                    if let Some(params) = &req.params {
                                        if let Some(prompt) = params.get("prompt").and_then(|p| p.as_str()) {
                                            match agent.prompt(prompt).await {
                                                Ok(completion) => {
                                                    let completion_val = serde_json::json!(completion);
                                                    if let Err(e) = memory.log_trace(prompt, &completion_val).await {
                                                        eprintln!("Failed to log trace to memory: {}", e);
                                                    }

                                                    // Periodically evaluate traces instead of every single prompt
                                                    // For now, we can check a simple mod condition on a static or passed counter,
                                                    // but to avoid global mutable state we evaluate if the recent traces count is > 0
                                                    // and we limit evaluating to avoid spamming the LLM
                                                    let memory_clone = Arc::clone(&memory);
                                                    let evaluator_clone = Arc::clone(&evaluator);
                                                    tokio::spawn(async move {
                                                        let trace_count_res = memory_clone.get_recent_traces(1).await.map_err(|e| e.to_string());
                                                        if let Ok(recent) = trace_count_res {
                                                            if !recent.is_empty() && recent[0].0 % 5 == 0 {
                                                                let traces_result = memory_clone.get_recent_traces(10).await.map_err(|e| e.to_string());
                                                                match traces_result {
                                                                    Ok(traces) => {
                                                                        if let Err(e) = evaluator_clone.evaluate_and_process(memory_clone, traces).await {
                                                                            eprintln!("Background evaluation failed: {}", e);
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        eprintln!("Failed to get recent traces for evaluation: {}", e);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    });

                                                    RpcResponse {
                                                        jsonrpc: "2.0".to_string(),
                                                        result: Some(completion_val),
                                                        error: None,
                                                        id: req.id,
                                                    }
                                                }
                                                Err(e) => {
                                                    RpcResponse {
                                                        jsonrpc: "2.0".to_string(),
                                                        result: None,
                                                        error: Some(RpcError {
                                                            code: -32603,
                                                            message: format!("Agent error: {}", e),
                                                        }),
                                                        id: req.id,
                                                    }
                                                }
                                            }
                                        } else {
                                            RpcResponse {
                                                jsonrpc: "2.0".to_string(),
                                                result: None,
                                                error: Some(RpcError {
                                                    code: -32602,
                                                    message: "Missing 'prompt' in params".to_string(),
                                                }),
                                                id: req.id,
                                            }
                                        }
                                    } else {
                                        RpcResponse {
                                            jsonrpc: "2.0".to_string(),
                                            result: None,
                                            error: Some(RpcError {
                                                code: -32602,
                                                message: "Missing params".to_string(),
                                            }),
                                            id: req.id,
                                        }
                                    }
                                } else {
                                    RpcResponse {
                                        jsonrpc: "2.0".to_string(),
                                        result: None,
                                        error: Some(RpcError {
                                            code: -32601,
                                            message: "Method not found".to_string(),
                                        }),
                                        id: req.id,
                                    }
                                }
                            }
                            Err(e) => {
                                println!("Failed to parse JSON-RPC: {}", e);
                                RpcResponse {
                                    jsonrpc: "2.0".to_string(),
                                    result: None,
                                    error: Some(RpcError {
                                        code: -32700,
                                        message: "Parse error".to_string(),
                                    }),
                                    id: None,
                                }
                            }
                        };

                        let response_str = serde_json::to_string(&response).unwrap();
                        if let Err(e) = ws_sender.send(tokio_tungstenite::tungstenite::Message::Text(response_str)).await {
                            println!("Error sending message: {}", e);
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
/// opens the WebSocket on the same port for JSON-RPC `chat`.
async fn serve_http(mut stream: TcpStream) {
    let body = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>ForgeRig</title>
<style>
  body { font-family: sans-serif; max-width: 640px; margin: 2rem auto; padding: 0 1rem; background: #0f1115; color: #e6e6e6; }
  h1 { font-size: 1.4rem; }
  #status { color: #7cf787; }
  input, pre { width: 100%; box-sizing: border-box; }
  input { padding: .6rem; margin: .5rem 0; font-size: 1rem; }
  pre { background: #1b1f27; border-radius: 6px; padding: .8rem; white-space: pre-wrap; word-break: break-word; min-height: 4rem; }
</style>
</head>
<body>
<h1>ForgeRig</h1>
<p id="status">Connecting…</p>
<input id="prompt" placeholder="Ask the orchestrator daemon…" autofocus>
<pre id="out">Ready.</pre>
<script>
  var ws=null, label=document.getElementById('status'), out=document.getElementById('out');
  function connect(){
    label.textContent='Connecting…';
    ws=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/');
    ws.onopen=function(){ label.textContent='Connected to ForgeRig daemon'; };
    ws.onclose=function(){ label.textContent='Disconnected — retrying…'; setTimeout(connect,1000); };
    ws.onmessage=function(e){
      var d;
      try { d=JSON.parse(e.data); } catch(_) { return; }
      if (d.result) out.textContent=JSON.stringify(d.result,null,2);
      else if (d.error) out.textContent='Error: '+d.error.message;
    };
  }
  document.getElementById('prompt').addEventListener('keydown',function(e){
    if (e.key==='Enter') send();
  });
  function send(){
    var p=document.getElementById('prompt').value;
    if (!p) return;
    if (!ws || ws.readyState!==1) { out.textContent='Not connected to daemon yet.'; return; }
    ws.send(JSON.stringify({jsonrpc:'2.0',method:'chat',params:{prompt:p},id:1}));
    out.textContent='Thinking…';
  }
  connect();
</script>
</body>
</html>"#;

    let status_line = "HTTP/1.1 200 OK\r\n";
    let headers = "Content-Type: text/html; charset=utf-8\r\nConnection: close\r\n";
    let response = format!(
        "{}\r\n{}\r\nContent-Length: {}\r\n\r\n{}",
        status_line, headers, body.as_bytes().len(), body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}