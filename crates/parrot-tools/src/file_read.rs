use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Default)]
pub struct FileReadTool;

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Use offset and limit to read specific line ranges."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (1-based, optional)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (optional)"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let path_str = super::str_arg(&arguments, "path", "file_read")?;

        let path = super::resolve_arg_path(path_str, &ctx.working_dir);

        let metadata = std::fs::metadata(&path).map_err(|e| AgentError::ToolExecution {
            tool: "file_read".to_string(),
            message: format!("Cannot read file {:?}: {}", path, e),
        })?;

        if metadata.len() > ctx.max_file_size_bytes {
            return Ok(ToolOutput {
                content: format!(
                    "File too large: {} bytes (max: {} bytes)",
                    metadata.len(),
                    ctx.max_file_size_bytes
                ),
                is_error: true,
            });
        }

        let content = std::fs::read_to_string(&path).map_err(|e| AgentError::ToolExecution {
            tool: "file_read".to_string(),
            message: format!("Cannot read file {:?}: {}", path, e),
        })?;

        // 标记为已读：file_edit 的 read-before-edit 护栏依赖此集合。
        // 部分读取（offset/limit）同样计入——模型至少见过该文件的内容。
        ctx.files_read.lock().unwrap().insert(path.clone());

        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as usize;
        let limit = arguments.get("limit").and_then(|v| v.as_u64());

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1);
        let end = match limit {
            Some(l) => (start + l as usize).min(lines.len()),
            None => lines.len(),
        };

        if start >= lines.len() {
            return Ok(ToolOutput {
                content: format!(
                    "File has {} lines, offset {} is out of range",
                    lines.len(),
                    offset
                ),
                is_error: true,
            });
        }

        let result: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {}", start + i + 1, line))
            .collect();

        Ok(ToolOutput {
            content: result.join("\n"),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn successful_read_marks_file_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        let out = FileReadTool
            .call(json!({"path": "sample.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);

        assert!(
            ctx.files_read.lock().unwrap().contains(&path),
            "file_read 成功后应把解析后的绝对路径标记为已读"
        );
    }

    #[tokio::test]
    async fn failed_read_does_not_mark_file_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        // 当前契约：I/O 失败时 call 返回 Err(AgentError)，而非 is_error=true 的 ToolOutput。
        let out = FileReadTool
            .call(json!({"path": "missing.txt"}), &ctx)
            .await;
        assert!(out.is_err());
        assert!(ctx.files_read.lock().unwrap().is_empty());
    }
}
