# Redact-Secrets Hook Expansion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expand the `redact_secrets` hook with JWT + `.env KEY=value` built-in patterns, add an `extra_patterns: Vec<String>` user-config field, and implement invalid-regex warn+skip semantics.

**Architecture:** `RedactSecrets` becomes a config-carrying struct `RedactSecrets { cfg: RedactSecretsConfig }` (matching the pattern established for `ShellDenylist` in Plan #1). Built-in patterns upgrade from `Vec<Regex>` to `Vec<SecretPattern>` (regex + replacement template). The `build_registry` function in `parrot-hooks/src/lib.rs` reads `[hooks.redact_secrets]` from `HooksConfig.configs` and deserializes it into `RedactSecretsConfig`.

**Tech Stack:** Rust 2021, regex crate (named capture groups + `${name}` replacement templates), serde, toml, tracing

## Global Constraints

- Same as Plan #1: `thiserror` for crate-boundary errors, zero IO in `parrot-core`, tagged WS enums.
- `redact_secrets` stays opt-in (not in default `enabled`).
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
- Branch: `feat/lifecycle-hooks`
- **Depends on:** Plan #1 completed (redact_secrets already moved to `crates/parrot-hooks/src/redact_secrets.rs`, `build_registry` already reads per-hook configs from `HooksConfig.configs`).

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/parrot-hooks/src/redact_secrets.rs` | Rewrite | `RedactSecretsConfig`, `SecretPattern`, built-in patterns, `sanitize()` method, `handle()` |
| `crates/parrot-hooks/src/lib.rs` | Modify | `build_registry` arm for `redact_secrets` deserializes config |
| `crates/parrot-config/src/config.rs` | Modify (test only) | Add config parse test for `extra_patterns` |

---

### Task 1: Add `RedactSecretsConfig` + Convert to Config-Carrying Struct

**Files:**
- Modify: `crates/parrot-hooks/src/redact_secrets.rs` (full rewrite)
- Modify: `crates/parrot-hooks/src/lib.rs` (build_registry arm)

**Interfaces:**
- Produces: `RedactSecretsConfig { extra_patterns: Vec<String> }` with `Default`
- Produces: `RedactSecrets::new(cfg: RedactSecretsConfig) -> Self`
- Produces: `SecretPattern { re: Regex, repl: &'static str }` (private)

- [ ] **Step 1: Write the failing tests**

Replace the entire `crates/parrot-hooks/src/redact_secrets.rs` with:

```rust
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
                ).unwrap(),
                repl: "${key}=[REDACTED]",
            },
        ]
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedactSecretsConfig {
    /// User-defined regex patterns. Matched content is replaced with
    /// `[REDACTED]`. Invalid regex is warn-logged and skipped.
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

impl Default for RedactSecretsConfig {
    fn default() -> Self {
        Self {
            extra_patterns: Vec::new(),
        }
    }
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

        if changed { Some(buf) } else { None }
    }
}

#[async_trait]
impl Hook for RedactSecrets {
    fn id(&self) -> &'static str {
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
            tool_name,
            result,
            ..
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

    fn tool_result_ev(content: &str, tool_name: &'static str) -> (HookEvent<'static>, ToolOutput) {
        let out = ToolOutput {
            content: content.to_string(),
            is_error: false,
        };
        let ev = HookEvent::ToolResult {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name,
            input: &serde_json::Value::Null,
            result: &out,
        };
        (ev, out)
    }

    // --- Existing behavior preserved ---

    #[tokio::test]
    async fn redacts_aws_key() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let (ev, _out) = tool_result_ev("key AKIAIOSFODNN7EXAMPLE lost", "read");
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
        let (ev, _out) = tool_result_ev("just some normal code", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookAction::NoOp));
    }

    // --- New: JWT ---

    #[tokio::test]
    async fn redacts_jwt() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let (ev, _out) = tool_result_ev(
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

    // --- New: .env KEY=value ---

    #[tokio::test]
    async fn redacts_env_value_keeps_key() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let (ev, _out) = tool_result_ev("ANTHROPIC_API_KEY=sk-ant-xxx", "read");
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
        let (ev, _out) = tool_result_ev("PORT=8080\nDEBUG=true\nNODE_ENV=production", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(res, HookAction::NoOp));
    }

    #[tokio::test]
    async fn env_only_matches_line_start() {
        let h = RedactSecrets::new(RedactSecretsConfig::default());
        let (ev, _out) = tool_result_ev("some text FOO_KEY=barbaz", "read");
        let res = h.handle(ev, &ctx()).await.unwrap();
        // FOO_KEY=barbaz is not at line start, so .env pattern won't match.
        // But "barbaz" itself doesn't match any other pattern either.
        assert!(matches!(res, HookAction::NoOp));
    }

    // --- New: extra_patterns ---

    #[tokio::test]
    async fn extra_pattern_redacts() {
        let cfg = RedactSecretsConfig {
            extra_patterns: vec!["MY-CUSTOM-\\d+".to_string()],
        };
        let h = RedactSecrets::new(cfg);
        let (ev, _out) = tool_result_ev("value MY-CUSTOM-12345 here", "read");
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
            extra_patterns: vec!["(".to_string()], // invalid regex
        };
        let h = RedactSecrets::new(cfg);
        // Built-in patterns should still work even with invalid extra regex
        let (ev, _out) = tool_result_ev("key AKIAIOSFODNN7EXAMPLE lost", "read");
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
```

- [ ] **Step 2: Update `build_registry` for redact_secrets config**

In `crates/parrot-hooks/src/lib.rs`, update the `"redact_secrets"` arm to deserialize config:

```rust
            "redact_secrets" => {
                let hook_cfg = cfg
                    .configs
                    .get("redact_secrets")
                    .and_then(|v| v.clone().try_deserialize::<redact_secrets::RedactSecretsConfig>())
                    .unwrap_or_default();
                reg.register(Arc::new(redact_secrets::RedactSecrets::new(hook_cfg)))
            }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p parrot-hooks`
Expected: All PASS

- [ ] **Step 4: Verify workspace**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-hooks/src/redact_secrets.rs crates/parrot-hooks/src/lib.rs
git commit -m "feat: redact_secrets expansion (JWT + .env + extra_patterns)"
```

---

### Task 2: Config Parse Test for `extra_patterns`

**Files:**
- Modify: `crates/parrot-config/src/config.rs` (add test)

- [ ] **Step 1: Write the test**

Add to `crates/parrot-config/src/config.rs` test module:

```rust
    #[test]
    fn hooks_parse_redact_secrets_extra_patterns() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6

[hooks]
enabled = ["redact_secrets"]

[hooks.redact_secrets]
extra_patterns = ["CUSTOM-\\d+", "MY-TOKEN-[a-z]+"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.hooks.enabled, vec!["redact_secrets"]);
        assert!(c.hooks.configs.contains_key("redact_secrets"));
        let rs_cfg = c.hooks.configs.get("redact_secrets").unwrap();
        let extra = rs_cfg.get("extra_patterns").and_then(|v| v.as_array()).unwrap();
        assert_eq!(extra.len(), 2);
    }
```

- [ ] **Step 2: Run test**

Run: `cargo test -p parrot-config -- hooks_parse_redact_secrets_extra_patterns`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/parrot-config/src/config.rs
git commit -m "test: config parse for redact_secrets extra_patterns"
```

---

### Task 3: Final Verification

- [ ] **Step 1: Full workspace check**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: All PASS

- [ ] **Step 2: Commit if any fmt/clippy fixes needed**

```bash
git add -A
git commit -m "fix: fmt/clippy after redact_secrets expansion"
```
(Only if there are changes; skip if clean.)
