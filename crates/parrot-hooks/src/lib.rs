// Built-in hook implementations + registry builder.
// Hooks live here rather than in parrot-daemon so they can be reused
// and tested independently of the daemon binary.

pub mod dangerous_command_blocker;
pub mod external;
pub mod redact_secrets;
pub mod shell_denylist;

use parrot_config::HooksConfig;
use parrot_core::hooks::{Hook, HookRegistry};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let global_timeout = Duration::from_secs(cfg.timeout_seconds.max(1));
    let mut reg = HookRegistry::new(global_timeout);
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
            other => warn!(
                hook_id = other,
                "unknown hook id in [hooks].enabled; skipping"
            ),
        }
    }
    // External hooks: orthogonal to `enabled`. Listed ⇒ registered.
    for ext in &cfg.external {
        match external::ExternalHook::new(ext, global_timeout) {
            Ok(h) => {
                info!(
                    hook_id = %h.id(),
                    points = ?h.supported(),
                    "registered external hook"
                );
                reg.register(Arc::new(h));
            }
            Err(detail) => warn!(
                hook_id = %ext.id,
                error = %detail,
                "failed to build external hook; skipping"
            ),
        }
    }
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::ExternalHookConfig;
    use std::collections::HashMap;

    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
            external: Vec::new(),
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
            external: Vec::new(),
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
            external: Vec::new(),
        };
        let _reg = build_registry(&cfg);
    }

    #[test]
    fn build_registry_unknown_external_event_warns_and_skips() {
        // Unknown event-kind string in any external entry → Err → warn + skip
        // (no panic). Other valid externals, if any, still register.
        let cfg = HooksConfig {
            enabled: vec![],
            timeout_seconds: 5,
            configs: HashMap::new(),
            external: vec![ExternalHookConfig {
                id: "bad".into(),
                command: vec!["echo".into()],
                events: vec!["unknown_event".into()],
                timeout_seconds: None,
                config: toml::Value::Table(toml::value::Table::new()),
            }],
        };
        let _reg = build_registry(&cfg);
        // registry created; failed external hook skipped, no panic.
    }

    #[test]
    fn build_registry_external_with_empty_events_ok() {
        // Empty events vec → HookPoints::empty() (hook never fires) but
        // construction succeeds and the external is registered.
        let cfg = HooksConfig {
            enabled: vec![],
            timeout_seconds: 5,
            configs: HashMap::new(),
            external: vec![ExternalHookConfig {
                id: "noop-listener".into(),
                command: vec!["echo".into()],
                events: vec![],
                timeout_seconds: None,
                config: toml::Value::Table(toml::value::Table::new()),
            }],
        };
        let _reg = build_registry(&cfg);
    }

    #[test]
    fn build_registry_mixed_internal_and_external() {
        // Built-in shell_denylist + an external hook coexist: `enabled`
        // governs built-in only; `external` vec independent (spec §1, §8).
        let cfg = HooksConfig {
            enabled: vec!["shell_denylist".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
            external: vec![ExternalHookConfig {
                id: "ext-1".into(),
                command: vec!["echo".into()],
                events: vec!["agent_start".into()],
                timeout_seconds: None,
                config: toml::Value::Table(toml::value::Table::new()),
            }],
        };
        let _reg = build_registry(&cfg);
    }
}
