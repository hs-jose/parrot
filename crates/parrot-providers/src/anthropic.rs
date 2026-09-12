use async_trait::async_trait;
use parrot_config::ModelEntry;
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
    provider_id: String,
    api_key: String,
    base_url: String,
    config_models: Vec<ModelEntry>,
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

#[derive(Debug, Serialize)]
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
    pub fn new(
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        config_models: Vec<ModelEntry>,
    ) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self {
            provider_id,
            api_key,
            base_url,
            config_models,
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
                        // 带 tool_use 块的助手消息
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
                    // 把连续的工具结果合并进一条 user 消息
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
        let joined = messages
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (!joined.is_empty()).then_some(joined)
    }

    /// 组装 `AnthropicRequest`；`chat` 与 `chat_stream` 仅 stream 标志不同。
    fn build_request(
        model: &str,
        messages: Vec<AnthropicMessage>,
        tools: Vec<AnthropicTool>,
        system: Option<String>,
        config: &GenerateConfig,
        stream: Option<bool>,
    ) -> AnthropicRequest {
        AnthropicRequest {
            model: model.to_string(),
            messages,
            max_tokens: config.max_tokens.unwrap_or(8192),
            system,
            tools,
            stream,
            temperature: config.temperature,
            stop_sequences: config.stop_sequences.clone(),
        }
    }

    async fn send_request(
        &self,
        request: &AnthropicRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        // 把单次 HTTP 尝试包进重试/退避策略（§4.8）。闭包按引用捕获
        // `self` 与 `request`；每次重试构造新的 future 重发同一个 POST。
        // 流式阶段的错误不走这条路（只在响应体开始流式传输之前重试）。
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

    /// 远端分页拉取 /v1/models。任一页失败即整体失败（fail-open 在调用方）。
    async fn fetch_remote_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut all = Vec::new();
        let mut after_id: Option<String> = None;
        const MAX_PAGES: u32 = 50;
        let mut pages = 0u32;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                tracing::warn!("anthropic list_models 超过 {MAX_PAGES} 页，停止分页");
                break;
            }
            let mut url = format!("{}/v1/models?limit=1000", self.base_url);
            if let Some(after) = &after_id {
                url.push_str("&after_id=");
                url.push_str(after);
            }
            let response = crate::retry::with_retry(|| self.get_models_page(&url)).await?;
            let json: Value = response
                .json()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            let (page_models, has_more, last_id) = parse_anthropic_models_page(&json);
            all.extend(page_models.into_iter().map(|(id, name)| ModelInfo {
                id,
                name,
                provider: self.provider_id.clone(),
                context_window: 0,
                max_output_tokens: 0,
            }));
            if !has_more {
                break;
            }
            match last_id {
                Some(id) => after_id = Some(id),
                None => break,
            }
        }
        Ok(all)
    }

    async fn get_models_page(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let mut request = self
            .client
            .get(url)
            .header("anthropic-version", "2023-06-01");
        if !self.api_key.is_empty() {
            request = request.header("x-api-key", &self.api_key);
        }
        let response = request
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

    fn fallback_models(config_models: &[ModelEntry], provider_id: &str) -> Vec<ModelInfo> {
        if !config_models.is_empty() {
            return config_models
                .iter()
                .map(|m| crate::models::entry_to_model_info(m, provider_id))
                .collect();
        }
        vec![
            ModelInfo {
                id: "claude-sonnet-4-6".to_string(),
                name: "Claude Sonnet 4.6".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-opus-4".to_string(),
                name: "Claude Opus 4".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-haiku-3-5".to_string(),
                name: "Claude Haiku 3.5".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
        ]
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        match self.fetch_remote_models().await {
            Ok(remote) => Ok(crate::models::merge_models(
                remote,
                &self.config_models,
                &self.provider_id,
            )),
            Err(e) => {
                tracing::warn!("anthropic list_models 远端拉取失败({e})，回退配置 models");
                Ok(Self::fallback_models(
                    &self.config_models,
                    &self.provider_id,
                ))
            }
        }
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

        tracing::info!(
            "chat_stream: {} messages, roles: {:?}",
            anthropic_messages.len(),
            anthropic_messages
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>()
        );
        for (i, m) in anthropic_messages.iter().enumerate() {
            let preview = if m.content.is_string() {
                m.content
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>()
            } else {
                m.content.to_string().chars().take(120).collect::<String>()
            };
            tracing::info!("  msg[{}] role={} preview={:?}", i, m.role, preview);
        }

        let request = Self::build_request(
            model,
            anthropic_messages,
            anthropic_tools,
            system,
            config,
            Some(true),
        );

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

        let request = Self::build_request(
            model,
            anthropic_messages,
            anthropic_tools,
            system,
            config,
            None,
        );

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
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            ..ChatMessage::new(ChatRole::Assistant, text_content)
        })
    }
}

/// 解析一页 /v1/models 响应：(id, name) 列表、has_more、last_id。
fn parse_anthropic_models_page(json: &Value) -> (Vec<(String, String)>, bool, Option<String>) {
    let mut models = Vec::new();
    if let Some(data) = json.get("data").and_then(|v| v.as_array()) {
        for m in data {
            let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = m
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            models.push((id.to_string(), name));
        }
    }
    let has_more = json
        .get("has_more")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let last_id = json
        .get("last_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    (models, has_more, last_id)
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

        // 处理完整的 SSE 事件（以空行分隔）
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
                                stop_reason: ProviderStopReason::from_anthropic(stop_reason),
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

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::DetailedModelEntry;
    use serde_json::json;

    #[test]
    fn parse_models_page_extracts_ids_and_pagination() {
        let json = json!({
            "data": [
                {"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "type": "model"},
                {"id": "claude-opus-4", "display_name": "Claude Opus 4", "type": "model"}
            ],
            "has_more": true,
            "last_id": "claude-opus-4"
        });
        let (models, has_more, last_id) = parse_anthropic_models_page(&json);
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0],
            (
                "claude-sonnet-4-6".to_string(),
                "Claude Sonnet 4.6".to_string()
            )
        );
        assert!(has_more);
        assert_eq!(last_id.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn parse_models_page_end_of_pages() {
        let json = json!({"data": [{"id": "m1"}], "has_more": false});
        let (models, has_more, last_id) = parse_anthropic_models_page(&json);
        assert_eq!(models.len(), 1);
        assert!(!has_more);
        assert_eq!(last_id, None);
        // display_name 缺省回退 id
        assert_eq!(models[0].1, "m1");
    }

    #[test]
    fn fallback_prefers_config_models() {
        let entries = vec![ModelEntry::Detailed(DetailedModelEntry {
            id: "deepseek-v4-flash[1m]".into(),
            name: None,
            context_window: Some(1_000_000),
            max_output_tokens: Some(8192),
            thinking: None,
            reasoning_effort: None,
        })];
        let models = AnthropicProvider::fallback_models(&entries, "claude-proxy");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-v4-flash[1m]");
        assert_eq!(models[0].context_window, 1_000_000);
        assert_eq!(models[0].provider, "claude-proxy");
    }

    #[test]
    fn fallback_hardcoded_when_config_empty() {
        let models = AnthropicProvider::fallback_models(&[], "anthropic");
        assert_eq!(models.len(), 3);
        assert!(models.iter().all(|m| m.provider == "anthropic"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
    }
}
