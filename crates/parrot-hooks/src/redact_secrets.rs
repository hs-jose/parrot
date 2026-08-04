use async_trait::async_trait;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

struct SecretPattern {
    re: Regex,
    repl: &'static str,
}

fn built_in_patterns() -> &'static [SecretPattern] {
    static PATTERNS: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            SecretPattern {
                re: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                repl: "[REDACTED]",
            },
            SecretPattern {
                re: Regex::new(r"ghp_[A-Za-z0-9]{36}").unwrap(),
                repl: "[REDACTED]",
            },
            SecretPattern {
                re: Regex::new(r"sk-ant-[A-Za-z0-9_\-]{20,}").unwrap(),
                repl: "[REDACTED]",
            },
            SecretPattern {
                re: Regex::new(r"-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----").unwrap(),
                repl: "[REDACTED]",
            },
            SecretPattern {
                re: Regex::new(r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").unwrap(),
                repl: "[REDACTED]",
            },
            SecretPattern {
                re: Regex::new(
                    r"(?m)^(?P<key>[A-Z_][A-Z0-9_]{2,}(?:_KEY|_TOKEN|_SECRET|_PASSWORD|_CREDENTIALS?))\s*=\s*(?P<val>[^\r\n#]+)",
                )
                .unwrap(),
                repl: "${key}=[REDACTED]",
            },
        ]
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RedactSecretsConfig {
    /// User-defined regex patterns. Matched content is replaced with
    /// `[REDACTED]`. Invalid regex is warn-logged and skipped.
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

pub struct RedactSecrets {
    cfg: RedactSecretsConfig,
}

impl RedactSecrets {
    pub fn new(cfg: RedactSecretsConfig) -> Self {
        Self { cfg }
    }

    fn sanitize(&self, content: &str) -> Option<String> {
        let mut buf = content.to_string();
        let mut changed = false;

        for SecretPattern { re, repl } in built_in_patterns() {
            let next = re.replace_all(&buf, *repl).to_string();
            if next != buf {
                changed = true;
                buf = next;
            }
        }

        for pattern_str in &self.cfg.extra_patterns {
            let re = match Regex::new(pattern_str) {
                Ok(re) => re,
                Err(e) => {
                    tracing::warn!(
                        pattern = pattern_str,
                        error = %e,
                        "invalid regex in [hooks.redact_secrets].extra_patterns; skipping"
                    );
                    continue;
                }
            };
            let next = re.replace_all(&buf, "[REDACTED]").to_string();
            if next != buf {
                changed = true;
                buf = next;
            }
        }

        if changed {
            Some(buf)
        } else {
            None
        }
    }
}

#[async_trait]
impl Hook for RedactSecrets {
    fn id(&self) -> &str {
        "redact_secrets"
    }
    fn supported(&self) -> HookPoints {
        HookPoints::TOOL_RESULT
    }
    async fn handle(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookAction, parrot_core::AgentError> {
        if let HookEvent::ToolResult {
            tool_name, result, ..
        } = ev
        {
            if matches!(
                tool_name,
                "read" | "shell_exec" | "bash" | "shell" | "file_read"
            ) {
                if let Some(sanitized) = self.sanitize(&result.content) {
                    return Ok(HookAction::ReplaceResult {
                        content: sanitized,
                        is_error: result.is_error,
                    });
                }
            }
        }
        Ok(HookAction::NoOp)
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

    fn tool_result_ev(content: &str, tool_name: &'static str) -> HookEvent<'static> {
        let out: &'static ToolOutput = Box::leak(Box::new(ToolOutput {
            content: content.to_string(),
            is_error: false,
        }));
        let input: &'static serde_json::Value = Box::leak(Box::new(serde_json::Value::Null));
        HookEvent::ToolResult {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name,
            input,
            result: out,
        }
    }

    #[tokio::test]
    async fn redacts_aws_key() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev("key AKIAIOSFODNN7EXAMPLE lost", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        match res {
            HookAction::ReplaceResult { content, is_error } => {
                assert!(content.contains("[REDACTED]"));
                assert!(!content.contains("AKIAIOSFODNN7EXAMPLE"));
                assert!(!is_error);
            }
            _ => panic!("expected ReplaceResult"),
        }
    }

    #[tokio::test]
    async fn no_change_when_clean() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev("just some normal code", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookAction::NoOp));
    }

    #[tokio::test]
    async fn redacts_jwt() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev(
            "token: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwInQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
            "read",
        );
        let res = h.handle(ev, &ctx()).await.unwrap();
        match res {
            HookAction::ReplaceResult { content, .. } => {
                assert!(content.contains("[REDACTED]"));
                assert!(!content.contains("eyJhbGciOiJIUzI1NiJ9"));
            }
            _ => panic!("expected ReplaceResult for JWT"),
        }
    }

    #[tokio::test]
    async fn redacts_env_value_keeps_key() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev("ANTHROPIC_API_KEY=sk-ant-xxx", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        match res {
            HookAction::ReplaceResult { content, .. } => {
                assert!(content.contains("ANTHROPIC_API_KEY=[REDACTED]"));
                assert!(!content.contains("sk-ant-xxx"));
            }
            _ => panic!("expected ReplaceResult for .env"),
        }
    }

    #[tokio::test]
    async fn env_does_not_redact_non_sensitive_keys() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev("PORT=8080\nDEBUG=true\nNODE_ENV=production", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookAction::NoOp));
    }

    #[tokio::test]
    async fn env_only_matches_line_start() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let ev = tool_result_ev("some text FOO_KEY=barbaz", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookAction::NoOp));
    }

    #[tokio::test]
    async fn extra_pattern_redacts() {
        let cfg = RedactSecretsConfig {
            extra_patterns: vec!["MY-CUSTOM-\\d+".to_string()],
        };
        let h = RedactSecrets::new(cfg);
        let ev = tool_result_ev("value MY-CUSTOM-12345 here", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        match res {
            HookAction::ReplaceResult { content, .. } => {
                assert!(content.contains("[REDACTED]"));
                assert!(!content.contains("MY-CUSTOM-12345"));
            }
            _ => panic!("expected ReplaceResult for extra pattern"),
        }
    }

    #[tokio::test]
    async fn extra_pattern_invalid_regex_warns_skips() {
        let cfg = RedactSecretsConfig {
            extra_patterns: vec!["(".to_string()],
        };
        let h = RedactSecrets::new(cfg);
        let ev = tool_result_ev("key AKIAIOSFODNN7EXAMPLE lost", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        match res {
            HookAction::ReplaceResult { content, .. } => {
                assert!(content.contains("[REDACTED]"));
                assert!(!content.contains("AKIAIOSFODNN7EXAMPLE"));
            }
            _ => panic!("built-in patterns should still work with invalid extra regex"),
        }
    }
}
