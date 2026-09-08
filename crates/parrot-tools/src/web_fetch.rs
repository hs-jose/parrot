use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Default)]
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL. Returns the response body as text."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let url = super::str_arg(&arguments, "url", "web_fetch")?;

        let response =
            self.client
                .get(url)
                .send()
                .await
                .map_err(|e| AgentError::ToolExecution {
                    tool: "web_fetch".to_string(),
                    message: format!("Failed to fetch URL '{}': {}", url, e),
                })?;

        let status = response.status();
        let content = response
            .text()
            .await
            .map_err(|e| AgentError::ToolExecution {
                tool: "web_fetch".to_string(),
                message: format!("Failed to read response body: {}", e),
            })?;

        // 超大响应截断（按字符边界切，避免 panic）
        let content = if content.len() > 100_000 {
            let mut end = 100_000;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}... (truncated, total {} bytes)",
                &content[..end],
                content.len()
            )
        } else {
            content
        };

        Ok(ToolOutput {
            content: format!("HTTP {} from {}\n\n{}", status.as_u16(), url, content),
            is_error: !status.is_success(),
        })
    }
}
