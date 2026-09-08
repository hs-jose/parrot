use crate::error::AgentError;
use crate::tool_output::truncate_tool_content;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

pub use parrot_protocol::types::ToolOutput;

/// 会话级"已读文件"集合的共享句柄。file_read 写入、file_edit 检查。
/// 纯内存状态，core 保持 zero-IO。
pub type SharedFilesRead = Arc<Mutex<HashSet<PathBuf>>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub max_file_size_bytes: u64,
    /// 本会话已成功读取过的文件（`resolve_arg_path` 解析后的路径）。
    /// file_edit 的 read-before-edit 护栏数据源。
    pub files_read: SharedFilesRead,
}

impl ToolContext {
    pub fn new(working_dir: std::path::PathBuf, max_file_size_bytes: u64) -> Self {
        Self {
            working_dir,
            max_file_size_bytes,
            files_read: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 注入外部共享的已读集合（engine 每会话持有一份）。
    pub fn with_files_read(mut self, files_read: SharedFilesRead) -> Self {
        self.files_read = files_read;
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError>;

    /// 工具级前置策略:返回 Err(reason) 拒绝调用。默认放行。
    async fn before_call(&self, _arguments: &Value) -> Result<(), String> {
        Ok(())
    }
    /// 工具级输出变换(截断之前)。默认 no-op。
    async fn after_call(&self, _output: &mut ToolOutput) {}
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }
    pub async fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().await.insert(name, tool);
    }
    pub async fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().await.get(name).cloned()
    }
    pub async fn list_definitions(&self) -> Vec<ToolDefinition> {
        let tools = self.tools.read().await;
        let mut defs = Vec::new();
        for tool in tools.values() {
            defs.push(ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            });
        }
        defs
    }

    /// 管道:before_call → call → after_call → 截断(注册表机制,最后)。
    /// Deny 与执行错误都产生合成 error 输出并统一走 after_call + 截断,
    /// 保证进入上下文/事件日志的工具输出一定被截断。Err 仅当工具不存在。
    pub async fn execute(
        &self,
        name: &str,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AgentError> {
        let Some(tool) = self.get(name).await else {
            return Err(AgentError::ToolExecution {
                tool: name.to_string(),
                message: "Tool not found".to_string(),
            });
        };

        let mut output = match tool.before_call(&arguments).await {
            Ok(()) => tool
                .call(arguments, ctx)
                .await
                .unwrap_or_else(|e| ToolOutput {
                    content: format!("Error: {e}"),
                    is_error: true,
                }),
            Err(reason) => ToolOutput {
                content: format!("blocked: {reason}"),
                is_error: true,
            },
        };

        tool.after_call(&mut output).await;
        output.content = truncate_tool_content(&output.content);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes back the input"
        }
        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"]
            })
        }
        async fn call(
            &self,
            arguments: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, AgentError> {
            let msg = arguments
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok(ToolOutput {
                content: msg.to_string(),
                is_error: false,
            })
        }
    }

    #[tokio::test]
    async fn register_and_get_tool() {
        let registry = ToolRegistry::new();
        let tool = Arc::new(EchoTool);
        registry.register(tool).await;
        let got = registry.get("echo").await;
        assert!(got.is_some());
        let missing = registry.get("nonexistent").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn list_definitions() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).await;
        let defs = registry.list_definitions().await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[tokio::test]
    async fn call_tool() {
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let tool = EchoTool;
        let result = tool.call(json!({"message": "hello"}), &ctx).await.unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn execute_truncates_oversized_output() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(BigTool)).await;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let out = registry.execute("big", json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("(truncated, total"));
        assert!(out.content.len() <= crate::tool_output::MAX_TOOL_OUTPUT_BYTES + 64);
    }

    #[tokio::test]
    async fn execute_converts_call_error_to_error_output() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(FailTool)).await;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let out = registry.execute("fail", json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.starts_with("Error:"));
    }

    #[tokio::test]
    async fn execute_unknown_tool_still_errs() {
        let registry = ToolRegistry::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let err = registry.execute("nope", json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, AgentError::ToolExecution { .. }));
    }

    #[tokio::test]
    async fn before_call_deny_blocks_call() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DenyAllTool)).await;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let out = registry.execute("deny_all", json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "blocked: denied by policy");
    }

    #[tokio::test]
    async fn denied_output_also_truncated() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DenyHugeReasonTool)).await;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let out = registry
            .execute("deny_huge", json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("(truncated, total"));
    }

    #[tokio::test]
    async fn after_call_runs_before_truncate() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(AppendTool)).await;
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
        let out = registry.execute("append", json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("(truncated"));
    }

    struct BigTool;

    #[async_trait]
    impl Tool for BigTool {
        fn name(&self) -> &str {
            "big"
        }
        fn description(&self) -> &str {
            "Emits oversized output"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
            Ok(ToolOutput {
                content: "x".repeat(crate::tool_output::MAX_TOOL_OUTPUT_BYTES * 2),
                is_error: false,
            })
        }
    }

    struct FailTool;

    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
            Err(AgentError::ToolExecution {
                tool: "fail".into(),
                message: "boom".into(),
            })
        }
    }

    struct DenyAllTool;

    #[async_trait]
    impl Tool for DenyAllTool {
        fn name(&self) -> &str {
            "deny_all"
        }
        fn description(&self) -> &str {
            "Denies itself"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
            unreachable!("before_call should have denied")
        }
        async fn before_call(&self, _args: &Value) -> Result<(), String> {
            Err("denied by policy".into())
        }
    }

    struct DenyHugeReasonTool;

    #[async_trait]
    impl Tool for DenyHugeReasonTool {
        fn name(&self) -> &str {
            "deny_huge"
        }
        fn description(&self) -> &str {
            "Denies with huge reason"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
            unreachable!("before_call should have denied")
        }
        async fn before_call(&self, _args: &Value) -> Result<(), String> {
            Err("n".repeat(crate::tool_output::MAX_TOOL_OUTPUT_BYTES * 2))
        }
    }

    struct AppendTool;

    #[async_trait]
    impl Tool for AppendTool {
        fn name(&self) -> &str {
            "append"
        }
        fn description(&self) -> &str {
            "Appends oversized content in after_call"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
            Ok(ToolOutput {
                content: "small".to_string(),
                is_error: false,
            })
        }
        async fn after_call(&self, output: &mut ToolOutput) {
            output
                .content
                .push_str(&"y".repeat(crate::tool_output::MAX_TOOL_OUTPUT_BYTES));
        }
    }

    #[test]
    fn files_read_set_is_shared_via_builder() {
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 1024);
        let shared = std::sync::Arc::clone(&ctx.files_read);

        // with_files_read 注入的是外部集合：往注入集合写，ctx 里可见。
        let injected: crate::tool::SharedFilesRead = Default::default();
        injected
            .lock()
            .unwrap()
            .insert(std::path::PathBuf::from("a.rs"));
        let ctx2 = ToolContext::new(std::path::PathBuf::from("."), 1024)
            .with_files_read(std::sync::Arc::clone(&injected));

        assert!(ctx2
            .files_read
            .lock()
            .unwrap()
            .contains(std::path::Path::new("a.rs")));
        assert!(shared.lock().unwrap().is_empty(), "默认集合独立于注入集合");
    }
}
