//! Provider resolution plus a provider-erased chat/eval backend.
//!
//! Almost every target provider speaks the OpenAI chat-completions protocol, so
//! they share one rig client (`openai::Client::from_url`). Google Gemini is the
//! exception — its OpenAI-compat path diverges from rig's fixed
//! `/v1/chat/completions` — so it uses the native client.
//!
//! Free-first model defaults are chosen per provider and every model is
//! overridable via env.

use rig::completion::{self, CompletionModel, CompletionRequest, CompletionResponse, ModelChoice, Prompt};
use rig::extractor::{Extractor, ExtractorBuilder};
use rig::providers::gemini;
use rig::tool::{Tool, ToolSet};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::memory::{EvaluationResult, MemoryEngine};
use crate::lean::LeanExecutor;
use crate::tools::BashExecutor;
use crate::tools::CodeIngest;
use crate::wasm::WasmTransformer;
use crate::net_fetch::NetFetchTool;

/// Events emitted while a chat completion streams. The daemon forwards these
/// over the WebSocket as `chat_chunk` / `chat_tool` / `chat_review`
/// notifications so the UI paints tokens and progress markers as they arrive
/// instead of waiting for the full reply.
/// Cooperative cancellation handle for an in-flight generation. Stop sets
/// the flag; the streaming loop polls it on every chunk and between tool
/// calls, drops the provider socket, and returns `Stopped` so no completion
/// is persisted and no chat_done is emitted. Cutting the loop early also
/// stops token spend at the provider.
pub type StopFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// Error returned when the user stops a generation mid-flight.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped by user")
    }
}

impl From<Stopped> for completion::CompletionError {
    fn from(_: Stopped) -> Self {
        completion::CompletionError::ResponseError("stopped by user".into())
    }
}

fn is_stopped(stop: &StopFlag) -> bool {
    stop.load(std::sync::atomic::Ordering::SeqCst)
}

/// Events emitted while a chat completion streams. The daemon forwards these
/// over the WebSocket as `chat_chunk` / `chat_tool` / `chat_review`
/// notifications so the UI paints tokens and progress markers as they arrive
/// instead of waiting for the full reply.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A slice of assistant text.
    TextDelta(String),
    /// A thinking/reviewing/research phase marker.
    Phase(PhaseEvent),
    /// A tool call started (arguments still streaming or complete).
    ToolStart(String),
    /// A tool call finished; String is a truncated result preview.
    ToolResult(String),
    /// The generation was stopped: the client must tear down the thinking
    /// bubble and mark the turn stopped (never a completion).
    Stopped,
}

#[derive(Debug, Clone)]
pub enum PhaseEvent {
    /// The model is thinking / reasoning about the answer.
    Thinking,
    /// The model is reviewing / critiquing its previous answer.
    Reviewing,
    /// The model is researching / looking up info (via tools).
    Researching,
    /// The model has finished its reasoning and is ready to emit the answer.
    Ready,
}

impl StreamEvent {
    /// Render as a JSON-RPC notification for the wire.
    pub fn into_rpc(self, session_id: &str) -> Value {
        match self {
            StreamEvent::TextDelta(delta) => json!({
                "jsonrpc": "2.0", "method": "chat_chunk",
                "params": { "session_id": session_id, "delta": delta },
            }),
            StreamEvent::Phase(phase) => {
                let (phase_name, note) = match &phase {
                    PhaseEvent::Thinking => ("thinking", ""),
                    PhaseEvent::Reviewing => ("reviewing", ""),
                    PhaseEvent::Researching => ("researching", ""),
                    PhaseEvent::Ready => ("ready", ""),
                };
                json!({
                    "jsonrpc": "2.0", "method": "chat_phase",
                    "params": { "session_id": session_id, "phase": phase_name, "note": note },
                })
            }
            StreamEvent::ToolStart(name) => json!({
                "jsonrpc": "2.0", "method": "chat_tool",
                "params": { "session_id": session_id, "phase": "start", "name": name },
            }),
            StreamEvent::ToolResult(preview) => json!({
                "jsonrpc": "2.0", "method": "chat_tool",
                "params": { "session_id": session_id, "phase": "result", "preview": preview },
            }),
            StreamEvent::Stopped => json!({
                "jsonrpc": "2.0", "method": "chat_stopped",
                "params": { "session_id": session_id },
            }),
        }
    }
}

/// One parsed SSE `data:` payload from an OpenAI-compat stream.
#[derive(Debug, Default)]
struct SseDelta {
    content: String,
    tool_calls: Vec<ToolCallDelta>,
    finish: bool,
}

