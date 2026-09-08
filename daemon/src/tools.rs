use serde::{Deserialize, Serialize};
use rig::tool::Tool;
use tokio::process::Command;
use std::process::Output;
use thiserror::Error;
use serde_json::json;

#[derive(Error, Debug)]
pub enum BashExecutorError {
    #[error("Failed to execute command: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BashExecutorArgs {
    pub command: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BashExecutorResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Default)]
pub struct BashExecutor;

impl Tool for BashExecutor {
    const NAME: &'static str = "bash_executor";

    type Error = BashExecutorError;
    type Args = BashExecutorArgs;
    type Output = BashExecutorResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Execute a bash command in the native Linux userland and return stdout, stderr, and exit code.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    }
                }
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let output: Output = Command::new("bash")
            .arg("-c")
            .arg(&args.command)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(BashExecutorResult {
            stdout,
            stderr,
            exit_code: output.status.code(),
        })
    }
}
