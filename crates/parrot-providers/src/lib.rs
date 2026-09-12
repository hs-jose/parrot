use parrot_config::AppConfig;
use parrot_core::provider::ProviderRegistry;
use std::sync::Arc;

pub mod anthropic;
pub mod models;
pub mod openai;
pub mod retry;

pub async fn register_all(registry: &ProviderRegistry, config: &AppConfig) {
    for provider_config in &config.providers {
        let models = if provider_config.models.is_empty() {
            vec![provider_config.default_model.clone()]
        } else {
            provider_config
                .models
                .iter()
                .map(|m| m.id().to_string())
                .collect()
        };
        match provider_config.protocol.as_str() {
            "anthropic" => {
                let provider = crate::anthropic::AnthropicProvider::new(
                    provider_config.id.clone(),
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.models.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
            "openai" => {
                let provider = crate::openai::OpenAiProvider::new(
                    provider_config.id.clone(),
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.models.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
            other => {
                tracing::warn!(
                    "Unknown protocol: {}, skipping provider {}",
                    other,
                    provider_config.id
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::ProviderConfig;

    fn provider(id: &str, protocol: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            protocol: protocol.to_string(),
            api_key: String::new(),
            default_model: format!("{id}-model"),
            base_url: None,
            models: vec![format!("{id}-model").into()],
        }
    }

    #[tokio::test]
    async fn openai_protocol_registers_openai_provider() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("ds", "openai"));
        register_all(&registry, &config).await;
        let p = registry.resolve("ds-model").await.expect("resolved");
        assert_eq!(p.provider_id(), "ds");
    }

    #[tokio::test]
    async fn anthropic_protocol_registers_anthropic_provider() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("claude-proxy", "anthropic"));
        register_all(&registry, &config).await;
        let p = registry
            .resolve("claude-proxy-model")
            .await
            .expect("resolved");
        assert_eq!(p.provider_id(), "claude-proxy");
    }

    #[tokio::test]
    async fn unknown_protocol_is_skipped() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("x", "gemini"));
        register_all(&registry, &config).await;
        assert!(registry.provider_ids().await.is_empty());
    }
}
