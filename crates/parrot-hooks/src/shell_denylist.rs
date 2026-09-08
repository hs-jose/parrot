use async_trait::async_trait;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ShellDenylistConfig {
    /// 子串模式（不区分大小写）。命令包含任一模式即被拦截。
    #[serde(default = "ShellDenylistConfig::default_patterns")]
    pub patterns: Vec<String>,
}

impl ShellDenylistConfig {
    fn default_patterns() -> Vec<String> {
        vec![
            "rm -rf /".to_string(),
            "sudo".to_string(),
            "chmod 777".to_string(),
        ]
    }
}

impl Default for ShellDenylistConfig {
    fn default() -> Self {
        Self {
            patterns: Self::default_patterns(),
        }
    }
}

pub struct ShellDenylist {
    cfg: ShellDenylistConfig,
}

impl ShellDenylist {
    pub fn new(cfg: ShellDenylistConfig) -> Self {
        Self { cfg }
    }

    fn is_denied(&self, command: &str) -> bool {
        let command_lower = command.to_lowercase();
        self.cfg
            .patterns
            .iter()
            .any(|p| command_lower.contains(&p.to_lowercase()))
    }
}

#[async_trait]
impl Hook for ShellDenylist {
    fn id(&self) -> &str {
        "shell_denylist"
    }
    fn supported(&self) -> HookPoints {
        HookPoints::TOOL_CALL
    }
    async fn handle(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookAction, parrot_core::AgentError> {
        if let Some(cmd) = crate::extract_shell_command(&ev) {
            if self.is_denied(cmd) {
                return Ok(HookAction::Block {
                    reason: format!("command denied by shell_denylist: {}", cmd),
                });
            }
        }
        Ok(HookAction::NoOp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ctx() -> HookCtx<'static> {
        HookCtx {
            session_id: Uuid::nil(),
            working_dir: std::path::Path::new("."),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    fn tool_call_ev(cmd: &str) -> HookEvent<'static> {
        let args = serde_json::json!({"command": cmd});
        HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "shell_exec",
            arguments: Box::leak(Box::new(args)),
        }
    }

    #[tokio::test]
    async fn blocks_rm_rf_root() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("rm -rf /"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn blocks_sudo() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h
            .handle(tool_call_ev("sudo apt update"), &ctx())
            .await
            .unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn blocks_case_insensitive() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("SUDO ls"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn passes_safe_command() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("ls -la"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[tokio::test]
    async fn ignores_non_shell_tool() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let args = serde_json::json!({"command": "rm -rf /"});
        let ev = HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "read",
            arguments: &args,
        };
        let out = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[tokio::test]
    async fn custom_patterns_block() {
        let cfg = ShellDenylistConfig {
            patterns: vec!["DROP TABLE".to_string()],
        };
        let h = ShellDenylist::new(cfg);
        let out = h
            .handle(tool_call_ev("psql -c 'DROP TABLE users'"), &ctx())
            .await
            .unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn empty_patterns_allows_all() {
        let cfg = ShellDenylistConfig { patterns: vec![] };
        let h = ShellDenylist::new(cfg);
        let out = h.handle(tool_call_ev("rm -rf /"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[test]
    fn default_config_has_three_patterns() {
        let cfg = ShellDenylistConfig::default();
        assert_eq!(cfg.patterns.len(), 3);
        assert!(cfg.patterns.contains(&"rm -rf /".to_string()));
        assert!(cfg.patterns.contains(&"sudo".to_string()));
        assert!(cfg.patterns.contains(&"chmod 777".to_string()));
    }
}
