use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

pub struct FileGlobTool;

impl FileGlobTool {
    pub fn new(_working_dir: std::path::PathBuf) -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileGlobTool {
    fn name(&self) -> &str { "file_glob" }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Returns matching file paths relative to the working directory."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g. \"**/*.rs\", \"src/**/*.py\")"
                },
                "path": {
                    "type": "string",
                    "description": "Base directory to search in (optional, defaults to working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let pattern = arguments.get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::ToolExecution {
                tool: "file_glob".to_string(),
                message: "Missing 'pattern' argument".to_string(),
            })?;

        let base_path = arguments.get("path")
            .and_then(|v| v.as_str())
            .map(|p| {
                if std::path::Path::new(p).is_absolute() {
                    std::path::PathBuf::from(p)
                } else {
                    ctx.working_dir.join(p)
                }
            })
            .unwrap_or_else(|| ctx.working_dir.clone());

        let glob_pattern = base_path.join(pattern);
        let glob_str = glob_pattern.to_string_lossy().to_string();

        let mut matches = Vec::new();
        match glob::glob(&glob_str) {
            Ok(paths) => {
                for entry in paths {
                    match entry {
                        Ok(path) => {
                            let relative = path.strip_prefix(&base_path)
                                .unwrap_or(&path);
                            matches.push(relative.to_string_lossy().to_string());
                        }
                        Err(e) => {
                            return Ok(ToolOutput {
                                content: format!("Glob error: {}", e),
                                is_error: true,
                            });
                        }
                    }
                }
            }
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Invalid glob pattern '{}': {}", pattern, e),
                    is_error: true,
                });
            }
        }

        if matches.is_empty() {
            Ok(ToolOutput {
                content: format!("No files matched pattern '{}' in {:?}", pattern, base_path),
                is_error: false,
            })
        } else {
            matches.sort();
            Ok(ToolOutput {
                content: matches.join("\n"),
                is_error: false,
            })
        }
    }
}