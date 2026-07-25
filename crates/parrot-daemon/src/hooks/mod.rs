use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

mod dangerous_command_blocker;
mod redact_secrets;

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

    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: std::collections::HashMap::new(),
        };
        let reg = build_registry(&cfg);
        // No panic, just empty handlers (warn logged)
        // (Empty registry fast path: no crashes)
        let _ = reg;
    }

    #[test]
    fn build_registry_picks_up_known_hooks() {
        let cfg = HooksConfig {
            enabled: vec!["dangerous_command_blocker".into(), "redact_secrets".into()],
            timeout_seconds: 5,
            configs: std::collections::HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }
}
