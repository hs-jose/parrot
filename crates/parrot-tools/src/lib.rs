use parrot_config::AppConfig;
use parrot_core::error::AgentError;
use parrot_core::tool::ToolRegistry;
use serde_json::Value;

pub mod file_glob;
pub mod file_grep;
pub mod file_read;
pub mod file_write;
pub mod shell_exec;
pub mod web_fetch;

/// 把路径参数解析为绝对路径：绝对路径原样使用；相对路径拼接沙箱工作目录。
pub(crate) fn resolve_arg_path(p: &str, working_dir: &std::path::Path) -> std::path::PathBuf {
    if std::path::Path::new(p).is_absolute() {
        std::path::PathBuf::from(p)
    } else {
        working_dir.join(p)
    }
}

/// 取字符串参数；缺失时统一产出 `ToolExecution` 错误。
pub(crate) fn str_arg<'a>(
    arguments: &'a Value,
    key: &str,
    tool: &str,
) -> Result<&'a str, AgentError> {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Missing '{key}' argument"),
        })
}

pub async fn register_all(registry: &ToolRegistry, config: &AppConfig) {
    // 只读工具总是注册
    registry
        .register(std::sync::Arc::new(file_read::FileReadTool::new()))
        .await;
    registry
        .register(std::sync::Arc::new(file_glob::FileGlobTool::new()))
        .await;
    registry
        .register(std::sync::Arc::new(file_grep::FileGrepTool::new()))
        .await;

    // 写入类工具按配置注册
    if config.tools.file_write_allowed {
        registry
            .register(std::sync::Arc::new(file_write::FileWriteTool::new()))
            .await;
    }

    if config.tools.shell_allowed {
        registry
            .register(std::sync::Arc::new(shell_exec::ShellExecTool::new()))
            .await;
    }

    if config.tools.web_allowed {
        registry
            .register(std::sync::Arc::new(web_fetch::WebFetchTool::new()))
            .await;
    }
}
