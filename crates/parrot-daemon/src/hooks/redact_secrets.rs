use async_trait::async_trait;
use parrot_core::hooks::{Hook, HookCtx, HookEvent, HookPoints, HookResult};
use parrot_core::AgentError;
use regex::Regex;
use std::sync::OnceLock;

static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

fn patterns() -> &'static [Regex] {
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            Regex::new(r"ghp_[A-Za-z0-9]{36}").unwrap(),
            Regex::new(r"sk-ant-[A-Za-z0-9_\-]{20,}").unwrap(),
            Regex::new(r"-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----").unwrap(),
        ]
    })
}

pub struct RedactSecrets;

#[async_trait]
impl Hook for RedactSecrets {
    fn id(&self) -> &'static str {
        "redact_secrets"
    }
    fn supported(&self) -> HookPoints {
        HookPoints::TOOL_RESULT
    }
    async fn dispatch(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookResult, AgentError> {
        if let HookEvent::ToolResult {
            tool_name, result, ..
        } = ev
        {
            if matches!(
                tool_name,
                "read" | "shell_exec" | "bash" | "shell" | "file_read"
            ) {
                let mut content = result.content.clone();
                let mut changed = false;
                for re in patterns() {
                    let new = re.replace_all(&content, "[REDACTED]").to_string();
                    if new != content {
                        changed = true;
                        content = new;
                    }
                }
                if changed {
                    return Ok(HookResult::ReplaceResult {
                        content,
                        is_error: result.is_error,
                    });
                }
            }
        }
        Ok(HookResult::NoOp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_protocol::types::ToolOutput;
    use uuid::Uuid;

    fn ctx() -> HookCtx<'static> {
        HookCtx {
            session_id: Uuid::nil(),
            working_dir: std::path::Path::new("."),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn redacts_aws_key() {
        let h = RedactSecrets;
        let out = ToolOutput {
            content: "key AKIAIOSFODNN7EXAMPLE lost".into(),
            is_error: false,
        };
        let ev = HookEvent::ToolResult {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "read",
            input: &serde_json::Value::Null,
            result: &out,
        };
        let res = h.dispatch(ev, &ctx()).await.unwrap();
        match res {
            HookResult::ReplaceResult { content, is_error } => {
                assert!(content.contains("[REDACTED]"));
                assert!(!content.contains("AKIAIOSFODNN7EXAMPLE"));
                assert!(!is_error);
            }
            _ => panic!("expected ReplaceResult"),
        }
    }

    #[tokio::test]
    async fn no_change_when_clean() {
        let h = RedactSecrets;
        let out = ToolOutput {
            content: "just some normal code".into(),
            is_error: false,
        };
        let ev = HookEvent::ToolResult {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "read",
            input: &serde_json::Value::Null,
            result: &out,
        };
        let res = h.dispatch(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookResult::NoOp));
    }
}
