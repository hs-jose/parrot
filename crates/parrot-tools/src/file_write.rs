use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new(_working_dir: std::path::PathBuf, _max_file_size: u64) -> Self {
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
        let path_str = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::ToolExecution {
                tool: "file_write".to_string(),
                message: "Missing 'path' argument".to_string(),
            })?;

        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::ToolExecution {
                tool: "file_write".to_string(),
                message: "Missing 'content' argument".to_string(),
            })?;

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

        let path = if std::path::Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        // Create parent directories if needed
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
