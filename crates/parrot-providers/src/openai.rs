use async_trait::async_trait;
use parrot_config::ModelEntry;
use parrot_core::error::ProviderError;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderStopReason};
use parrot_core::tool::ToolDefinition;
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig, ModelInfo, ToolCallInfo};
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
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
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
    ) -> OpenAiRequest {
        OpenAiRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            tools: Self::convert_tools(tools),
            stream,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            stop: config.stop_sequences.clone(),
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

#[allow(dead_code)]
fn stop_reason_from_openai(reason: &str) -> ProviderStopReason {
    match reason {
        "tool_calls" => ProviderStopReason::ToolUse,
        "length" => ProviderStopReason::MaxTokens,
        _ => ProviderStopReason::EndTurn,
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // Task 5 换成远端拉取+合并；先按配置列表直接构造（fail-open 语义一致）
        Ok(self
            .config_models
            .iter()
            .map(|m| crate::models::entry_to_model_info(m, &self.provider_id))
            .collect())
    }

    async fn chat_stream(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        // Task 5 实现
        Err(ProviderError::Api {
            status: 501,
            body: "chat_stream not implemented yet".into(),
        })
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        let request = Self::build_request(model, messages, tools, config, None);
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
        let req = OpenAiProvider::build_request("gpt-5", &messages, &[], &config, Some(true));
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["model"], "gpt-5");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["stop"], json!(["STOP"]));
        assert!(value.get("tools").is_none());
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
}
