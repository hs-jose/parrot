use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;
use std::process::Stdio;

pub struct ShellExecTool;

impl ShellExecTool {
    pub fn new(_working_dir: std::path::PathBuf) -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ShellExecTool {
    fn name(&self) -> &str {
        "shell_exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Commands are sandboxed to the working directory."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory for the command (optional, defaults to sandbox working directory)"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::ToolExecution {
                tool: "shell_exec".to_string(),
                message: "Missing 'command' argument".to_string(),
            })?;

        let working_dir = arguments
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(|p| {
                if std::path::Path::new(p).is_absolute() {
                    std::path::PathBuf::from(p)
                } else {
                    ctx.working_dir.join(p)
                }
            })
            .unwrap_or_else(|| ctx.working_dir.clone());

        #[cfg(target_family = "windows")]
        let output = tokio::process::Command::new("cmd")
            .args(["/C", command])
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| AgentError::ToolExecution {
                tool: "shell_exec".to_string(),
                message: format!("Failed to execute command: {}", e),
            })?;

        #[cfg(not(target_family = "windows"))]
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| AgentError::ToolExecution {
                tool: "shell_exec".to_string(),
                message: format!("Failed to execute command: {}", e),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push_str("\n--- STDERR ---\n");
            }
            result.push_str(&stderr);
        }

        if result.is_empty() {
            result = format!(
                "Command exited with code {}",
                output.status.code().unwrap_or(-1)
            );
        }

        Ok(ToolOutput {
            content: result,
            is_error: !output.status.success(),
        })
    }
}

/// Result of a daemon-side shell execution for the `!cmd` TUI feature.
pub struct CommandResult {
    pub output: String,
    pub exit_code: i32,
}

/// Execute `command` with `cmd /C` (Windows) or `sh -c` (Unix) in
/// `working_dir`. Mirrors `ShellExecTool::call` output semantics (combined
/// stdout/stderr, `--- STDERR ---` separator, "Command exited with code N"
/// fallback). Errors are spawn failures, returned as `Err(String)`.
pub async fn run_shell_command(
    command: &str,
    working_dir: &std::path::Path,
) -> Result<CommandResult, String> {
    #[cfg(target_family = "windows")]
    let output = tokio::process::Command::new("cmd")
        .args(["/C", command])
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to execute command: {}", e))?;

    #[cfg(not(target_family = "windows"))]
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to execute command: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push_str("\n--- STDERR ---\n");
        }
        result.push_str(&stderr);
    }
    if result.is_empty() {
        result = format!(
            "Command exited with code {}",
            output.status.code().unwrap_or(-1)
        );
    }

    Ok(CommandResult {
        output: result,
        exit_code: output.status.code().unwrap_or(-1),
    })
}
