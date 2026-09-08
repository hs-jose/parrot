use crate::error::ProviderError;
use crate::tool::ToolDefinition;
use crate::types::{ChatMessage, GenerateConfig, ModelInfo};
use async_trait::async_trait;
use parrot_protocol::types::Usage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError>;
    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError>;
}

pub struct ChatStream {
    pub inner: tokio::sync::mpsc::Receiver<ProviderStreamEvent>,
}

/// provider 适配器解析流式响应（如 Anthropic SSE）时发出的子事件。
/// 只被引擎消费，由引擎包上 `AgentEvent` 生命周期信封后再转发。
///
/// 注意：工具结果与工具确认请求不在这里——前者由引擎执行工具后产生，
/// 后者是引擎针对命中 `require_confirmation` 的工具做出的策略决策。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderStreamEvent {
    TextDelta {
        delta: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        args_delta: String,
    },
    ToolCallEnd {
        id: String,
        arguments: serde_json::Value,
    },
    Finish {
        stop_reason: ProviderStopReason,
        usage: Usage,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl ProviderStopReason {
    pub fn from_anthropic(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }
}

impl From<ProviderStopReason> for parrot_protocol::agent_event::MessageStopReason {
    fn from(value: ProviderStopReason) -> Self {
        match value {
            ProviderStopReason::EndTurn => Self::EndTurn,
            ProviderStopReason::ToolUse => Self::ToolUse,
            ProviderStopReason::MaxTokens => Self::MaxTokens,
        }
    }
}

pub struct ProviderRegistry {
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    model_to_provider: RwLock<HashMap<String, String>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            model_to_provider: RwLock::new(HashMap::new()),
        }
    }
    pub async fn register(&self, provider: Arc<dyn LlmProvider>, models: Vec<String>) {
        let id = provider.provider_id().to_string();
        {
            let mut map = self.model_to_provider.write().await;
            for model in &models {
                map.insert(model.clone(), id.clone());
            }
        }
        self.providers.write().await.insert(id, provider);
    }
    pub async fn get(&self, provider_id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.read().await.get(provider_id).cloned()
    }
    pub async fn resolve(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        let providers = self.providers.read().await;
        let map = self.model_to_provider.read().await;
        if let Some(provider_id) = map.get(model) {
            return providers.get(provider_id).cloned();
        }
        None
    }
    /// 枚举所有已注册 provider 的 id。daemon 聚合各 provider 的
    /// `list_models()` 响应 `ClientMessage::ListModels` 时使用。
    pub async fn provider_ids(&self) -> Vec<String> {
        self.providers.read().await.keys().cloned().collect()
    }
}
