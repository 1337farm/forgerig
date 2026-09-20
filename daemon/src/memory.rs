use tokio_rusqlite::Connection;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct MemoryEngine {
    db: Arc<Connection>,
}

impl MemoryEngine {
    pub async fn new(db_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::open(db_path).await?;

        // Initialize schema
        conn.call(|conn| {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS execution_traces (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                    prompt TEXT NOT NULL,
                    completion TEXT NOT NULL
                )",
                [],
            )?;
            // Team sessions: partition traces per session so sub-agent
            // members never read each other's raw context. The orchestrator
            // aggregates explicitly; the global evaluator keeps working over
            // the unpartitioned view. `IF NOT EXISTS`-style migration: ignore
            // the error when the column already exists on old databases.
            let _ = conn.execute(
                "ALTER TABLE execution_traces ADD COLUMN session_id TEXT NOT NULL DEFAULT ''",
                [],
            );
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_traces_session ON execution_traces(session_id, timestamp)",
                [],
            )?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS macro_memory (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                    milestone TEXT NOT NULL,
                    context TEXT NOT NULL
                )",
                [],
            )?;

            // NetworkPolicy: deny-by-default egress allowlist per project/session.
            // scope: "global" or "session:<id>" or "project:<path>"
            conn.execute(
                "CREATE TABLE IF NOT EXISTS network_policy (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    scope TEXT NOT NULL,
                    domain TEXT NOT NULL,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    UNIQUE(scope, domain)
                )",
                [],
            )?;

            // Audit ledger for gatekeeper verdicts (append-only).
            conn.execute(
                "CREATE TABLE IF NOT EXISTS gatekeeper_audit (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                    tool TEXT NOT NULL,
                    allowed INTEGER NOT NULL,
                    reason TEXT NOT NULL,
                    detail TEXT NOT NULL
                )",
                [],
            )?;

            Ok(())
        }).await?;

        Ok(Self {
            db: Arc::new(conn),
        })
    }

    /// Log one turn, partitioned by session. Each team member's raw context
    /// stays in its own partition; cross-member insight flows only through
    /// explicit merges and the shared macro-memory the evaluator distills.
    pub async fn log_trace(&self, session_id: &str, prompt: &str, completion: &str) -> Result<(), Box<dyn std::error::Error>> {
        let session_str = session_id.to_string();
        let prompt_str = prompt.to_string();
        let completion_str = completion.to_string();

        self.db.call(move |conn| {
            conn.execute(
                "INSERT INTO execution_traces (session_id, prompt, completion) VALUES (?1, ?2, ?3)",
                (&session_str, &prompt_str, &completion_str),
            )?;
            Ok(())
        }).await?;
        Ok(())
    }

    pub async fn get_recent_traces(&self, limit: usize) -> Result<Vec<(i64, String, String)>, Box<dyn std::error::Error>> {
        self.db.call(move |conn| {
            let mut stmt = conn.prepare("SELECT id, prompt, completion FROM execution_traces ORDER BY timestamp DESC LIMIT ?")?;
            let mut rows = stmt.query([limit as i64])?;

            let mut traces = Vec::new();
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let prompt: String = row.get(1)?;
                let completion: String = row.get(2)?;
                traces.push((id, prompt, completion));
            }
            // Reverse so they are in chronological order
            traces.reverse();
            Ok(traces)
        }).await.map_err(|e| e.into())
    }

    /// Chronological (oldest-first) traces for one team member.
    pub async fn get_recent_traces_for_session(&self, session_id: &str, limit: usize) -> Result<Vec<(i64, String, String)>, Box<dyn std::error::Error>> {
        let session_str = session_id.to_string();
        self.db.call(move |conn| {
            let mut stmt = conn.prepare("SELECT id, prompt, completion FROM execution_traces WHERE session_id = ? ORDER BY timestamp DESC LIMIT ?")?;
            let mut rows = stmt.query(rusqlite::params![session_str, limit as i64])?;

            let mut traces = Vec::new();
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let prompt: String = row.get(1)?;
                let completion: String = row.get(2)?;
                traces.push((id, prompt, completion));
            }
            // Reverse so they are in chronological order
            traces.reverse();
            Ok(traces)
        }).await.map_err(|e| e.into())
    }

    pub async fn prune_traces(&self, ids: Vec<i64>) -> Result<(), Box<dyn std::error::Error>> {
        if ids.is_empty() {
            return Ok(());
        }
        self.db.call(move |conn| {
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM execution_traces WHERE id IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            stmt.execute(rusqlite::params_from_iter(ids.iter()))?;
            Ok(())
        }).await.map_err(|e| e.into())
    }

    /// NetworkPolicy: check if a domain is allowed for a given scope.
    pub async fn is_domain_allowed(&self, scope: &str, domain: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let scope = scope.to_string();
        let domain = domain.to_string();
        self.db.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT 1 FROM network_policy WHERE scope = ?1 AND domain = ?2"
            )?;
            let mut rows = stmt.query([&scope, &domain])?;
            Ok(rows.next()?.is_some())
        }).await.map_err(|e| e.into())
    }

    /// Add a domain to the allowlist for a scope.
    pub async fn allow_domain(&self, scope: &str, domain: &str) -> Result<(), Box<dyn std::error::Error>> {
        let scope = scope.to_string();
        let domain = domain.to_string();
        self.db.call(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO network_policy (scope, domain) VALUES (?1, ?2)",
                [&scope, &domain],
            )?;
            Ok(())
        }).await.map_err(|e| e.into())
    }

    /// Remove a domain from the allowlist for a scope.
    pub async fn deny_domain(&self, scope: &str, domain: &str) -> Result<(), Box<dyn std::error::Error>> {
        let scope = scope.to_string();
        let domain = domain.to_string();
        self.db.call(move |conn| {
            conn.execute(
                "DELETE FROM network_policy WHERE scope = ?1 AND domain = ?2",
                [&scope, &domain],
            )?;
            Ok(())
        }).await.map_err(|e| e.into())
    }

    /// List all allowed domains for a scope.
    pub async fn list_allowed_domains(&self, scope: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let scope = scope.to_string();
        self.db.call(move |conn| {
            let mut stmt = conn.prepare("SELECT domain FROM network_policy WHERE scope = ?1 ORDER BY domain")?;
            let mut rows = stmt.query([&scope])?;
            let mut domains = Vec::new();
            while let Some(row) = rows.next()? {
                domains.push(row.get(0)?);
            }
            Ok(domains)
        }).await.map_err(|e| e.into())
    }

    /// Chronological (oldest-first) list of prior milestones (permanent memories).
    pub async fn get_macro_memories(&self, limit: usize) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
        self.db.call(move |conn| {
            let mut stmt = conn.prepare("SELECT milestone, context FROM macro_memory ORDER BY timestamp DESC LIMIT ?")?;
            let mut rows = stmt.query([limit as i64])?;

            let mut mems = Vec::new();
            while let Some(row) = rows.next()? {
                let milestone: String = row.get(0)?;
                let context: String = row.get(1)?;
                mems.push((milestone, context));
            }
            mems.reverse();
            Ok(mems)
        }).await.map_err(|e| e.into())
    }

    /// Log a gatekeeper verdict to the persistent audit trail.
    pub async fn log_gatekeeper_verdict(&self, tool: &str, allowed: bool, reason: &str, detail: &str) -> Result<(), Box<dyn std::error::Error>> {
        let tool = tool.to_string();
        let allowed = allowed.to_string();
        let reason = reason.to_string();
        let detail = detail.to_string();
        self.db.call(move |conn| {
            conn.execute(
                "INSERT INTO gatekeeper_audit (tool, allowed, reason, detail) VALUES (?1, ?2, ?3, ?4)",
                [&tool, &allowed, &reason, &detail],
            )?;
            Ok(())
        }).await.map_err(|e| e.into())
    }

    /// Get recent gatekeeper audit entries.
    pub async fn get_gatekeeper_audit(&self, limit: usize) -> Result<Vec<(String, bool, String, String)>, Box<dyn std::error::Error>> {
        self.db.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT tool, allowed, reason, detail FROM gatekeeper_audit ORDER BY timestamp DESC LIMIT ?"
            )?;
            let mut rows = stmt.query([limit as i64])?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next()? {
                let tool: String = row.get(0)?;
                let allowed: String = row.get(1)?;
                let reason: String = row.get(2)?;
                let detail: String = row.get(3)?;
                entries.push((tool, allowed == "true", reason, detail));
            }
            entries.reverse();
            Ok(entries)
        }).await.map_err(|e| e.into())
    }

    /// Also append to AGENTS.md in the current working directory (the work
    /// root when running in-guest, or the daemon CWD host-side).
    pub async fn log_macro_memory(&self, milestone: &str, context: &str) -> Result<(), Box<dyn std::error::Error>> {
        let milestone_str = milestone.to_string();
        let context_str = context.to_string();

        {
            let (m, c) = (milestone_str.clone(), context_str.clone());
            self.db.call(move |conn| {
                conn.execute(
                    "INSERT INTO macro_memory (milestone, context) VALUES (?1, ?2)",
                    (&m, &c),
                )?;
                Ok(())
            }).await?;
        }

        let append_content = format!("\n## Milestone: {}\n\n{}\n", milestone_str, context_str);
        match std::fs::read_to_string("AGENTS.md") {
            Ok(existing) if !existing.contains(&milestone_str) => write_agents_append(append_content)?,
            Ok(_) => {}
            Err(_) => write_agents_append(append_content)?,
        }

        Ok(())
    }
}

