//! Brokered network fetch — the ONLY egress path for model-driven code.
//!
//! All sandboxed tools (bash_executor, lean_executor, etc.) are denied raw
/// socket access. If the model needs to download something, it calls
/// `net_fetch` with a URL. The daemon checks the NetworkPolicy (deny-by-default
/// allowlist), fetches on the host side (with SSL_CERT_DIR already set), verifies
/// size + optional SHA-256, and returns the body (truncated to 2 MiB).

use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use crate::memory::MemoryEngine;
use crate::gatekeeper;
use std::sync::Arc;
use sha2::Digest;

const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
const FETCH_TIMEOUT_SECS: u64 = 30;

#[derive(Error, Debug)]
pub enum NetFetchError {
    #[error("domain not allowed by policy: {0}")]
    NotAllowed(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("size mismatch: expected {0} got {1}")]
    SizeMismatch(u64, u64),
    #[error("sha256 mismatch")]
    Checksum,
    #[error("network error: {0}")]
    Network(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetFetchArgs {
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_max_bytes() -> usize {
    MAX_FETCH_BYTES
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetFetchResult {
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
    pub truncated: bool,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NetFetchTool {
    memory: Arc<MemoryEngine>,
    scope: String, // "global" or "session:<id>"
}

impl NetFetchTool {
    pub fn new(memory: Arc<MemoryEngine>, scope: String) -> Self {
        Self { memory, scope }
    }
}

impl Tool for NetFetchTool {
    const NAME: &'static str = "net_fetch";

    type Error = NetFetchError;
    type Args = NetFetchArgs;
    type Output = NetFetchResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Fetch a URL via the brokered network path. The domain must be allowlisted in the NetworkPolicy for the current scope. Returns the response body (truncated to 2 MiB).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "HTTPS URL to fetch (must be allowlisted)"
                    },
                    "sha256": {
                        "type": "string",
                        "description": "Optional expected SHA-256 of the body (hex). Mismatch = error."
                    },
                    "max_bytes": {
                        "type": "number",
                        "description": "Max bytes to read (default 2 MiB, max 2 MiB)"
                    }
                },
                "required": ["url"]
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // Validate URL scheme and extract host.
        let url = args.url.trim();
        if !url.starts_with("https://") {
            gatekeeper::log_verdict("net_fetch", false, "non-https-url", url);
            return Err(NetFetchError::NotAllowed("only https:// URLs allowed".to_string()));
        }

        let parsed = match url::Url::parse(url) {
            Ok(u) => u,
            Err(_) => {
                gatekeeper::log_verdict("net_fetch", false, "invalid-url", url);
                return Err(NetFetchError::NotAllowed("invalid URL".to_string()));
            }
        };

        let host = match parsed.host_str() {
            Some(h) => h.to_string(),
            None => {
                gatekeeper::log_verdict("net_fetch", false, "no-host", url);
                return Err(NetFetchError::NotAllowed("no host in URL".to_string()));
            }
        };

        // Policy check: deny-by-default.
        let allowed = self.memory
            .is_domain_allowed(&self.scope, &host)
            .await
            .map_err(|e| NetFetchError::Network(e.to_string()))?;

        if !allowed {
            gatekeeper::log_verdict("net_fetch", false, "domain-not-allowed", &host);
            // Also log to persistent audit.
            let _ = self.memory.log_gatekeeper_verdict("net_fetch", false, "domain-not-allowed", &host).await;
            return Err(NetFetchError::NotAllowed(format!("domain not allowed: {host}")));
        }

        // Fetch with size cap and timeout.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| NetFetchError::Network(e.to_string()))?;

        let resp = client.get(url)
            .send()
            .await
            .map_err(|e| NetFetchError::Http(e.to_string()))?;

        let status = resp.status().as_u16();
        let content_type = resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !resp.status().is_success() {
            gatekeeper::log_verdict("net_fetch", false, &format!("http-{}", status), url);
            let _ = self.memory.log_gatekeeper_verdict("net_fetch", false, &format!("http-{}", status), url).await;
            return Err(NetFetchError::Http(format!("HTTP {}", status)));
        }

        // Read with size cap.
        let max_bytes = args.max_bytes.min(MAX_FETCH_BYTES);
        let bytes = resp.bytes().await.map_err(|e| NetFetchError::Network(e.to_string()))?;

        if bytes.len() > max_bytes {
            gatekeeper::log_verdict("net_fetch", false, "size-exceeded", &format!("{} > {}", bytes.len(), max_bytes));
            let _ = self.memory.log_gatekeeper_verdict("net_fetch", false, "size-exceeded", url).await;
            return Err(NetFetchError::SizeMismatch(max_bytes as u64, bytes.len() as u64));
        }

        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        let total = bytes.len();
        let computed_sha = hex::encode(hasher.finalize());

        // Optional SHA verification.
        if let Some(expected) = args.sha256 {
            if !expected.eq_ignore_ascii_case(&computed_sha) {
                gatekeeper::log_verdict("net_fetch", false, "sha-mismatch", &format!("expected {} got {}", expected, computed_sha));
                let _ = self.memory.log_gatekeeper_verdict("net_fetch", false, "sha-mismatch", url).await;
                return Err(NetFetchError::Checksum);
            }
        }

        // Truncate for model context.
        let (body_str, truncated) = gatekeeper::truncate_output(&String::from_utf8_lossy(&bytes));
        let (body_str, _) = gatekeeper::scrub_secrets(&body_str);

        gatekeeper::log_verdict("net_fetch", true, &format!("http-{}", status), &format!("{} bytes", total));
        let _ = self.memory.log_gatekeeper_verdict("net_fetch", true, &format!("http-{}", status), url).await;

        Ok(NetFetchResult {
            url: url.to_string(),
            status,
            content_type,
            body: body_str,
            truncated,
            sha256: Some(computed_sha),
        })
    }
}

// Extension trait for convenience.
pub trait NetFetchExt {
    fn net_fetch_tool(&self, scope: String) -> NetFetchTool;
}

impl NetFetchExt for Arc<MemoryEngine> {
    fn net_fetch_tool(&self, scope: String) -> NetFetchTool {
        NetFetchTool::new(self.clone(), scope)
    }
}