//! Provider resolution plus a provider-erased chat/eval backend.
//!
//! Almost every target provider speaks the OpenAI chat-completions protocol, so
//! they share one rig client (`openai::Client::from_url`). Google Gemini is the
//! exception — its OpenAI-compat path diverges from rig's fixed
//! `/v1/chat/completions` — so it uses the native client.
//!
//! Free-first model defaults are chosen per provider and every model is
//! overridable via env.

use rig::agent::AgentBuilder;
use rig::completion::{self, CompletionModel, CompletionRequest, CompletionResponse, ModelChoice, Prompt};
use rig::extractor::{Extractor, ExtractorBuilder};
use rig::providers::gemini;
use serde_json::json;

use crate::memory::EvaluationResult;
use crate::lean::LeanExecutor;
use crate::tools::BashExecutor;
use crate::wasm::WasmTransformer;

const SYSTEM_PREAMBLE: &str = "\
You are an autonomous orchestrator daemon running in a Linux userland inside an \
Android app. You have tools to run bash, transform WASM, and type-check Lean \
theorem-prover sources. Be concise and action-oriented.";

/// Upper bound on a single chat completion (rig's HTTP client has no timeout).
const CHAT_TIMEOUT_SECS: u64 = 300;

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
            Provider::Mistral => Some("https://api.mistral.ai/v1"),
            Provider::Ollama => Some("http://localhost:11434"),
            Provider::Gemini | Provider::Custom => None,
        }
    }

    fn default_chat_model(self) -> &'static str {
        match self {
            Provider::OpenAi => "gpt-4o-mini",
            Provider::OpenRouter => "meta-llama/llama-3.3-70b-instruct:free",
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
    /// True when a real key (not the fallback "dummy-key") was configured.
    key_present: bool,
    provider: Provider,
    chat_model: String,
}

enum BackendKind {
    Compat {
        agent: rig::agent::Agent<LoggedOpenAiModel>,
        eval: Extractor<LoggedOpenAiModel, EvaluationResult>,
    },
    Gemini {
        agent: rig::agent::Agent<gemini::completion::CompletionModel>,
        eval: Extractor<gemini::completion::CompletionModel, EvaluationResult>,
    },
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
}

impl LoggedOpenAiModel {
    fn new(http: reqwest::Client, base_url: String, api_key: String, model: String) -> Self {
        Self { http, base_url, api_key, model }
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

        let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));
        let request_json = serde_json::to_string(&body).map_err(completion::CompletionError::JsonError)?;
        eprintln!("chat http: -> POST {url} (model={})", self.model);
        eprintln!("chat http: request {request_json}");

        let t0 = std::time::Instant::now();
        let send_result = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            // Force a fresh connection per request. The shared reqwest pool
            // otherwise reuses a keep-alive socket that NVIDIA's load balancer
            // may have half-closed, which hangs the *second* call until our
            // timeout ("operation timed out") — the classic first-works-
            // second-hangs symptom.
            .header("Connection", "close")
            .json(&body)
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

        let v: serde_json::Value = serde_json::from_str(&text).map_err(completion::CompletionError::JsonError)?;
        let choices = v
            .get("choices")
            .and_then(|c| c.as_array())
            .ok_or_else(|| completion::CompletionError::ResponseError("response had no choices".into()))?;
        let first = choices
            .first()
            .ok_or_else(|| completion::CompletionError::ResponseError("response had empty choices".into()))?;
        let message = first.get("message");

        if let Some(calls) = message.and_then(|m| m.get("tool_calls")).and_then(|t| t.as_array()) {
            if let Some(call) = calls.first() {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| completion::CompletionError::ResponseError("tool call missing name".into()))?
                    .to_string();
                let args = call.get("function").and_then(|f| f.get("arguments")).cloned().unwrap_or(json!({}));
                let args = match args {
                    serde_json::Value::String(s) => {
                        serde_json::from_str::<serde_json::Value>(&s).unwrap_or(json!({}))
                    }
                    other => other,
                };
                return Ok(CompletionResponse {
                    choice: ModelChoice::ToolCall(name, args),
                    raw_response: v,
                });
            }
        }
        // Fall back to a text message, ignoring leading/trailing whitespace.
        // Reasoning models (deepseek-v4-flash etc.) return a whitespace-only
        // "content" next to tool_calls — the tool call above must win, else a
        // follow-up that triggers a tool answers with a blank 2-char reply.
        if let Some(content) = message.and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
            let content = content.trim();
            if !content.is_empty() {
                return Ok(CompletionResponse {
                    choice: ModelChoice::Message(content.to_string()),
                    raw_response: v,
                });
            }
        }
        Err(completion::CompletionError::ResponseError(
            "response did not contain a message or tool call".into(),
        ))
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
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

impl Backend {
    pub fn resolve() -> Backend {
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
            let agent = client
                .agent(&chat_model)
                .preamble(SYSTEM_PREAMBLE)
                .tool(BashExecutor::default())
                .tool(WasmTransformer::default())
                .tool(LeanExecutor::default())
                .build();
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
            let chat = LoggedOpenAiModel::new(http.clone(), base.to_string(), key.clone(), chat_model.clone());
            let agent = AgentBuilder::new(chat)
                .preamble(SYSTEM_PREAMBLE)
                .temperature(0.7)
                .tool(BashExecutor::default())
                .tool(WasmTransformer::default())
                .tool(LeanExecutor::default())
                .build();
            let eval_model_impl = LoggedOpenAiModel::new(http, base.to_string(), key.clone(), eval_model.clone());
            let eval: Extractor<LoggedOpenAiModel, EvaluationResult> = ExtractorBuilder::new(eval_model_impl).build();
            BackendKind::Compat { agent, eval }
        };

        Backend { kind, key_present, provider, chat_model }
    }

    pub async fn chat(&self, prompt: &str) -> Result<String, String> {
        // The compat model bounds its HTTP request with a 15s connect + 180s
        // read timeout (see LoggedOpenAiModel); this outer cap is a coarse
        // safety net for the whole agent prompt, including a possible tool call.
        let fut = match &self.kind {
            BackendKind::Compat { agent, .. } => futures_util::future::Either::Left(agent.prompt(prompt)),
            BackendKind::Gemini { agent, .. } => futures_util::future::Either::Right(agent.prompt(prompt)),
        };
        match tokio::time::timeout(std::time::Duration::from_secs(CHAT_TIMEOUT_SECS), fut).await {
            Ok(res) => res.map_err(|e| e.to_string()),
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
        format!(
            "provider={} model={} key={}",
            self.provider.name(),
            self.chat_model,
            if self.key_present { "set" } else { "missing" }
        )
    }
}