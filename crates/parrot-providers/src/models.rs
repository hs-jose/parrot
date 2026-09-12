//! list_models 配置元数据合并（spec §3.1）。
//!
//! 远端 `GET /models` 拿不到 context window，配置是唯一可靠来源：
//! 以远端结果为基底，配置条目按 id 覆盖（Detailed 条目逐字段覆盖 Some
//! 字段）；配置独有条目追加。

use parrot_config::ModelEntry;
use parrot_core::types::ModelInfo;

pub fn entry_to_model_info(entry: &ModelEntry, provider_id: &str) -> ModelInfo {
    match entry {
        ModelEntry::Simple(id) => ModelInfo {
            id: id.clone(),
            name: id.clone(),
            provider: provider_id.to_string(),
            context_window: 0,
            max_output_tokens: 0,
        },
        ModelEntry::Detailed(d) => ModelInfo {
            id: d.id.clone(),
            name: d.name.clone().unwrap_or_else(|| d.id.clone()),
            provider: provider_id.to_string(),
            context_window: d.context_window.unwrap_or(0),
            max_output_tokens: d.max_output_tokens.unwrap_or(0),
        },
    }
}

pub fn merge_models(
    remote: Vec<ModelInfo>,
    config_models: &[ModelEntry],
    provider_id: &str,
) -> Vec<ModelInfo> {
    let mut result = remote;
    for entry in config_models {
        let id = entry.id().to_string();
        match result.iter_mut().find(|m| m.id == id) {
            Some(existing) => {
                if let ModelEntry::Detailed(d) = entry {
                    if let Some(name) = &d.name {
                        existing.name = name.clone();
                    }
                    if let Some(cw) = d.context_window {
                        existing.context_window = cw;
                    }
                    if let Some(mot) = d.max_output_tokens {
                        existing.max_output_tokens = mot;
                    }
                }
            }
            None => result.push(entry_to_model_info(entry, provider_id)),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::config::DetailedModelEntry;

    fn remote(id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            provider: "p".into(),
            context_window: 0,
            max_output_tokens: 0,
        }
    }

    fn detailed(id: &str, cw: Option<u32>, mot: Option<u32>) -> ModelEntry {
        ModelEntry::Detailed(DetailedModelEntry {
            id: id.to_string(),
            name: None,
            context_window: cw,
            max_output_tokens: mot,
            thinking: None,
            reasoning_effort: None,
        })
    }

    #[test]
    fn merge_overrides_config_metadata() {
        let result = merge_models(
            vec![remote("m1", "Remote Name")],
            &[detailed("m1", Some(1_000_000), Some(8192))],
            "p",
        );
        assert_eq!(result[0].name, "Remote Name");
        assert_eq!(result[0].context_window, 1_000_000);
        assert_eq!(result[0].max_output_tokens, 8192);
    }

    #[test]
    fn simple_entry_does_not_override_remote() {
        let result = merge_models(
            vec![remote("m1", "Remote Name")],
            &[ModelEntry::Simple("m1".into())],
            "p",
        );
        assert_eq!(result[0].name, "Remote Name");
        assert_eq!(result[0].context_window, 0);
    }

    #[test]
    fn merge_appends_config_only_entries() {
        let result = merge_models(
            vec![remote("m1", "M1")],
            &[
                ModelEntry::Simple("m2".into()),
                detailed("m3", Some(200_000), None),
            ],
            "p",
        );
        assert_eq!(result.len(), 3);
        assert_eq!(result[1].id, "m2");
        assert_eq!(result[1].name, "m2");
        assert_eq!(result[1].context_window, 0);
        assert_eq!(result[2].id, "m3");
        assert_eq!(result[2].context_window, 200_000);
        assert_eq!(result[2].max_output_tokens, 0);
    }

    #[test]
    fn entry_to_model_info_fills_defaults() {
        let info = entry_to_model_info(&ModelEntry::Simple("m".into()), "prov");
        assert_eq!(info.name, "m");
        assert_eq!(info.provider, "prov");
        assert_eq!(info.context_window, 0);
    }
}