fn write_agents_append(content: String) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("AGENTS.md")?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

#[derive(Serialize, Deserialize, Debug, JsonSchema)]
pub struct EvaluationResult {
    pub milestone_reached: bool,
    pub milestone_summary: Option<String>,
    pub noisy_trace_ids: Vec<i64>,
}

/// Evaluate recent traces, prune noise, and record milestone memory.
pub async fn evaluate_and_process(
    backend: &Arc<crate::provider::Backend>,
    memory_engine: &Arc<MemoryEngine>,
    traces: Vec<(i64, String, String)>,
) -> Result<(), String> {
    if traces.is_empty() {
        return Ok(());
    }

    let mut trace_text = String::new();
    for (id, prompt, completion) in &traces {
        trace_text.push_str(&format!("Trace ID: {}\nPrompt: {}\nCompletion: {}\n\n", id, prompt, completion));
    }

    let evaluation_prompt = format!(
        "Evaluate the following execution traces and determine if a major milestone has been reached.\n\
         If a milestone has been reached, provide a summary of the architectural decisions made.\n\
         Identify any trace IDs that are noisy terminal retries, compilation noise, or failures that can be safely pruned.\n\n\
         Traces:\n{}",
        trace_text
    );

    let result = backend.evaluate(&evaluation_prompt).await?;

    if !result.noisy_trace_ids.is_empty() {
        if let Err(e) = memory_engine.prune_traces(result.noisy_trace_ids.clone()).await {
            eprintln!("Failed to prune traces: {}", e);
        }
    }

    if result.milestone_reached {
        if let Some(summary) = &result.milestone_summary {
            let milestone_title = format!("Milestone at {}", chrono::Utc::now().to_rfc3339());
            if let Err(e) = memory_engine.log_macro_memory(&milestone_title, summary).await {
                eprintln!("Failed to log macro memory: {}", e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traces_partition_by_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let engine = MemoryEngine::new(db_path.to_str().unwrap()).await.unwrap();

        engine.log_trace("s1", "p1", "c1").await.unwrap();
        engine.log_trace("s2", "p2", "c2").await.unwrap();
        engine.log_trace("s1", "p3", "c3").await.unwrap();

        let s1 = engine.get_recent_traces_for_session("s1", 10).await.unwrap();
        assert_eq!(s1.len(), 2);
        assert_eq!(s1[0].1, "p1");
        assert_eq!(s1[1].1, "p3");
        let s2 = engine.get_recent_traces_for_session("s2", 10).await.unwrap();
        assert_eq!(s2.len(), 1);
        // Global evaluator view still sees everything.
        assert_eq!(engine.get_recent_traces(10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_network_policy_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let engine = MemoryEngine::new(db_path.to_str().unwrap()).await.unwrap();

        // Initially not allowed
        assert!(!engine.is_domain_allowed("global", "example.com").await.unwrap());

        // Allow it
        engine.allow_domain("global", "example.com").await.unwrap();
        assert!(engine.is_domain_allowed("global", "example.com").await.unwrap());

        // Different scope not affected
        assert!(!engine.is_domain_allowed("session:123", "example.com").await.unwrap());
    }
}