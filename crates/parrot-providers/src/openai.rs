use async_trait::async_trait;
use parrot_config::{ModelEntry, ReasoningEffort};
use parrot_core::error::ProviderError;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderStopReason, ProviderStreamEvent};
use parrot_core::tool::ToolDefinition;
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig, ModelInfo, ToolCallInfo};
use parrot_protocol::types::Usage;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct OpenAiProvider {
    provider_id: String,
    api_key: String,
    base_url: String,
    config_models: Vec<ModelEntry>,
    client: Client,
}

#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiFunctionCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunctionCall {
    id: String,
    function: OpenAiFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunction {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
}

impl OpenAiResponseMessage {
    fn to_chat_message(&self) -> ChatMessage {
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        if let Some(calls) = &self.tool_calls {
            for tc in calls {
                let arguments = tc
                    .function
                    .arguments
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                tool_calls.push(ToolCallInfo {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments,
                });
            }
        }
        ChatMessage {
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            ..ChatMessage::new(
                ChatRole::Assistant,
                self.content.clone().unwrap_or_default(),
            )
        }
    }
}

impl OpenAiProvider {
    pub fn new(
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        config_models: Vec<ModelEntry>,
    ) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        Self {
            provider_id,
            api_key,
            base_url,
            config_models,
            client: Client::new(),
        }
    }

    /// api_key 为空 ⇒ 不带鉴权 header（Ollama 等本地服务）。
    fn auth_header(api_key: &str) -> Option<String> {
        (!api_key.is_empty()).then(|| format!("Bearer {api_key}"))
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| match m.role {
                ChatRole::System => json!({"role": "system", "content": m.content}),
                ChatRole::User => json!({"role": "user", "content": m.content}),
                ChatRole::Assistant => {
                    let mut obj = json!({"role": "assistant", "content": m.content});
                    if let Some(tool_calls) = &m.tool_calls {
                        let calls: Vec<Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments.to_string(),
                                    }
                                })
                            })
                            .collect();
                        obj["tool_calls"] = Value::Array(calls);
                    }
                    obj
                }
                ChatRole::Tool => {
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    json!({"role": "tool", "tool_call_id": id, "content": m.content})
                }
            })
            .collect()
    }

    fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect()
    }

    fn build_request(
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
        stream: Option<bool>,
        reasoning_effort: Option<&ReasoningEffort>,
    ) -> OpenAiRequest {
        OpenAiRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            tools: Self::convert_tools(tools),
            stream,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            stop: config.stop_sequences.clone(),
            reasoning_effort: reasoning_effort.cloned(),
        }
    }

    async fn send_request(
        &self,
        request: &OpenAiRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        crate::retry::with_retry(|| self.send_request_once(request)).await
    }

    async fn send_request_once(
        &self,
        request: &OpenAiRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut builder = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        if let Some(auth) = Self::auth_header(&self.api_key) {
            builder = builder.header("authorization", auth);
        }
        let response = builder
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
                .map(|s| s.saturating_mul(1000))
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

    async fn fetch_remote_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = crate::retry::with_retry(|| self.get_models_page(&url)).await?;
        let json: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        Ok(parse_openai_models(&json)
            .into_iter()
            .map(|(id, name)| ModelInfo {
                id,
                name,
                provider: self.provider_id.clone(),
                context_window: 0,
                max_output_tokens: 0,
            })
            .collect())
    }

    async fn get_models_page(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let mut builder = self.client.get(url);
        if let Some(auth) = Self::auth_header(&self.api_key) {
            builder = builder.header("authorization", auth);
        }
        let response = builder
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
                .map(|s| s.saturating_mul(1000))
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

fn stop_reason_from_openai(reason: &str) -> ProviderStopReason {
    match reason {
        "tool_calls" => ProviderStopReason::ToolUse,
        "length" => ProviderStopReason::MaxTokens,
        _ => ProviderStopReason::EndTurn,
    }
}

#[derive(Debug)]
struct OpenToolCall {
    index: usize,
    id: String,
    args: String,
}

/// OpenAI Chat Completions SSE 分片聚合器（spec §3.2）。
/// feed 逐 chunk 产出事件；finish 在流结束时收尾（关未闭合的
/// tool_calls、发 Finish）。usage 从任意带 usage 的 chunk 累积，
/// 取不到则 0/0。
#[derive(Debug)]
struct OpenAiStreamAggregator {
    open_calls: Vec<OpenToolCall>,
    stop_reason: Option<ProviderStopReason>,
    usage: Usage,
}

impl OpenAiStreamAggregator {
    fn new() -> Self {
        Self {
            open_calls: Vec::new(),
            stop_reason: None,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        }
    }

    fn end_event(call: OpenToolCall) -> ProviderStreamEvent {
        let arguments = serde_json::from_str::<Value>(&call.args)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        ProviderStreamEvent::ToolCallEnd {
            id: call.id,
            arguments,
        }
    }

    fn close_all(&mut self) -> Vec<ProviderStreamEvent> {
        std::mem::take(&mut self.open_calls)
            .into_iter()
            .map(Self::end_event)
            .collect()
    }

    fn feed(&mut self, chunk: &Value) -> Vec<ProviderStreamEvent> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            self.usage.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
        }
        if let Some(reason) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
        {
            self.stop_reason = Some(stop_reason_from_openai(reason));
            events.extend(self.close_all());
        }
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return events;
        };
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                events.push(ProviderStreamEvent::TextDelta {
                    delta: text.to_string(),
                });
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in calls {
                let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let slot = self.open_calls.iter().position(|c| c.index == index);
                let slot_idx = match slot {
                    Some(i) => i,
                    None => {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        events.push(ProviderStreamEvent::ToolCallStart {
                            id: id.clone(),
                            name,
                        });
                        self.open_calls.push(OpenToolCall {
                            index,
                            id,
                            args: String::new(),
                        });
                        self.open_calls.len() - 1
                    }
                };
                if let Some(args) = tc
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    let call = &mut self.open_calls[slot_idx];
                    call.args.push_str(args);
                    events.push(ProviderStreamEvent::ToolCallDelta {
                        id: call.id.clone(),
                        args_delta: args.to_string(),
                    });
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<ProviderStreamEvent> {
        let mut events = self.close_all();
        events.push(ProviderStreamEvent::Finish {
            stop_reason: self
                .stop_reason
                .take()
                .unwrap_or(ProviderStopReason::EndTurn),
            usage: std::mem::take(&mut self.usage),
        });
        events
    }
}

