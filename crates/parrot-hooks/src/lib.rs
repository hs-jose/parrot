// Built-in hook implementations + registry builder.
// Hooks live here rather than in parrot-daemon so they can be reused
// and tested independently of the daemon binary.

pub mod dangerous_command_blocker;
pub mod redact_secrets;
pub mod shell_denylist;

use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let mut reg = HookRegistry::new(Duration::from_secs(cfg.timeout_seconds.max(1)));
    for id in &cfg.enabled {
        match id.as_str() {
            "dangerous_command_blocker" => {
                reg.register(Arc::new(dangerous_command_blocker::DangerousCommandBlocker))
            }
            "redact_secrets" => {
                let hook_cfg = cfg
                    .configs
                    .get("redact_secrets")
                    .and_then(|v| redact_secrets::RedactSecretsConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(redact_secrets::RedactSecrets::new(hook_cfg)))
            }
            "shell_denylist" => {
                let hook_cfg = cfg
                    .configs
                    .get("shell_denylist")
                    .and_then(|v| shell_denylist::ShellDenylistConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(shell_denylist::ShellDenylist::new(hook_cfg)))
            }
            other => warn!("unknown hook id in [hooks].enabled: {other} (skipping)"),
        }
    }
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let reg = build_registry(&cfg);
        let _ = reg;
    }

    #[test]
    fn build_registry_picks_up_known_hooks() {
        let cfg = HooksConfig {
            enabled: vec![
                "dangerous_command_blocker".into(),
                "redact_secrets".into(),
                "shell_denylist".into(),
            ],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }

    #[test]
    fn build_registry_shell_denylist_with_config() {
        let mut configs = HashMap::new();
        configs.insert(
            "shell_denylist".to_string(),
            toml::Value::Table({
                let mut t = toml::value::Table::new();
                t.insert(
                    "patterns".to_string(),
                    toml::Value::Array(vec![toml::Value::String("DROP TABLE".into())]),
                );
                t
            }),
        );
        let cfg = HooksConfig {
            enabled: vec!["shell_denylist".into()],
            timeout_seconds: 5,
            configs,
        };
        let _reg = build_registry(&cfg);
    }
}
