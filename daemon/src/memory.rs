use tokio_rusqlite::Connection;
use std::sync::Arc;

#[derive(Clone)]
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

            conn.execute(
                "CREATE TABLE IF NOT EXISTS macro_memory (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
                    milestone TEXT NOT NULL,
                    context TEXT NOT NULL
                )",
                [],
            )?;
            Ok(())
        }).await?;

        Ok(Self {
            db: Arc::new(conn),
        })
    }

    pub async fn log_trace(&self, prompt: &str, completion: &str) -> Result<(), Box<dyn std::error::Error>> {
        let prompt_str = prompt.to_string();
        let completion_str = completion.to_string();

        self.db.call(move |conn| {
            conn.execute(
                "INSERT INTO execution_traces (prompt, completion) VALUES (?1, ?2)",
                (&prompt_str, &completion_str),
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

        // Also append to AGENTS.md in the current working directory (the work
        // root when running in-guest, or the daemon CWD host-side).
        let append_content = format!("\n## Milestone: {}\n\n{}\n", milestone_str, context_str);
        match std::fs::read_to_string("AGENTS.md") {
            Ok(existing) if !existing.contains(&milestone_str) => write_agents_append(append_content)?,
            Ok(_) => {}
            Err(_) => write_agents_append(append_content)?,
        }

        Ok(())
    }

    /// Chronological (oldest-first) list of prior milestones.
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