#[derive(Debug, Default, Clone)]
struct ToolCallDelta {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

/// Parse a single SSE line. Returns (done, delta): done=true on `[DONE]`.
fn parse_sse_line(line: &str) -> Option<(bool, SseDelta)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some((true, SseDelta::default()));
    }
    let v: Value = serde_json::from_str(data).ok()?;
    let mut delta = SseDelta::default();
    let choice = v.get("choices")?.as_array()?.first()?;
    if choice.get("finish_reason").and_then(|r| r.as_str()).is_some() {
        delta.finish = true;
    }
    let d = choice.get("delta")?;
    if let Some(text) = d.get("content").and_then(|c| c.as_str()) {
        delta.content = text.to_string();
    }
    if let Some(calls) = d.get("tool_calls").and_then(|c| c.as_array()) {
        for call in calls {
            let index = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let id = call.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
            let f = call.get("function");
            delta.tool_calls.push(ToolCallDelta {
                index,
                id,
                name: f.and_then(|x| x.get("name")).and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                arguments: f.and_then(|x| x.get("arguments")).and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            });
        }
    }
    Some((false, delta))
}

/// Split newly-arrived SSE bytes into complete lines, keeping the tail.
fn feed_sse_lines(buffer: &mut String, bytes: &[u8]) -> Vec<String> {
    buffer.push_str(&String::from_utf8_lossy(bytes));
    let mut lines = Vec::new();
    while let Some(pos) = buffer.find('\n') {
        lines.push(buffer[..pos].to_string());
        buffer.drain(..=pos);
    }
    lines
}

const SYSTEM_PREAMBLE: &str = "\
You are an autonomous orchestrator daemon running in a Linux userland inside an \
Android app. You have tools to run bash, transform WASM, and type-check Lean \
theorem-prover sources. Be concise and action-oriented. Format replies as Markdown. \
Formatting contract (the client parses this output, so follow it exactly): use \
fenced code blocks with a language tag for all code, inline code spans for \
identifiers and paths, GFM tables for tabular data, short paragraphs, and no \
filler. Prefer the smallest correct reply: fewer tokens is faster for everyone.";

/// Upper bound on a single chat completion (rig's HTTP client has no timeout).
const CHAT_TIMEOUT_SECS: u64 = 300;

/// The system message seeded into every session thread.
pub fn system_message() -> serde_json::Value {
    json!({ "role": "system", "content": SYSTEM_PREAMBLE })
}

/// Hard cap on the number of tool→reply rounds before we stop rather than loop.
const MAX_TOOL_TURNS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    OpenRouter,
    Nvidia,
    Groq,
    DeepSeek,
    Mistral,
    Gemini,
    Ollama,
    Custom,
}

impl Provider {
    fn from_env() -> Provider {
        match std::env::var("FORGERIG_PROVIDER")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "openrouter" => Provider::OpenRouter,
            "nvidia" | "nim" | "nvidia_nim" => Provider::Nvidia,
            "groq" => Provider::Groq,
            "deepseek" => Provider::DeepSeek,
            "mistral" | "leanstral" => Provider::Mistral,
            "gemini" | "google" => Provider::Gemini,
            "ollama" => Provider::Ollama,
            "custom" => Provider::Custom,
            _ => Provider::OpenAi,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::OpenRouter => "openrouter",
            Provider::Nvidia => "nvidia",
            Provider::Groq => "groq",
            Provider::DeepSeek => "deepseek",
            Provider::Mistral => "mistral",
            Provider::Gemini => "gemini",
            Provider::Ollama => "ollama",
            Provider::Custom => "custom",
        }
    }

    fn default_base_url(self) -> Option<&'static str> {
        match self {
            Provider::OpenAi => Some("https://api.openai.com"),
            Provider::OpenRouter => Some("https://openrouter.ai/api"),
            Provider::Nvidia => Some("https://integrate.api.nvidia.com"),
            Provider::Groq => Some("https://api.groq.com/openai"),
            Provider::DeepSeek => Some("https://api.deepseek.com"),
            Provider::Mistral => Some("https://api.mistral.ai"),
            Provider::Ollama => Some("http://localhost:11434"),
            Provider::Gemini | Provider::Custom => None,
        }
    }

    fn default_chat_model(self) -> &'static str {
        match self {
            Provider::OpenAi => "gpt-4o-mini",
            // `openrouter/auto` always resolves (the curated `:free` slugs
            // rot out of the catalog); it routes near the low cost band by
            // default and supports tool calling like any selected model.
            Provider::OpenRouter => "openrouter/auto",
            Provider::Nvidia => "nvidia/llama-3.1-nemotron-70b-instruct",
            Provider::Groq => "llama-3.3-70b-versatile",
            Provider::DeepSeek => "deepseek-chat",
            Provider::Mistral => "mistral-small-latest",
            Provider::Gemini => "gemini-2.5-flash",
            Provider::Ollama => "llama3.1:8b",
            Provider::Custom => "gpt-4o-mini",
        }
    }

    fn default_eval_model(self) -> &'static str {
        match self {
            Provider::Nvidia => "nvidia/llama-3.1-nemotron-51b-instruct",
            Provider::Groq => "llama-3.1-8b-instant",
            Provider::Gemini => "gemini-2.5-flash",
            Provider::Ollama => "llama3.1:8b",
            other => other.default_chat_model(),
        }
    }

    fn key_env(self) -> Option<&'static str> {
        match self {
            Provider::OpenAi => Some("OPENAI_API_KEY"),
            Provider::OpenRouter => Some("OPENROUTER_API_KEY"),
            Provider::Nvidia => Some("NVIDIA_API_KEY"),
            Provider::Groq => Some("GROQ_API_KEY"),
            Provider::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Provider::Mistral => Some("MISTRAL_API_KEY"),
            Provider::Gemini => Some("GEMINI_API_KEY"),
            Provider::Ollama => None,
            Provider::Custom => Some("FORGERIG_API_KEY"),
        }
    }
}

