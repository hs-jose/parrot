use async_trait::async_trait;
use parrot_core::hooks::{Hook, HookCtx, HookEvent, HookPoints, HookResult};
use parrot_core::AgentError;
use regex::Regex;
use std::sync::OnceLock;

static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();

fn patterns() -> &'static [(Regex, &'static str)] {
    PATTERNS.get_or_init(|| {
        vec![
            (Regex::new(r"rm\s+-rf\s+/").unwrap(), "rm -rf /"),
            (
                Regex::new(r":\(\)\s*\{\s*:\|:&\s*\};\s*:").unwrap(),
                "fork bomb",
            ),
            (Regex::new(r"chmod\s+-R\s+777\s+/").unwrap(), "chmod 777 /"),
            (
                Regex::new(r#"\b(sh|bash|zsh)\s+-c\s+['\"].*\|\s*(sh|bash|zsh)\b"#).unwrap(),
                "reverse shell",
            ),
            (
                Regex::new(r">\s*/dev/sda").unwrap(),
                "writing to raw disk device",
            ),
        ]
    })
}

pub struct DangerousCommandBlocker;

#[async_trait]
impl Hook for DangerousCommandBlocker {
    fn id(&self) -> &'static str {
        "dangerous_command_blocker"
    }
    fn supported(&self) -> HookPoints {
        HookPoints::TOOL_CALL
    }
    async fn dispatch(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookResult, AgentError> {
        if let HookEvent::ToolCall {
            tool_name,
            arguments,
            ..
        } = ev
        {
            if matches!(tool_name, "shell_exec" | "bash" | "shell") {
                if let Some(cmd) = arguments.get("command").and_then(|v| v.as_str()) {
                    for (re, label) in patterns() {
                        if re.is_match(cmd) {
                            return Ok(HookResult::Block {
                                reason: format!("dangerous command: {label}"),
                            });
                        }
                    }
                }
            }
        }
        Ok(HookResult::NoOp)
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

    #[tokio::test]
    async fn blocks_rm_rf_root() {
        let h = DangerousCommandBlocker;
        let args = serde_json::json!({"command": "rm -rf /"});
        let ev = HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "shell_exec",
            arguments: &args,
        };
        let out = h.dispatch(ev, &ctx()).await.unwrap();
        assert!(matches!(out, HookResult::Block { .. }));
    }

    #[tokio::test]
    async fn passes_safe_command() {
        let h = DangerousCommandBlocker;
        let args = serde_json::json!({"command": "ls -la"});
        let ev = HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "shell_exec",
            arguments: &args,
        };
        let out = h.dispatch(ev, &ctx()).await.unwrap();
        assert!(matches!(out, HookResult::NoOp));
    }

    #[tokio::test]
    async fn ignores_non_shell_tool() {
        let h = DangerousCommandBlocker;
        let ev = HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "read",
            arguments: &serde_json::Value::Null,
        };
        let out = h.dispatch(ev, &ctx()).await.unwrap();
        assert!(matches!(out, HookResult::NoOp));
    }

    #[tokio::test]
    async fn blocks_rm_rf_system_dir() {
        let h = DangerousCommandBlocker;
        for cmd in ["rm -rf /etc", "rm -rf /var", "rm -rf /usr/local"] {
            let args = serde_json::json!({"command": cmd});
            let ev = HookEvent::ToolCall {
                session_id: Uuid::nil(),
                turn_id: Uuid::nil(),
                parent_message_id: Uuid::nil(),
                tool_call_id: "x",
                tool_name: "shell_exec",
                arguments: &args,
            };
            let out = h.dispatch(ev, &ctx()).await.unwrap();
            assert!(
                matches!(out, HookResult::Block { .. }),
                "{cmd} must be blocked"
            );
        }
    }
}
