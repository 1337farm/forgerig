//! Provider resolution plus a provider-erased chat/eval backend.
//!
//! Almost every target provider speaks the OpenAI chat-completions protocol, so
//! they share one rig client (`openai::Client::from_url`). Google Gemini is the
//! exception — its OpenAI-compat path diverges from rig's fixed
//! `/v1/chat/completions` — so it uses the native client.
//!
//! Free-first model defaults are chosen per provider and every model is
//! overridable via env.

use rig::completion::Prompt;
use rig::extractor::Extractor;
use rig::providers::{gemini, openai};

use crate::memory::EvaluationResult;
use crate::tools::BashExecutor;
use crate::wasm::WasmTransformer;

const SYSTEM_PREAMBLE: &str = "\
You are an autonomous orchestrator daemon running in a Linux userland inside an \
Android app. You have tools to run bash and transform WASM. Be concise and \
action-oriented.";

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
            "custom" | "opencode" => Provider::Custom,
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
            Provider::Nvidia => "meta/llama-3.3-70b-instruct",
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
            Provider::Nvidia => "meta/llama-3.1-8b-instruct",
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
        agent: rig::agent::Agent<openai::CompletionModel>,
        eval: Extractor<openai::CompletionModel, EvaluationResult>,
    },
    Gemini {
        agent: rig::agent::Agent<gemini::completion::CompletionModel>,
        eval: Extractor<gemini::completion::CompletionModel, EvaluationResult>,
    },
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
                .build();
            let eval = client.extractor::<EvaluationResult>(&eval_model).build();
            BackendKind::Gemini { agent, eval }
        } else {
            let base = base_url.as_deref().unwrap_or("https://api.openai.com");
            eprintln!("provider={} base_url={} model={} eval_model={}", provider.name(), base, chat_model, eval_model);
            let client = openai::Client::from_url(&key, base);
            let agent = client
                .agent(&chat_model)
                .preamble(SYSTEM_PREAMBLE)
                .tool(BashExecutor::default())
                .tool(WasmTransformer::default())
                .build();
            let eval = client.extractor::<EvaluationResult>(&eval_model).build();
            BackendKind::Compat { agent, eval }
        };

        Backend { kind, key_present, provider, chat_model }
    }

    pub async fn chat(&self, prompt: &str) -> Result<String, String> {
        match &self.kind {
            BackendKind::Compat { agent, .. } => agent.prompt(prompt).await.map_err(|e| e.to_string()),
            BackendKind::Gemini { agent, .. } => agent.prompt(prompt).await.map_err(|e| e.to_string()),
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