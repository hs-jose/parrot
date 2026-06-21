use parrot_config::AppConfig;
use parrot_core::provider::ProviderRegistry;
use std::sync::Arc;

pub mod anthropic;

pub async fn register_all(registry: &ProviderRegistry, config: &AppConfig) {
    for provider_config in &config.providers {
        match provider_config.api_type.as_str() {
            "anthropic" => {
                let provider = crate::providers::anthropic::AnthropicProvider::new(
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.default_model.clone(),
                );
                let models = if provider_config.models.is_empty() {
                    vec![provider_config.default_model.clone()]
                } else {
                    provider_config.models.clone()
                };
                registry.register(Arc::new(provider), models).await;
            }
            "openai" => {
                tracing::warn!("OpenAI provider not yet implemented, skipping");
            }
            other => {
                tracing::warn!("Unknown api_type: {}, skipping", other);
            }
        }
    }
}