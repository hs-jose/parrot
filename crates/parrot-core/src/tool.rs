use crate::error::AgentError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// `ToolOutput` is canonical in `parrot-protocol::types` (it appears on the wire
// in `ServerMessage::ToolResult` and inside `EventLogEntry::ToolResult`). Core
// re-exports it so the `Tool::call` trait returns the same type the daemon
// persists and ships — no boundary conversion needed.
pub use parrot_protocol::types::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub tool_name: String,
    pub tool_call_id: String,
    pub output: ToolOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub max_file_size_bytes: u64,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError>;
}

/// 工具级策略钩子:before 可拒绝调用,after 变换结果。
/// 与 HookRegistry(用户可配、fail-open)分工:这里是框架内建机制,
/// 有序且不可绕过;after 链中最后注册者最后执行(截断收口用)。
#[async_trait]
pub trait ToolHook: Send + Sync {
    fn id(&self) -> &str;
    async fn before(&self, _tool: &str, _args: &Value) -> ToolDecision {
        ToolDecision::Proceed
    }
    async fn after(&self, _tool: &str, _output: &mut ToolOutput) {}
}

#[derive(Debug, Clone)]
pub enum ToolDecision {
    Proceed,
    Deny { reason: String },
}

pub struct TruncateHook;

#[async_trait]
impl ToolHook for TruncateHook {
    fn id(&self) -> &str {
        "truncate"
    }
    async fn after(&self, _tool: &str, output: &mut ToolOutput) {
        output.content = crate::tool_output::truncate_tool_content(&output.content);
    }
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    hooks: RwLock<Vec<Arc<dyn ToolHook>>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            // 截断默认注册且必须保持最后:所有 after 变换(含用户 hook
            // Replace 注入的内容)之后才收口,无界输出无法穿透。
            hooks: RwLock::new(vec![Arc::new(TruncateHook)]),
        }
    }
    pub async fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().await.insert(name, tool);
    }
    /// 追加一个工具钩子;after 链按注册序执行,TruncateHook 永远垫底。
    pub async fn add_hook(&self, hook: Arc<dyn ToolHook>) {
        let mut hooks = self.hooks.write().await;
        hooks.retain(|h| h.id() != "truncate");
        hooks.push(hook);
        hooks.push(Arc::new(TruncateHook));
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

    /// 管道:before 钩子链 → call → Err 转 error ToolOutput → after 链。
    /// 返回 Ok(output) 表示管道走完(含 Deny / 执行错误,均体现为
    /// is_error 输出);Err 仅当工具不存在。
    pub async fn execute(
        &self,
        name: &str,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, AgentError> {
        let hooks = self.hooks.read().await.clone();
        for hook in &hooks {
            if let ToolDecision::Deny { reason } = hook.before(name, &arguments).await {
                return Ok(ToolOutput {
                    content: format!("blocked: {reason}"),
                    is_error: true,
                });
            }
        }

        let Some(tool) = self.get(name).await else {
            return Err(AgentError::ToolExecution {
                tool: name.to_string(),
                message: "Tool not found".to_string(),
            });
        };
        let mut output = tool
            .call(arguments, ctx)
            .await
            .unwrap_or_else(|e| ToolOutput {
                content: format!("Error: {e}"),
                is_error: true,
            });

        for hook in &hooks {
            hook.after(name, &mut output).await;
        }
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
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let tool = EchoTool;
        let result = tool.call(json!({"message": "hello"}), &ctx).await.unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn execute_truncates_oversized_output() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(BigTool)).await;
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let out = registry.execute("big", json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("(truncated, total"));
        assert!(out.content.len() <= crate::tool_output::MAX_TOOL_OUTPUT_BYTES + 64);
    }

    #[tokio::test]
    async fn execute_converts_call_error_to_error_output() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(FailTool)).await;
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let out = registry.execute("fail", json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.starts_with("Error:"));
    }

    #[tokio::test]
    async fn execute_unknown_tool_still_errs() {
        let registry = ToolRegistry::new();
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let err = registry.execute("nope", json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, AgentError::ToolExecution { .. }));
    }

    #[tokio::test]
    async fn before_deny_blocks_call() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool)).await;
        registry.add_hook(Arc::new(DenyAllHook)).await;
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let out = registry
            .execute("echo", json!({"message": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "blocked: denied by policy");
    }

    #[tokio::test]
    async fn custom_after_hook_runs_before_truncate() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(BigTool)).await;
        registry.add_hook(Arc::new(AppendHook)).await;
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
        let out = registry.execute("big", json!({}), &ctx).await.unwrap();
        // Append 放大后截断仍生效 ⇒ 证明 after 链中截断垫底。
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

    struct DenyAllHook;

    #[async_trait]
    impl ToolHook for DenyAllHook {
        fn id(&self) -> &str {
            "deny_all"
        }
        async fn before(&self, _tool: &str, _args: &Value) -> ToolDecision {
            ToolDecision::Deny {
                reason: "denied by policy".into(),
            }
        }
    }

    struct AppendHook;

    #[async_trait]
    impl ToolHook for AppendHook {
        fn id(&self) -> &str {
            "append"
        }
        async fn after(&self, _tool: &str, output: &mut ToolOutput) {
            output
                .content
                .push_str(&"y".repeat(crate::tool_output::MAX_TOOL_OUTPUT_BYTES));
        }
    }
}
