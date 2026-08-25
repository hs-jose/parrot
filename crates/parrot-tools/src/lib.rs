use parrot_config::AppConfig;
use parrot_core::tool::ToolRegistry;

pub mod file_glob;
pub mod file_grep;
pub mod file_read;
pub mod file_write;
pub mod shell_exec;
pub mod web_fetch;

pub async fn register_all(registry: &ToolRegistry, config: &AppConfig) {
    let working_dir = std::path::PathBuf::from(&config.tools.sandbox.working_dir);
    let max_file_size = config.tools.max_file_size_mb * 1024 * 1024;

    // Always register read-only tools
    registry
        .register(std::sync::Arc::new(file_read::FileReadTool::new(
            working_dir.clone(),
            max_file_size,
        )))
        .await;
    registry
        .register(std::sync::Arc::new(file_glob::FileGlobTool::new(
            working_dir.clone(),
        )))
        .await;
    registry
        .register(std::sync::Arc::new(file_grep::FileGrepTool::new(
            working_dir.clone(),
        )))
        .await;

    // Conditionally register write tools
    if config.tools.file_write_allowed {
        registry
            .register(std::sync::Arc::new(file_write::FileWriteTool::new(
                working_dir.clone(),
                max_file_size,
            )))
            .await;
    }

    if config.tools.shell_allowed {
        registry
            .register(std::sync::Arc::new(shell_exec::ShellExecTool::new(
                working_dir.clone(),
            )))
            .await;
    }

    if config.tools.web_allowed {
        registry
            .register(std::sync::Arc::new(web_fetch::WebFetchTool::new()))
            .await;
    }
}
