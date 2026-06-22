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

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
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
}
