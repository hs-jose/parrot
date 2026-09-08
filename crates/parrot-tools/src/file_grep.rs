use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use regex::Regex;
use serde_json::Value;

#[derive(Default)]
pub struct FileGrepTool;

impl FileGrepTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileGrepTool {
    fn name(&self) -> &str {
        "file_grep"
    }

    fn description(&self) -> &str {
        "Search for a regex pattern in files. Returns matching lines in file:line:content format."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in (optional, defaults to working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "File glob pattern to filter files (e.g. \"*.rs\", \"*.py\"). Optional."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let pattern_str = super::str_arg(&arguments, "pattern", "file_grep")?;

        let re = Regex::new(pattern_str).map_err(|e| AgentError::ToolExecution {
            tool: "file_grep".to_string(),
            message: format!("Invalid regex pattern '{}': {}", pattern_str, e),
        })?;

        let base_path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| super::resolve_arg_path(p, &ctx.working_dir))
            .unwrap_or_else(|| ctx.working_dir.clone());

        let include_pattern = arguments
            .get("include")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // include 过滤 glob 只在入口编译一次；编译失败按工具错误返回
        // （与非法 regex 的处理一致）。
        let include_glob = if include_pattern.is_empty() {
            None
        } else {
            Some(
                glob::Pattern::new(include_pattern).map_err(|e| AgentError::ToolExecution {
                    tool: "file_grep".to_string(),
                    message: format!("Invalid include pattern '{}': {}", include_pattern, e),
                })?,
            )
        };

        let mut results = Vec::new();
        let mut files_searched = 0u32;
        let max_results = 100;

        search_dir(
            &base_path,
            &ctx.working_dir,
            &re,
            include_glob.as_ref(),
            &mut results,
            &mut files_searched,
            max_results,
        )?;

        if results.is_empty() {
            Ok(ToolOutput {
                content: format!(
                    "No matches found for pattern '{}' in {:?}",
                    pattern_str, base_path
                ),
                is_error: false,
            })
        } else {
            Ok(ToolOutput {
                content: results.join("\n"),
                is_error: false,
            })
        }
    }
}

fn search_dir(
    dir: &std::path::Path,
    working_dir: &std::path::Path,
    re: &Regex,
    include_glob: Option<&glob::Pattern>,
    results: &mut Vec<String>,
    files_searched: &mut u32,
    max_results: usize,
) -> Result<(), AgentError> {
    if results.len() >= max_results || *files_searched >= 500 {
        return Ok(());
    }

    let entries = std::fs::read_dir(dir).map_err(|e| AgentError::ToolExecution {
        tool: "file_grep".to_string(),
        message: format!("Cannot read directory {:?}: {}", dir, e),
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| AgentError::ToolExecution {
            tool: "file_grep".to_string(),
            message: format!("Directory entry error: {}", e),
        })?;

        let path = entry.path();

        if path.is_dir() {
            // 跳过隐藏目录和常见非项目目录
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
            }
            search_dir(
                &path,
                working_dir,
                re,
                include_glob,
                results,
                files_searched,
                max_results,
            )?;
        } else if path.is_file() {
            // 应用 include 过滤
            if let Some(gp) = include_glob {
                match path.file_name() {
                    Some(fname) if gp.matches(&fname.to_string_lossy()) => {}
                    _ => continue,
                }
            }

            // 跳过大文件
            if let Ok(metadata) = path.metadata() {
                if metadata.len() > 1024 * 1024 {
                    continue;
                }
            }

            if let Ok(content) = std::fs::read_to_string(&path) {
                *files_searched += 1;
                let relative = path.strip_prefix(working_dir).unwrap_or(&path);
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        results.push(format!("{}:{}:{}", relative.to_string_lossy(), i + 1, line));
                        if results.len() >= max_results {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