pub struct Backend {
    kind: BackendKind,
    /// Provider enum for dynamic key resolution.
    provider: Provider,
    chat_model: String,
}

enum BackendKind {
    Compat {
        model: LoggedOpenAiModel,
        tools: ToolSet,
        tool_defs: Vec<serde_json::Value>,
        eval: Extractor<LoggedOpenAiModel, EvaluationResult>,
    },
    Gemini {
        agent: rig::agent::Agent<gemini::completion::CompletionModel>,
        eval: Extractor<gemini::completion::CompletionModel, EvaluationResult>,
    },
}

/// A single tool invocation requested by the model (id + name + raw JSON args).
#[derive(Debug, Clone)]
struct ToolCallMsg {
    id: String,
    name: String,
    arguments: String,
}

/// OpenAI-chat-completions completion model that owns its HTTP request. This
/// replaces rig's `openai::Client::from_url` for the compat path so we can:
///
/// 1. Bound every request with a real connect/read timeout — rig's client has
///    none, so a stale keep-alive socket (the classic "first prompt works,
///    second hangs") blocked until our coarse outer cap (300s) kicked in.
/// 2. Log the full request payload, response payload, HTTP status, and any
///    server error body to the shared install log for on-device debugging.
///
/// The API key is only ever sent in the Authorization header and is never
/// logged.
#[derive(Clone)]
struct LoggedOpenAiModel {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Extra headers (e.g. OpenRouter attribution). Never logged.
    extra_headers: Vec<(String, String)>,
}

/// Join a provider base URL to the OpenAI-compatible chat path without
/// doubling a trailing `/v1` (the Mistral `/v1/v1/...` bug).
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/v1/chat/completions", base_url.trim_end_matches('/'))
}

/// Attribution headers OpenRouter uses for app ranking/discoverability.
/// Only sent to OpenRouter; other OpenAI-compatible providers ignore them.
fn openrouter_headers() -> Vec<(String, String)> {
    vec![
        ("HTTP-Referer".to_string(), "https://github.com/1337farm/forgerig".to_string()),
        ("X-Title".to_string(), "ForgeRig".to_string()),
    ]
}

impl LoggedOpenAiModel {
    fn new(http: reqwest::Client, base_url: String, api_key: String, model: String) -> Self {
        Self { http, base_url, api_key, model, extra_headers: Vec::new() }
    }

    fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// POST that returns the raw streaming response (caller reads SSE).
    /// Same logging/timeouts as post_chat; the body must set stream:true.
    async fn stream_post(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, completion::CompletionError> {
        let url = chat_completions_url(&self.base_url);
        let request_json = serde_json::to_string(body).map_err(completion::CompletionError::JsonError)?;
        eprintln!("chat http: -> POST {url} (model={}, stream)", self.model);
        eprintln!("chat http: request {request_json}");
        let t0 = std::time::Instant::now();
        let mut req = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Connection", "close")
            .header("Accept", "text/event-stream");
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        let resp = req
            .json(body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("chat http: stream send failed after {:.2}s: {e}", t0.elapsed().as_secs_f64());
                return Err(completion::CompletionError::HttpError(e));
            }
        };
        let status = resp.status();
        eprintln!("chat http: stream <- {status} (headers in {:.2}s)", t0.elapsed().as_secs_f64());
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!("chat http: stream server error body: {text}");
            return Err(completion::CompletionError::ProviderError(text));
        }
        Ok(resp)
    }

    /// One POST to /v1/chat/completions with full logging and parsed JSON.
    async fn post_chat(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, completion::CompletionError> {
        let url = chat_completions_url(&self.base_url);
        let request_json = serde_json::to_string(body).map_err(completion::CompletionError::JsonError)?;
        eprintln!("chat http: -> POST {url} (model={})", self.model);
        eprintln!("chat http: request {request_json}");

        let t0 = std::time::Instant::now();
        let mut req = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            // Force a fresh connection per request. The shared reqwest pool
            // otherwise reuses a keep-alive socket that NVIDIA's load balancer
            // may have half-closed, which hangs the *second* call until our
            // timeout ("operation timed out") — the classic first-works-
            // second-hangs symptom.
            .header("Connection", "close");
        for (k, v) in &self.extra_headers {
            req = req.header(k, v);
        }
        let send_result = req
            .json(body)
            .send()
            .await;
        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("chat http: send failed after {:.2}s: {e}", t0.elapsed().as_secs_f64());
                return Err(completion::CompletionError::HttpError(e));
            }
        };
        let status = resp.status();
        eprintln!("chat http: <- {status} (headers in {:.2}s)", t0.elapsed().as_secs_f64());
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("chat http: body read failed after {:.2}s: {e}", t0.elapsed().as_secs_f64());
                return Err(completion::CompletionError::HttpError(e));
            }
        };
        eprintln!("chat http: body done in {:.2}s ({} bytes)", t0.elapsed().as_secs_f64(), text.len());
        if !status.is_success() {
            eprintln!("chat http: server error body: {text}");
            return Err(completion::CompletionError::ProviderError(text));
        }
        eprintln!("chat http: response {text}");
        serde_json::from_str(&text).map_err(completion::CompletionError::JsonError)
    }

    /// Parse an OpenAI chat-completions response into (trimmed text, tool calls).
    fn parse_turn(
        v: &serde_json::Value,
    ) -> Result<(Option<String>, Vec<ToolCallMsg>), completion::CompletionError> {
        let choices = v
            .get("choices")
            .and_then(|c| c.as_array())
            .ok_or_else(|| completion::CompletionError::ResponseError("response had no choices".into()))?;
        let first = choices
            .first()
            .ok_or_else(|| completion::CompletionError::ResponseError("response had empty choices".into()))?;
        let message = first.get("message");

        let mut tool_calls = vec![];
        if let Some(calls) = message.and_then(|m| m.get("tool_calls")).and_then(|t| t.as_array()) {
            for call in calls {
                let id = call.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
                let f = call.get("function");
                let name = f
                    .and_then(|x| x.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                let arguments = f
                    .and_then(|x| x.get("arguments"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("{}")
                    .to_string();
                tool_calls.push(ToolCallMsg { id, name, arguments });
            }
        }

        // Reasoning models can return whitespace-only content next to tool_calls;
        // callers decide precedence. Trim so blank text yields None.
        let content = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Ok((content, tool_calls))
    }
}

impl CompletionModel for LoggedOpenAiModel {
    type Response = serde_json::Value;

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<serde_json::Value>, completion::CompletionError> {
        let mut messages = vec![];
        if let Some(preamble) = &request.preamble {
            messages.push(json!({ "role": "system", "content": preamble }));
        }
        for m in &request.chat_history {
            messages.push(json!({ "role": m.role, "content": m.content }));
        }
        // rig's prompt_with_context() is pub(crate); reproduce it here. Our
        // agents never attach documents, so this is normally just `request.prompt`.
        let user_content = if request.documents.is_empty() {
            request.prompt.clone()
        } else {
            let docs = request
                .documents
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("");
            format!("<attachments>\n{}</attachments>\n\n{}", docs, request.prompt)
        };
        messages.push(json!({ "role": "user", "content": user_content }));

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("messages".into(), json!(messages));
        if let Some(t) = request.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(mt) = request.max_tokens {
            body.insert("max_tokens".into(), json!(mt));
        }
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body.insert("tools".into(), json!(tools));
            body.insert("tool_choice".into(), json!("auto"));
        }
        if let Some(extra) = request.additional_params {
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        let body = serde_json::Value::Object(body);
        let v = self.post_chat(&body).await?;

        let (content, mut tool_calls) = Self::parse_turn(&v)?;
        // Single-shot consumer (rig's Extractor): a submit tool call wins, else
        // the text. Prefer the tool call even if content is also present.
        if let Some(call) = tool_calls.drain(..).next() {
            let args = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| json!({}));
            return Ok(CompletionResponse {
                choice: ModelChoice::ToolCall(call.name, args),
                raw_response: v,
            });
        }
        if let Some(content) = content {
            return Ok(CompletionResponse {
                choice: ModelChoice::Message(content),
                raw_response: v,
            });
        }
        Err(completion::CompletionError::ResponseError(
            "response did not contain a message or tool call".into(),
        ))
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Optional per-completion output cap from Settings (blank or invalid =
/// provider default). Read once per Backend::resolve.
fn max_tokens_limit() -> Option<u64> {
    std::env::var("FORGERIG_MAX_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

fn resolve_key(provider: Provider) -> String {
    if let Ok(k) = std::env::var("FORGERIG_API_KEY") {
        if !k.is_empty() {
            return k;
        }
    }
    match provider.key_env() {
        Some(env_key) => std::env::var(env_key).unwrap_or_else(|_| "dummy-key".to_string()),
        None => "ollama".to_string(),
    }
}

async fn run_agent_loop(
    model: &LoggedOpenAiModel,
    tools: &ToolSet,
    tool_defs: &[serde_json::Value],
    messages: &mut Vec<serde_json::Value>,
    prompt: &str,
) -> Result<String, completion::CompletionError> {
    // `messages` starts as [system, ...history]; append the new user turn.
    messages.push(json!({ "role": "user", "content": prompt }));
    for turn in 0..MAX_TOOL_TURNS {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(model.model));
        body.insert("messages".into(), json!(messages));
        body.insert("temperature".into(), json!(0.7));
        if let Some(mt) = max_tokens_limit() {
            body.insert("max_tokens".into(), json!(mt));
        }
        if !tool_defs.is_empty() {
            body.insert("tools".into(), json!(tool_defs));
            body.insert("tool_choice".into(), json!("auto"));
        }
        let v = model.post_chat(&serde_json::Value::Object(body)).await?;
        let (content, tool_calls) = LoggedOpenAiModel::parse_turn(&v)?;

        if tool_calls.is_empty() {
            // Persist the final assistant message into the thread.
            if let Some(message) = v
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
                .and_then(|m| m.get("message"))
            {
                messages.push(message.clone());
            }
            return Ok(content.unwrap_or_default());
        }

        // Keep the model's assistant message (with its tool_calls) verbatim.
        if let Some(message) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|m| m.get("message"))
        {
            messages.push(message.clone());
        }
        // Run every requested tool and feed each result back for the next turn.
        for tc in &tool_calls {
            let result = match tools.call(&tc.name, tc.arguments.clone()).await {
                Ok(r) => r,
                Err(e) => format!("tool error: {e}"),
            };
            eprintln!("chat tool: {} -> {:.240}", tc.name, result);
            messages.push(json!({ "role": "tool", "tool_call_id": tc.id, "content": result }));
        }
        eprintln!("chat tool: ran {} tool call(s) on turn {turn}, asking the model to answer", tool_calls.len());
    }
    Err(completion::CompletionError::ResponseError(format!(
        "agent did not finish within {MAX_TOOL_TURNS} tool turns"
    )))
}

/// Streaming twin of run_agent_loop: same tool loop, but assistant text and
/// tool activity flow out as StreamEvents while the model is still talking.
/// On stream-open failure it falls back to one non-streaming turn so a
/// half-broken SSE path degrades to today's behavior, not an error.
async fn run_agent_loop_streaming(
    model: &LoggedOpenAiModel,
    tools: &ToolSet,
    tool_defs: &[serde_json::Value],
    messages: &mut Vec<serde_json::Value>,
    _prompt: &str,
    emit: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    stop: &StopFlag,
) -> Result<String, completion::CompletionError> {
    use futures_util::StreamExt as _;
    let emit_ev = |ev: StreamEvent| {
        let _ = emit.send(ev);
    };
    // `messages` already ends with the new user turn (committed by the
    // caller at send time so the tab exists before the provider is
    // contacted). Do NOT push again — a duplicate user turn confuses the
    // model and wastes tokens.
    emit_ev(StreamEvent::Phase(PhaseEvent::Thinking));
    for _turn in 0..MAX_TOOL_TURNS {
        if is_stopped(stop) {
            emit_ev(StreamEvent::Stopped);
            return Err(Stopped.into());
        }
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(model.model));
        body.insert("messages".into(), json!(messages));
        body.insert("temperature".into(), json!(0.7));
        body.insert("stream".into(), json!(true));
        if let Some(mt) = max_tokens_limit() {
            body.insert("max_tokens".into(), json!(mt));
        }
        if !tool_defs.is_empty() {
            body.insert("tools".into(), json!(tool_defs));
            body.insert("tool_choice".into(), json!("auto"));
        }
        let body = serde_json::Value::Object(body);

        let resp = match model.stream_post(&body).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("chat stream: falling back to non-streaming turn ({e:?})");
                let out = run_agent_loop_once(model, tools, tool_defs, messages).await?;
                emit_ev(StreamEvent::Phase(PhaseEvent::Ready));
                emit_ev(StreamEvent::TextDelta(out.clone()));
                return Ok(out);
            }
        };
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut text = String::new();
        let mut builders: Vec<ToolCallDelta> = Vec::new();
        let mut ended = false;
        while !ended {
            if is_stopped(stop) {
                drop(stream);
                emit_ev(StreamEvent::Stopped);
                return Err(Stopped.into());
            }
            let chunk = stream.next().await;
            let bytes = match chunk {
                Some(Ok(b)) => b,
                Some(Err(e)) => return Err(completion::CompletionError::HttpError(e)),
                None => break,
            };
            for line in feed_sse_lines(&mut buf, &bytes) {
                let Some((done, delta)) = parse_sse_line(&line) else { continue };
                if !delta.content.is_empty() {
                    text.push_str(&delta.content);
                    emit_ev(StreamEvent::TextDelta(delta.content));
                }
                for tc in delta.tool_calls {
                    while builders.len() <= tc.index {
                        builders.push(ToolCallDelta::default());
                    }
                    let b = &mut builders[tc.index];
                    b.index = tc.index;
                    if !tc.id.is_empty() {
                        b.id = tc.id;
                    }
                    if !tc.name.is_empty() {
                        b.name = tc.name;
                    }
                    b.arguments.push_str(&tc.arguments);
                }
                if done || delta.finish {
                    ended = true;
                    break;
                }
            }
        }
        let calls: Vec<ToolCallDelta> = builders.into_iter().filter(|b| !b.name.is_empty()).collect();
        if calls.is_empty() {
            emit_ev(StreamEvent::Phase(PhaseEvent::Ready));
            messages.push(json!({ "role": "assistant", "content": text }));
            return Ok(text);
        }
        // Tool turn: the model is researching (calling tools). Emit a phase
        // marker so the UI can show a "researching" status instead of a
        // silent pause between chunks.
        emit_ev(StreamEvent::Phase(PhaseEvent::Researching));
        // replay the assistant tool_calls message for coherence,
        // run every requested tool, stream start/result markers, continue.
        let wire_calls: Vec<Value> = calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        messages.push(json!({ "role": "assistant", "tool_calls": wire_calls }));
        for tc in &calls {
            if is_stopped(stop) {
                emit_ev(StreamEvent::Stopped);
                return Err(Stopped.into());
            }
            emit_ev(StreamEvent::ToolStart(tc.name.clone()));
            let result = match tools.call(&tc.name, tc.arguments.clone()).await {
                Ok(r) => r,
                Err(e) => format!("tool error: {e}"),
            };
            eprintln!("chat tool: {} -> {:.240}", tc.name, result);
            let preview: String = result.chars().take(240).collect();
            emit_ev(StreamEvent::ToolResult(preview));
            messages.push(json!({ "role": "tool", "tool_call_id": tc.id, "content": result }));
        }
        emit_ev(StreamEvent::Phase(PhaseEvent::Reviewing));
        eprintln!("chat tool: streaming loop continues after tool turn");
    }
    Err(completion::CompletionError::ResponseError(format!(
        "agent did not finish within {MAX_TOOL_TURNS} tool turns"
    )))
}

/// One non-streaming turn used as the SSE fallback: sends the current thread
/// once and runs any requested tools a single time (no follow-up turn).
async fn run_agent_loop_once(
    model: &LoggedOpenAiModel,
    tools: &ToolSet,
    tool_defs: &[serde_json::Value],
    messages: &mut Vec<serde_json::Value>,
) -> Result<String, completion::CompletionError> {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(model.model));
    body.insert("messages".into(), json!(messages));
    body.insert("temperature".into(), json!(0.7));
    if let Some(mt) = max_tokens_limit() {
        body.insert("max_tokens".into(), json!(mt));
    }
    if !tool_defs.is_empty() {
        body.insert("tools".into(), json!(tool_defs));
        body.insert("tool_choice".into(), json!("auto"));
    }
    let v = model.post_chat(&serde_json::Value::Object(body)).await?;
    let (content, tool_calls) = LoggedOpenAiModel::parse_turn(&v)?;
    if let Some(message) = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|m| m.get("message"))
    {
        messages.push(message.clone());
    }
    for tc in &tool_calls {
        let result = match tools.call(&tc.name, tc.arguments.clone()).await {
            Ok(r) => r,
            Err(e) => format!("tool error: {e}"),
        };
        messages.push(json!({ "role": "tool", "tool_call_id": tc.id, "content": result }));
    }
    Ok(content.unwrap_or_default())
}