async fn parse_sse_stream(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
) -> Result<(), ProviderError> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut aggregator = OpenAiStreamAggregator::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::StreamError(e.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(event_end) = buffer.find("\n\n") {
            let event_text = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            for line in event_text.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(json) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(msg) = extract_stream_error(&json) {
                    tracing::error!("OpenAI stream error: {}", msg);
                    return Ok(());
                }
                for event in aggregator.feed(&json) {
                    let _ = tx.send(event).await;
                }
            }
        }
    }

    for event in aggregator.finish() {
        let _ = tx.send(event).await;
    }
    Ok(())
}

/// 解析 GET /models 响应：(id, name) 列表。openai 无 display name，name 取 id。
fn parse_openai_models(json: &Value) -> Vec<(String, String)> {
    json.get("data")
        .and_then(|v| v.as_array())
        .map(|data| {
            data.iter()
                .filter_map(|m| {
                    m.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| (id.to_string(), id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 识别流中的 error 事件体（部分兼容服务流中发 JSON error）。
fn extract_stream_error(json: &Value) -> Option<String> {
    let error = json.get("error")?;
    Some(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown stream error")
            .to_string(),
    )
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
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
                tracing::warn!("openai list_models 远端拉取失败({e})，回退配置 models");
                Ok(self
                    .config_models
                    .iter()
                    .map(|m| crate::models::entry_to_model_info(m, &self.provider_id))
                    .collect())
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
        let reasoning_effort = crate::models::find_detailed(&self.config_models, model)
            .and_then(|d| d.reasoning_effort.as_ref());
        let request =
            Self::build_request(model, messages, tools, config, Some(true), reasoning_effort);
        tracing::info!(
            "openai chat_stream: provider={}, model={}, messages={}",
            self.provider_id,
            model,
            messages.len()
        );
        let response = self.send_request(&request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            if let Err(e) = parse_sse_stream(response, tx).await {
                tracing::error!("openai SSE stream error: {}", e);
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
        let reasoning_effort = crate::models::find_detailed(&self.config_models, model)
            .and_then(|d| d.reasoning_effort.as_ref());
        let request = Self::build_request(model, messages, tools, config, None, reasoning_effort);
        let response = self.send_request(&request).await?;
        let body: OpenAiChatResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let choice = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Api {
                status: 200,
                body: "empty choices".into(),
            })?;
        Ok(choice.message.to_chat_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage::new(role, content)
    }

    #[test]
    fn convert_messages_maps_all_roles() {
        let messages = vec![
            msg(ChatRole::System, "sys"),
            msg(ChatRole::User, "hello"),
            {
                let mut m = msg(ChatRole::Assistant, "");
                m.tool_calls = Some(vec![ToolCallInfo {
                    id: "call_1".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "hi"}),
                }]);
                m
            },
            {
                let mut m = msg(ChatRole::Tool, "tool output");
                m.tool_call_id = Some("call_1".into());
                m
            },
        ];
        let out = OpenAiProvider::convert_messages(&messages);
        assert_eq!(out[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(out[1], json!({"role": "user", "content": "hello"}));
        assert_eq!(
            out[2],
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{\"text\":\"hi\"}"}
                }]
            })
        );
        assert_eq!(
            out[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "tool output"})
        );
    }

    #[test]
    fn convert_tools_maps_function_format() {
        let tools = vec![ToolDefinition {
            name: "echo".into(),
            description: "Echo a message".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let out = OpenAiProvider::convert_tools(&tools);
        assert_eq!(
            out[0],
            json!({
                "type": "function",
                "function": {
                    "name": "echo",
                    "description": "Echo a message",
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        );
    }

    #[test]
    fn build_request_serializes_expected_shape() {
        let config = GenerateConfig {
            model: "gpt-5".into(),
            temperature: Some(0.7),
            max_tokens: Some(1024),
            stop_sequences: Some(vec!["STOP".into()]),
        };
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request("gpt-5", &messages, &[], &config, Some(true), None);
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["model"], "gpt-5");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["stop"], json!(["STOP"]));
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn build_request_injects_reasoning_effort_when_present() {
        use parrot_config::ReasoningEffort;
        let config = GenerateConfig::default();
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request(
            "gpt-5",
            &messages,
            &[],
            &config,
            None,
            Some(&ReasoningEffort::High),
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_omits_reasoning_effort_when_absent() {
        let config = GenerateConfig::default();
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request("gpt-5", &messages, &[], &config, None, None);
        let value = serde_json::to_value(&req).unwrap();
        assert!(value.get("reasoning_effort").is_none());
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            stop_reason_from_openai("tool_calls"),
            ProviderStopReason::ToolUse
        );
        assert_eq!(stop_reason_from_openai("stop"), ProviderStopReason::EndTurn);
        assert_eq!(
            stop_reason_from_openai("length"),
            ProviderStopReason::MaxTokens
        );
        assert_eq!(
            stop_reason_from_openai("weird"),
            ProviderStopReason::EndTurn
        );
    }

    #[test]
    fn parse_chat_response_maps_content_and_tool_calls() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "doing it",
                    "tool_calls": [
                        {"id": "call_a", "type": "function",
                         "function": {"name": "echo", "arguments": "{\"x\": 1}"}},
                        {"id": "call_b", "type": "function",
                         "function": {"name": "ls", "arguments": "{}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let parsed: OpenAiChatResponse = serde_json::from_value(body).expect("deserialize");
        let msg = parsed.choices[0].message.to_chat_message();
        assert_eq!(msg.content, "doing it");
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].id, "call_a");
        assert_eq!(tcs[0].name, "echo");
        assert_eq!(tcs[0].arguments, json!({"x": 1}));
        assert_eq!(tcs[1].arguments, json!({}));
    }

    #[test]
    fn auth_header_bearer_or_none() {
        assert_eq!(
            OpenAiProvider::auth_header("sk-abc"),
            Some("Bearer sk-abc".to_string())
        );
        assert_eq!(OpenAiProvider::auth_header(""), None);
    }

    #[test]
    fn aggregator_streams_text_then_finish() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"content":"Hel"}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"content":"lo"}}]})));
        events.extend(
            agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2}})),
        );
        events.extend(agg.finish());
        assert_eq!(
            events,
            vec![
                ProviderStreamEvent::TextDelta {
                    delta: "Hel".into()
                },
                ProviderStreamEvent::TextDelta { delta: "lo".into() },
                ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 7,
                        output_tokens: 2
                    },
                }
            ]
        );
    }

    #[test]
    fn aggregator_aggregates_parallel_tool_calls() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"echo","arguments":""}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"x\":"}}
        ]}}]})));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":1,"id":"call_b","function":{"name":"ls","arguments":"{\"path\":\".\""}}
        ]}}]})));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"1}"}},
            {"index":1,"function":{"arguments":",\"depth\":2}"}}
        ]}}]})));
        events.extend(agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        events.extend(agg.finish());
        assert_eq!(
            events,
            vec![
                ProviderStreamEvent::ToolCallStart {
                    id: "call_a".into(),
                    name: "echo".into()
                },
                ProviderStreamEvent::ToolCallDelta {
                    id: "call_a".into(),
                    args_delta: "{\"x\":".into()
                },
                ProviderStreamEvent::ToolCallStart {
                    id: "call_b".into(),
                    name: "ls".into()
                },
                ProviderStreamEvent::ToolCallDelta {
                    id: "call_b".into(),
                    args_delta: "{\"path\":\".\"".into()
                },
                ProviderStreamEvent::ToolCallDelta {
                    id: "call_a".into(),
                    args_delta: "1}".into()
                },
                ProviderStreamEvent::ToolCallDelta {
                    id: "call_b".into(),
                    args_delta: ",\"depth\":2}".into()
                },
                ProviderStreamEvent::ToolCallEnd {
                    id: "call_a".into(),
                    arguments: json!({"x": 1}),
                },
                ProviderStreamEvent::ToolCallEnd {
                    id: "call_b".into(),
                    arguments: json!({"path": ".", "depth": 2}),
                },
                ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0
                    },
                }
            ]
        );
    }

    #[test]
    fn aggregator_parses_bad_arguments_as_empty_object() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"f","arguments":"not-json"}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        events.extend(agg.finish());
        assert!(events.iter().any(|e| matches!(
            e,
            ProviderStreamEvent::ToolCallEnd { arguments, .. }
                if arguments.is_object() && arguments.as_object().unwrap().is_empty()
        )));
    }

    #[test]
    fn parse_openai_models_extracts_data_list() {
        let json = json!({"object":"list","data":[{"id":"gpt-5"},{"id":"deepseek-chat","object":"model"}]});
        let models = parse_openai_models(&json);
        assert_eq!(
            models,
            vec![
                ("gpt-5".to_string(), "gpt-5".to_string()),
                ("deepseek-chat".to_string(), "deepseek-chat".to_string()),
            ]
        );
    }

    #[test]
    fn stream_error_detection() {
        let err = json!({"error": {"message": "rate limit exceeded", "type": "rate_limit_error"}});
        assert_eq!(
            extract_stream_error(&err),
            Some("rate limit exceeded".to_string())
        );
        let ok = json!({"choices":[{"delta":{"content":"x"}}]});
        assert_eq!(extract_stream_error(&ok), None);
        // error 无 message 字段时给兜底文案
        let bare = json!({"error": {"code": 500}});
        assert_eq!(
            extract_stream_error(&bare),
            Some("unknown stream error".to_string())
        );
    }
}
