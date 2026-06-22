use crate::error::ProviderError;
use crate::tool::ToolDefinition;
use crate::types::{ChatMessage, GenerateConfig, ModelInfo};
use async_trait::async_trait;
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
    pub inner: tokio::sync::mpsc::Receiver<crate::event_log::StreamEvent>,
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
    /// Enumerate the ids of all registered providers. Used by the daemon to
    /// aggregate `list_models()` across providers for `ClientMessage::ListModels`.
    pub async fn provider_ids(&self) -> Vec<String> {
        self.providers.read().await.keys().cloned().collect()
    }
}