impl Backend {
    pub async fn resolve(memory: Arc<MemoryEngine>) -> Self {
        let provider = Provider::from_env();
        let key = resolve_key(provider);
        let key_present = !key.is_empty() && key != "dummy-key";
        let base_url = std::env::var("FORGERIG_BASE_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| provider.default_base_url().map(|s| s.to_string()));
        let chat_model = env_or("FORGERIG_MODEL", provider.default_chat_model());
        let eval_model = env_or("FORGERIG_EVAL_MODEL", provider.default_eval_model());

        let kind = if provider == Provider::Gemini {
            let client = gemini::Client::new(&key);
            let builder = client
                .agent(&chat_model)
                .preamble(SYSTEM_PREAMBLE)
                .tool(BashExecutor::default())
                .tool(WasmTransformer::default())
                .tool(LeanExecutor::default())
                .tool(CodeIngest::default())
                .tool(NetFetchTool::new(memory.clone(), "global".to_string()));
            let agent = match max_tokens_limit() {
                Some(mt) => builder.max_tokens(mt).build(),
                None => builder.build(),
            };
            let eval = client.extractor::<EvaluationResult>(&eval_model).build();
            BackendKind::Gemini { agent, eval }
        } else {
            let base = base_url.as_deref().unwrap_or("https://api.openai.com");
            eprintln!("provider={} base_url={} model={} eval_model={}", provider.name(), base, chat_model, eval_model);
            // Own the HTTP client so we can set connect/read timeouts and log
            // full payloads (see LoggedOpenAiModel).
            let http = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .expect("reqwest client should build");
            let extra = if provider == Provider::OpenRouter {
                openrouter_headers()
            } else {
                Vec::new()
            };
            let model = LoggedOpenAiModel::new(http.clone(), base.to_string(), key.clone(), chat_model.clone())
                .with_headers(extra.clone());
            let eval: Extractor<LoggedOpenAiModel, EvaluationResult> =
                ExtractorBuilder::new(
                    LoggedOpenAiModel::new(http, base.to_string(), key.clone(), eval_model.clone())
                        .with_headers(extra),
                )
                .build();

            let mut tools = ToolSet::default();
            tools.add_tool(BashExecutor::default());
            tools.add_tool(WasmTransformer::default());
            tools.add_tool(LeanExecutor::default());
            tools.add_tool(CodeIngest::default());
            tools.add_tool(NetFetchTool::new(memory.clone(), "global".to_string()));

            let bash_def = BashExecutor::default().definition(String::new()).await;
            let wasm_def = WasmTransformer::default().definition(String::new()).await;
            let lean_def = LeanExecutor::default().definition(String::new()).await;
            let ingest_def = CodeIngest::default().definition(String::new()).await;
            let net_fetch_def = NetFetchTool::new(memory.clone(), "global".to_string()).definition(String::new()).await;
            let tool_defs: Vec<serde_json::Value> = [bash_def, wasm_def, lean_def, ingest_def, net_fetch_def]
                .into_iter()
                .map(|d| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": d.name.clone(),
                            "description": d.description.clone(),
                            "parameters": d.parameters.clone(),
                        }
                    })
                })
                .collect();

            BackendKind::Compat { model, tools, tool_defs, eval }
        };

        Self { kind, provider, chat_model }
    }

    pub async fn chat_session(
        &self,
        messages: &mut Vec<serde_json::Value>,
        prompt: &str,
    ) -> Result<String, String> {        // The compat model bounds each HTTP round-trip with a 15s connect +
        // 180s read timeout (see LoggedOpenAiModel); this outer cap bounds the
        // whole tool loop (up to MAX_TOOL_TURNS round-trips).
        let fut = match &self.kind {
            BackendKind::Compat { model, tools, tool_defs, .. } => {
                futures_util::future::Either::Left(async move {
                    run_agent_loop(model, tools, tool_defs, messages, prompt).await.map_err(|e| e.to_string())
                })
            }
            BackendKind::Gemini { agent, .. } => {
                // Gemini has no exposed multi-turn history; flatten the prior
                // user/assistant turns into a single transcript.
                let transcript = messages
                    .iter()
                    .filter_map(|m| match (m.get("role").and_then(|r| r.as_str()), m.get("content").and_then(|c| c.as_str())) {
                        (Some("user"), Some(c)) => Some(format!("User: {c}")),
                        (Some("assistant"), Some(c)) => Some(format!("Assistant: {c}")),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                futures_util::future::Either::Right(async move {
                    let full: std::borrow::Cow<'_, str> = if transcript.is_empty() {
                        std::borrow::Cow::Borrowed(prompt)
                    } else {
                        std::borrow::Cow::Owned(format!("{transcript}\nUser: {prompt}"))
                    };
                    agent.prompt(&full).await.map_err(|e| e.to_string())
                })
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(CHAT_TIMEOUT_SECS), fut).await {
            Ok(res) => res,
            Err(_) => Err(format!("chat timed out after {}s", CHAT_TIMEOUT_SECS)),
        }
    }

    /// Streaming twin of chat_session: tokens and tool activity flow out as
    /// StreamEvents while the model is still talking. Gemini has no streaming
    /// path here, so it resolves as one silent turn (client paints it whole).
    pub async fn chat_session_streaming(
        &self,
        messages: &mut Vec<serde_json::Value>,
        prompt: &str,
        emit: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        stop: &StopFlag,
    ) -> Result<String, String> {
        let fut = match &self.kind {
            BackendKind::Compat { model, tools, tool_defs, .. } => {
                futures_util::future::Either::Left(async move {
                    run_agent_loop_streaming(model, tools, tool_defs, messages, prompt, emit, stop)
                        .await
                        .map_err(|e| e.to_string())
                })
            }
            BackendKind::Gemini { agent, .. } => {
                let transcript = messages
                    .iter()
                    .filter_map(|m| match (m.get("role").and_then(|r| r.as_str()), m.get("content").and_then(|c| c.as_str())) {
                        (Some("user"), Some(c)) => Some(format!("User: {c}")),
                        (Some("assistant"), Some(c)) => Some(format!("Assistant: {c}")),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                futures_util::future::Either::Right(async move {
                    let full: std::borrow::Cow<'_, str> = if transcript.is_empty() {
                        std::borrow::Cow::Borrowed(prompt)
                    } else {
                        std::borrow::Cow::Owned(format!("{transcript}\nUser: {prompt}"))
                    };
                    agent.prompt(&full).await.map_err(|e| e.to_string())
                })
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(CHAT_TIMEOUT_SECS), fut).await {
            Ok(res) => res,
            Err(_) => Err(format!("chat timed out after {}s", CHAT_TIMEOUT_SECS)),
        }
    }

    pub async fn evaluate(&self, prompt: &str) -> Result<EvaluationResult, String> {
        match &self.kind {
            BackendKind::Compat { eval, .. } => eval.extract(prompt).await.map_err(|e| e.to_string()),
            BackendKind::Gemini { eval, .. } => eval.extract(prompt).await.map_err(|e| e.to_string()),
        }
    }

    pub fn describe(&self) -> String {
        // Re-read the key dynamically so key status updates without restart
        let key = resolve_key(self.provider);
        let key_present = !key.is_empty() && key != "dummy-key";
        format!(
            "provider={} model={} key={}",
            self.provider.name(),
            self.chat_model,
            if key_present { "set" } else { "missing" }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_text_delta_parses() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let (done, d) = parse_sse_line(line).unwrap();
        assert!(!done);
        assert_eq!(d.content, "Hello");
        assert!(!d.finish);
    }

    #[test]
    fn sse_done_and_finish_flag() {
        assert!(parse_sse_line("data: [DONE]").unwrap().0);
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let (done, d) = parse_sse_line(line).unwrap();
        assert!(!done);
        assert!(d.finish);
    }

    #[test]
    fn sse_tool_call_delta_parses_with_index() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash_executor","arguments":"{\"com"}}]}}]}"#;
        let (_, d) = parse_sse_line(line).unwrap();
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0].name, "bash_executor");
        assert_eq!(d.tool_calls[0].id, "call_1");
    }

    #[test]
    fn sse_comments_and_blanks_skipped() {
        assert!(parse_sse_line(": ping").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line("event: message").is_none());
    }

    #[test]
    fn feed_sse_lines_keeps_partial_tail() {
        let mut buf = String::new();
        let lines = feed_sse_lines(&mut buf, b"data: {\"a\":1}\npartial");
        assert_eq!(lines, vec!["data: {\"a\":1}".to_string()]);
        let lines2 = feed_sse_lines(&mut buf, b"-tail\n");
        assert_eq!(lines2, vec!["partial-tail".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn stream_event_renders_rpc_notifications() {
        let v = StreamEvent::TextDelta("hi".into()).into_rpc("s1");
        assert_eq!(v["method"], json!("chat_chunk"));
        assert_eq!(v["params"]["delta"], json!("hi"));
        let v = StreamEvent::ToolStart("bash_executor".into()).into_rpc("s1");
        assert_eq!(v["method"], json!("chat_tool"));
        assert_eq!(v["params"]["phase"], json!("start"));
        let v = StreamEvent::Stopped.into_rpc("s1");
        assert_eq!(v["method"], json!("chat_stopped"));
    }

    #[test]
    fn stop_flag_starts_clear() {
        let flag: StopFlag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(!is_stopped(&flag));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(is_stopped(&flag));
    }

    #[test]
    fn chat_url_never_doubles_api_version() {
        // Regression: the Mistral base once ended in `/v1`, producing
        // `.../v1/v1/chat/completions` on every chat call.
        assert_eq!(
            chat_completions_url("https://api.mistral.ai"),
            "https://api.mistral.ai/v1/chat/completions"
        );
        for provider in [
            Provider::OpenAi,
            Provider::OpenRouter,
            Provider::Nvidia,
            Provider::Groq,
            Provider::DeepSeek,
            Provider::Mistral,
            Provider::Ollama,
        ] {
            let base = provider.default_base_url().unwrap();
            let url = chat_completions_url(base);
            assert!(
                !url.contains("/v1/v1/"),
                "{base} joined to {url} doubles the version"
            );
        }
    }

    #[test]
    fn openrouter_default_model_always_resolves() {
        // Curated `:free` slugs rot out of the catalog (404); the auto
        // router slug is stable and routes near the low cost band.
        assert_eq!(Provider::OpenRouter.default_chat_model(), "openrouter/auto");
    }

    #[test]
    fn openrouter_headers_carry_attribution() {
        let headers = openrouter_headers();
        assert!(headers.iter().any(|(k, v)| k == "HTTP-Referer" && v.contains("forgerig")));
        assert!(headers.iter().any(|(k, v)| k == "X-Title" && !v.is_empty()));
    }
}
