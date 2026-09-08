use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;
use std::process::Stdio;

#[derive(Default)]
pub struct ShellExecTool;

impl ShellExecTool {
    pub fn new() -> Self {
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
        let command = super::str_arg(&arguments, "command", "shell_exec")?;

        let working_dir = arguments
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(|p| super::resolve_arg_path(p, &ctx.working_dir))
            .unwrap_or_else(|| ctx.working_dir.clone());

        let output =
            spawn_shell(command, &working_dir)
                .await
                .map_err(|e| AgentError::ToolExecution {
                    tool: "shell_exec".to_string(),
                    message: e,
                })?;

        Ok(ToolOutput {
            content: assemble_output(&output),
            is_error: !output.status.success(),
        })
    }
}

/// `!cmd` TUI 功能在 daemon 侧执行 shell 的结果。
pub struct CommandResult {
    pub output: String,
    pub exit_code: i32,
}

/// 平台对应的 shell 程序。
fn shell_program() -> &'static str {
    #[cfg(target_family = "windows")]
    {
        "cmd"
    }
    #[cfg(not(target_family = "windows"))]
    {
        "sh"
    }
}

/// 在 `working_dir` 下以 `cmd /C`（Windows）或 `sh -c`（Unix）执行
/// `command`，捕获 stdout/stderr。`Err` 仅表示 spawn 失败。
async fn spawn_shell(
    command: &str,
    working_dir: &std::path::Path,
) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new(shell_program());
    #[cfg(target_family = "windows")]
    cmd.args(["/C", command]);
    #[cfg(not(target_family = "windows"))]
    cmd.arg("-c").arg(command);
    cmd.current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to execute command: {e}"))
}

/// 按约定语义拼装输出：stdout 在前；stderr 存在时加 `--- STDERR ---`
/// 分隔附后；两者皆空时回退"Command exited with code N"。
fn assemble_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

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
    result
}

/// daemon 侧执行 shell 命令（供 `!cmd` TUI 功能使用）。输出语义与
/// `ShellExecTool::call` 一致（复用 `spawn_shell` + `assemble_output`）。
pub async fn run_shell_command(
    command: &str,
    working_dir: &std::path::Path,
) -> Result<CommandResult, String> {
    let output = spawn_shell(command, working_dir).await?;
    Ok(CommandResult {
        exit_code: output.status.code().unwrap_or(-1),
        output: assemble_output(&output),
    })
}
