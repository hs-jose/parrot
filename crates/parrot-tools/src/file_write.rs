use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Default)]
pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it does not exist."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let path_str = super::str_arg(&arguments, "path", "file_write")?;
        let content = super::str_arg(&arguments, "content", "file_write")?;

        if content.len() as u64 > ctx.max_file_size_bytes {
            return Ok(ToolOutput {
                content: format!(
                    "Content too large: {} bytes (max: {} bytes)",
                    content.len(),
                    ctx.max_file_size_bytes
                ),
                is_error: true,
            });
        }

        let path = super::resolve_arg_path(path_str, &ctx.working_dir);

        // 父目录不存在则创建
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AgentError::ToolExecution {
                tool: "file_write".to_string(),
                message: format!("Cannot create directory {:?}: {}", parent, e),
            })?;
        }

        std::fs::write(&path, content).map_err(|e| AgentError::ToolExecution {
            tool: "file_write".to_string(),
            message: format!("Cannot write file {:?}: {}", path, e),
        })?;

        Ok(ToolOutput {
            content: format!("Successfully wrote {} bytes to {:?}", content.len(), path),
            is_error: false,
        })
    }
}
