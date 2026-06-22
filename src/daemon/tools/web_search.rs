use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

pub struct WebSearchTool;

impl Default for WebSearchTool {
    fn default() -> Self {
        Self
    }
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Not yet implemented (planned for Phase 2)."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, _arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(ToolOutput {
            content: "web_search is not yet implemented. This tool will be available in Phase 2."
                .to_string(),
            is_error: true,
        })
    }
}
