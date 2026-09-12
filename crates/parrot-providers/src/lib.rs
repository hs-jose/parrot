use parrot_config::{AppConfig, ModelEntry};
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
        for entry in &provider_config.models {
            let ModelEntry::Detailed(d) = entry else {
                continue;
            };
            match provider_config.protocol.as_str() {
                "anthropic" if d.reasoning_effort.is_some() => {
                    tracing::warn!(
                        "provider {}: reasoning_effort 仅 openai 协议生效，已忽略",
                        provider_config.id
                    );
                }
                "openai" if d.thinking.is_some() => {
                    tracing::warn!(
                        "thinking 仅 anthropic 协议生效，已忽略（provider {}）",
                        provider_config.id
                    );
                }
                _ => {}
            }
        }
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

    #[tokio::test]
    async fn cross_protocol_fields_warn_but_register() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        // anthropic 协议配 reasoning_effort：warn 但照常注册
        let mut p = provider("a", "anthropic");
        p.models = vec![parrot_config::ModelEntry::Detailed(
            parrot_config::config::DetailedModelEntry {
                id: "a-model".into(),
                name: None,
                context_window: None,
                max_output_tokens: None,
                thinking: None,
                reasoning_effort: Some(parrot_config::ReasoningEffort::Low),
            },
        )];
        config.providers.push(p);
        // openai 协议配 thinking：同理
        let mut q = provider("b", "openai");
        q.models = vec![parrot_config::ModelEntry::Detailed(
            parrot_config::config::DetailedModelEntry {
                id: "b-model".into(),
                name: None,
                context_window: None,
                max_output_tokens: None,
                thinking: Some(parrot_config::ThinkingConfig {
                    budget_tokens: 1024,
                }),
                reasoning_effort: None,
            },
        )];
        config.providers.push(q);
        register_all(&registry, &config).await;
        assert_eq!(registry.provider_ids().await.len(), 2);
        assert!(registry.resolve("a-model").await.is_some());
        assert!(registry.resolve("b-model").await.is_some());
    }
}
