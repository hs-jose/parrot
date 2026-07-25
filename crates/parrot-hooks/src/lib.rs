// Built-in hook implementations + registry builder.
// Hooks live here rather than in parrot-daemon so they can be reused
// and tested independently of the daemon binary.

pub mod dangerous_command_blocker;
pub mod redact_secrets;

use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
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
            "redact_secrets" => reg.register(Arc::new(redact_secrets::RedactSecrets)),
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
            enabled: vec!["dangerous_command_blocker".into(), "redact_secrets".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }
}
