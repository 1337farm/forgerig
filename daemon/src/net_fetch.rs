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

/// Extract the host that the NetworkPolicy must be asked about.
///
/// Split out from `call` so the scheme/host rules are unit-testable without a
/// database or a socket — this is the only thing standing between a
/// model-supplied string and the allowlist, so the parsing must be done by a
/// real URL parser and never by string splitting. `https://github.com@evil.com/`
/// is the case that punishes a naive split: its host is `evil.com`, not
/// `github.com`.
fn https_host(url: &str) -> Result<String, NetFetchError> {
    let url = url.trim();
    if !url.starts_with("https://") {
        return Err(NetFetchError::NotAllowed("only https:// URLs allowed".to_string()));
    }
    let parsed = url::Url::parse(url)
        .map_err(|_| NetFetchError::NotAllowed("invalid URL".to_string()))?;
    // Reject a port-less-but-scheme-relative or schemeless parse the prefix
    // check let through by accident.
    if parsed.scheme() != "https" {
        return Err(NetFetchError::NotAllowed("only https:// URLs allowed".to_string()));
    }
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| NetFetchError::NotAllowed("no host in URL".to_string()))
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
        // Scheme + host extraction, then the deny-by-default policy check.
        let url = args.url.trim();
        let host = match https_host(url) {
            Ok(h) => h,
            Err(e) => {
                gatekeeper::log_verdict("net_fetch", false, &e.to_string(), url);
                return Err(e);
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

        // Read with size cap. The SHA (when provided) is always verified
        // against the FULL response body; truncation for model context
        // happens afterwards so a truncated hash can never pass verification.
        let max_bytes = args.max_bytes.min(MAX_FETCH_BYTES);
        let bytes = resp.bytes().await.map_err(|e| NetFetchError::Network(e.to_string()))?;

        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        let total = bytes.len();
        let computed_sha = hex::encode(hasher.finalize());

        // Optional SHA verification (full body).
        if let Some(expected) = args.sha256 {
            if !expected.eq_ignore_ascii_case(&computed_sha) {
                gatekeeper::log_verdict("net_fetch", false, "sha-mismatch", &format!("expected {} got {}", expected, computed_sha));
                let _ = self.memory.log_gatekeeper_verdict("net_fetch", false, "sha-mismatch", url).await;
                return Err(NetFetchError::Checksum);
            }
        }

        let (capped, capped_truncated) = if bytes.len() > max_bytes {
            gatekeeper::log_verdict("net_fetch", true, "size-capped", &format!("{} > {}", bytes.len(), max_bytes));
            let _ = self.memory.log_gatekeeper_verdict("net_fetch", true, "size-capped", url).await;
            (&bytes[..max_bytes], true)
        } else {
            (&bytes[..], false)
        };

        // Truncate for model context.
        let (body_str, model_truncated) = gatekeeper::truncate_output(&String::from_utf8_lossy(capped));
        let truncated = capped_truncated || model_truncated;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Only https is brokered. Everything else is refused before any socket
    /// or policy lookup, so a non-https URL can never reach an allowlisted
    /// domain over a weaker transport.
    #[test]
    fn only_https_is_accepted() {
        for bad in [
            "http://github.com/",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com/",
            "//github.com/x",
            "javascript:alert(1)",
            "",
        ] {
            assert!(https_host(bad).is_err(), "accepted {bad:?}");
        }
        assert!(https_host("https://github.com/").is_ok());
    }

    /// The allowlist is keyed on the parsed host, so a userinfo trick must not
    /// launder an attacker host as an allowlisted one.
    #[test]
    fn host_is_parsed_not_split() {
        // Naive splitting on '/' or '@' would yield "github.com" here.
        assert_eq!(https_host("https://github.com@evil.com/").unwrap(), "evil.com");
        assert_eq!(https_host("https://user:pw@evil.com/x").unwrap(), "evil.com");
        // Subdomains are their own host, not the parent.
        assert_eq!(https_host("https://api.github.com/v1").unwrap(), "api.github.com");
        // A lookalike parent domain must not match on a suffix.
        assert_eq!(https_host("https://notgithub.com/").unwrap(), "notgithub.com");
        // Explicit ports don't leak into the host key.
        assert_eq!(https_host("https://github.com:8443/x").unwrap(), "github.com");
    }

    /// Surrounding whitespace is a realistic model output; it must be trimmed
    /// rather than rejected.
    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(https_host("  https://github.com/x \n").unwrap(), "github.com");
    }

    #[test]
    fn empty_host_forms_are_refused() {
        for bad in ["https://", "https://:8443/x"] {
            assert!(https_host(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// `https:///just/a/path` is normalized by the URL spec to host `just`
    /// (the extra slash collapses), not treated as an empty authority. That is
    /// fail-closed either way, but it must not be *silently* accepted as a
    /// hostless URL, so the resolved host is what the allowlist sees.
    #[test]
    fn over_slashed_url_resolves_to_a_real_host() {
        assert_eq!(https_host("https:///just/a/path").unwrap(), "just");
    }
}