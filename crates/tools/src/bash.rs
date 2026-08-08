//! Shell execution. The broadest capability in the registry; always serial.

use std::process::Stdio;
use std::time::Duration;

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str, truncate_middle};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_OUTPUT_BYTES: usize = 100_000;

pub struct Bash;

#[async_trait::async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: format!(
                "Run a bash command in the workspace directory. Default timeout \
                 {DEFAULT_TIMEOUT_SECS}s, maximum {MAX_TIMEOUT_SECS}s. Output is \
                 truncated beyond {MAX_OUTPUT_BYTES} bytes."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to execute"},
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Optional timeout override in seconds"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let command = required_str(&input, "command")?;
        let timeout = input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);

        // Under a sandbox, the command (and its children) run confined to the
        // workspace via the OS mechanism; otherwise plain bash.
        let (program, args) = match &ctx.sandbox {
            Some(sb) => sb.wrap_bash(command),
            None => (
                "bash".to_string(),
                vec!["-c".to_string(), command.to_string()],
            ),
        };
        let child = tokio::process::Command::new(program)
            .args(args)
            .current_dir(&ctx.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Failed(format!("failed to spawn bash: {e}")))?;

        let output = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
            .await
            .map_err(|_| ToolError::Timeout(timeout))?
            .map_err(|e| ToolError::Failed(format!("failed to collect output: {e}")))?;

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&stderr);
        }
        let text = truncate_middle(text, MAX_OUTPUT_BYTES);

        if output.status.success() {
            Ok(if text.is_empty() {
                "(no output)".into()
            } else {
                text
            })
        } else {
            Err(ToolError::Failed(format!(
                "exit status {}\n{}",
                output.status.code().unwrap_or(-1),
                text
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let out = Bash
            .run(&ctx(), "t", json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_with_output() {
        let err = Bash
            .run(&ctx(), "t", json!({"command": "echo oops >&2; exit 3"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exit status 3"), "{msg}");
        assert!(msg.contains("oops"), "{msg}");
    }

    #[tokio::test]
    async fn times_out() {
        let err = Bash
            .run(
                &ctx(),
                "t",
                json!({"command": "sleep 5", "timeout_seconds": 1}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(1)));
    }

    #[tokio::test]
    async fn missing_command_rejected() {
        let err = Bash.run(&ctx(), "t", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
