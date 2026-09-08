// 内置 hook 实现 + 注册表构建器。
// hook 放在这里而非 parrot-daemon，以便独立于 daemon 二进制复用与测试。

pub mod dangerous_command_blocker;
pub mod external;
pub mod redact_secrets;
pub mod shell_denylist;

use parrot_config::HooksConfig;
use parrot_core::hooks::{Hook, HookEvent, HookRegistry};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// 从 `ToolCall` 事件提取 shell 命令参数：仅对 shell 类工具
/// （`shell_exec` / `bash` / `shell`）生效，取其 `command` 字符串参数。
/// 非 ToolCall / 非 shell 工具 / 无 command 参数一律返回 `None`。
pub(crate) fn extract_shell_command<'a>(ev: &'a HookEvent<'a>) -> Option<&'a str> {
    let HookEvent::ToolCall {
        tool_name,
        arguments,
        ..
    } = ev
    else {
        return None;
    };
    if !matches!(*tool_name, "shell_exec" | "bash" | "shell") {
        return None;
    }
    arguments.get("command").and_then(|v| v.as_str())
}

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
    // 外部 hook 与 `enabled` 正交：列出来就注册。
    for ext in &cfg.external {
        match external::ExternalHook::new(ext) {
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
        // 任一 external 条目里有未知 event-kind 字符串 → Err → warn + 跳过
        // （不 panic）。其他合法的 external（若有）照常注册。
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
        // 注册表创建成功；失败的 external hook 被跳过，无 panic。
    }

    #[test]
    fn build_registry_external_with_empty_events_ok() {
        // 空 events 列表 → HookPoints::empty()（永不触发），
        // 但构造成功且 external 照常注册。
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
        // 内置 shell_denylist 与外部 hook 共存：`enabled` 只管内置；
        // `external` 列表相互独立（spec §1, §8）。
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
