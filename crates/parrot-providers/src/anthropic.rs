use async_trait::async_trait;
use parrot_core::error::ProviderError;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderStopReason, ProviderStreamEvent};
use parrot_core::tool::ToolDefinition;
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig, ModelInfo, ToolCallInfo};
use parrot_protocol::types::Usage;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: Client,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Clone)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    #[allow(dead_code)]
    stop_reason: Option<String>,
    #[allow(dead_code)]
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[allow(dead_code)]
    input_tokens: u32,
    #[allow(dead_code)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>, _default_model: String) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self {
            api_key,
            base_url,
            client: Client::new(),
        }
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<AnthropicMessage> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < messages.len() {
            let msg = &messages[i];
            match msg.role {
                ChatRole::System => {
                    i += 1;
                    continue;
                }
                ChatRole::User => {
                    result.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: Value::String(msg.content.clone()),
                    });
                    i += 1;
                }
                ChatRole::Assistant => {
                    if let Some(tool_calls) = &msg.tool_calls {
                        // Assistant message with tool_use blocks
                        let mut blocks = Vec::new();
                        if !msg.content.is_empty() {
                            blocks.push(serde_json::json!({
                                "type": "text",
                                "text": msg.content,
                            }));
                        }
                        for tc in tool_calls {
                            blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": tc.arguments,
                            }));
                        }
                        result.push(AnthropicMessage {
                            role: "assistant".to_string(),
                            content: Value::Array(blocks),
                        });
                    } else {
                        result.push(AnthropicMessage {
                            role: "assistant".to_string(),
                            content: Value::String(msg.content.clone()),
                        });
                    }
                    i += 1;
                }
                ChatRole::Tool => {
                    // Collect consecutive tool results into a single user message
                    let mut tool_results = Vec::new();
                    while i < messages.len() && messages[i].role == ChatRole::Tool {
                        tool_results.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": messages[i].tool_call_id.as_deref().unwrap_or(""),
                            "content": messages[i].content,
                        }));
                        i += 1;
                    }
                    result.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: Value::Array(tool_results),
                    });
                }
            }
        }
        result
    }

    fn convert_tools(tools: &[ToolDefinition]) -> Vec<AnthropicTool> {
        tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect()
    }

    fn extract_system_prompt(messages: &[ChatMessage]) -> Option<String> {
        messages
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.clone())
            .reduce(|a, b| format!("{}\n{}", a, b))
    }

    async fn send_request(
        &self,
        request: &AnthropicRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        // Wrap the single HTTP attempt in the retry/backoff policy (§4.8).
        // The closure captures `self` and `request` by reference; each retry
        // builds a fresh future and re-issues the same POST. Stream-phase
        // errors don't go through this path (we only retry up to the point
        // where the response body starts streaming).
        crate::retry::with_retry(|| self.send_request_once(request)).await
    }

    async fn send_request_once(
        &self,
        request: &AnthropicRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5000);
            return Err(ProviderError::RateLimited {
                retry_after_ms: retry_after,
            });
        }
        if status.is_client_error() || status.is_server_error() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_id(&self) -> &str {
        "anthropic"
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![
            ModelInfo {
                id: "claude-sonnet-4-6".to_string(),
                name: "Claude Sonnet 4.6".to_string(),
                provider: "anthropic".to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-opus-4".to_string(),
                name: "Claude Opus 4".to_string(),
                provider: "anthropic".to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-haiku-3-5".to_string(),
                name: "Claude Haiku 3.5".to_string(),
                provider: "anthropic".to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
        ])
    }

    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        let system = Self::extract_system_prompt(messages);
        let anthropic_messages = Self::convert_messages(messages);
        let anthropic_tools = Self::convert_tools(tools);

        let request = AnthropicRequest {
            model: model.to_string(),
            messages: anthropic_messages,
            max_tokens: config.max_tokens.unwrap_or(8192),
            system,
            tools: anthropic_tools,
            stream: Some(true),
            temperature: config.temperature,
            stop_sequences: config.stop_sequences.clone(),
        };

        let response = self.send_request(&request).await?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            if let Err(e) = parse_sse_stream(response, tx).await {
                tracing::error!("SSE stream error: {}", e);
            }
        });

        Ok(ChatStream { inner: rx })
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        let system = Self::extract_system_prompt(messages);
        let anthropic_messages = Self::convert_messages(messages);
        let anthropic_tools = Self::convert_tools(tools);

        let request = AnthropicRequest {
            model: model.to_string(),
            messages: anthropic_messages,
            max_tokens: config.max_tokens.unwrap_or(8192),
            system,
            tools: anthropic_tools,
            stream: None,
            temperature: config.temperature,
            stop_sequences: config.stop_sequences.clone(),
        };

        let response = self.send_request(&request).await?;
        let body: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let mut text_content = String::new();
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();

        for block in &body.content {
            match block.content_type.as_str() {
                "text" => {
                    if let Some(text) = &block.text {
                        text_content.push_str(text);
                    }
                }
                "tool_use" => {
                    if let (Some(id), Some(name), Some(input)) =
                        (&block.id, &block.name, &block.input)
                    {
                        tool_calls.push(ToolCallInfo {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: input.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(ChatMessage {
            role: ChatRole::Assistant,
            content: text_content,
            tool_call_id: None,
            tool_name: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
        })
    }
}

fn parse_stop_reason(reason: &str) -> ProviderStopReason {
    ProviderStopReason::from_anthropic(reason)
}

async fn parse_sse_stream(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
) -> Result<(), ProviderError> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut tool_index_to_id: HashMap<usize, String> = HashMap::new();
    let mut tool_accumulated_args: HashMap<usize, String> = HashMap::new();
    let mut current_usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
    };

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::StreamError(e.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE events (separated by blank lines)
        while let Some(event_end) = buffer.find("\n\n") {
            let event_text = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            let mut event_type = String::new();
            let mut data_lines = Vec::new();

            for line in event_text.lines() {
                if let Some(stripped) = line.strip_prefix("event:") {
                    event_type = stripped.trim().to_string();
                } else if let Some(stripped) = line.strip_prefix("data:") {
                    data_lines.push(stripped.trim().to_string());
                }
            }

            if data_lines.is_empty() {
                continue;
            }

            let data = data_lines.join("\n");
            let json: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event_type.as_str() {
                "message_start" => {
                    if let Some(msg) = json.get("message") {
                        if let Some(usage) = msg.get("usage") {
                            current_usage.input_tokens = usage
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                as u32;
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(content_block) = json.get("content_block") {
                        let block_type = content_block
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let index =
                            json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        if block_type == "tool_use" {
                            let id = content_block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = content_block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            tool_index_to_id.insert(index, id.clone());
                            let _ = tx
                                .send(ProviderStreamEvent::ToolCallStart { id, name })
                                .await;
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = json.get("delta") {
                        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let index =
                            json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    let _ = tx
                                        .send(ProviderStreamEvent::TextDelta {
                                            delta: text.to_string(),
                                        })
                                        .await;
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) =
                                    delta.get("partial_json").and_then(|v| v.as_str())
                                {
                                    if let Some(tool_id) = tool_index_to_id.get(&index) {
                                        tool_accumulated_args
                                            .entry(index)
                                            .or_default()
                                            .push_str(partial);
                                        let _ = tx
                                            .send(ProviderStreamEvent::ToolCallDelta {
                                                id: tool_id.clone(),
                                                args_delta: partial.to_string(),
                                            })
                                            .await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    if let Some(tool_id) = tool_index_to_id.get(&index) {
                        let args_str = tool_accumulated_args
                            .get(&index)
                            .map(String::as_str)
                            .unwrap_or("{}");
                        let arguments = serde_json::from_str(args_str)
                            .unwrap_or(Value::Object(serde_json::Map::new()));
                        let _ = tx
                            .send(ProviderStreamEvent::ToolCallEnd {
                                id: tool_id.clone(),
                                arguments,
                            })
                            .await;
                    }
                }
                "message_delta" => {
                    if let Some(delta) = json.get("delta") {
                        if let Some(usage_delta) = json.get("usage") {
                            current_usage.output_tokens += usage_delta
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                as u32;
                        }
                        let stop_reason = delta
                            .get("stop_reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("end_turn");
                        let _ = tx
                            .send(ProviderStreamEvent::Finish {
                                stop_reason: parse_stop_reason(stop_reason),
                                usage: current_usage.clone(),
                            })
                            .await;
                    }
                }
                "message_stop" => {}
                "ping" => {}
                "error" => {
                    let error_msg = json
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown stream error");
                    tracing::error!("Anthropic stream error: {}", error_msg);
                    break;
                }
                _ => {}
            }
        }
    }

    Ok(())
}
