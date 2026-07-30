//! ExternalHook: invokes a user-supplied child process per hook event.
//!
//! Lifecycle: spawn child → write JSON envelope to stdin (tokio writer task)
//! → await stdout with timeout → parse last non-empty line as JSON action →
//! fail-open `Err(AgentError::ExternalHook{...})` on any failure.
//!
//! Strict separation of concerns:
//! - `parse_points` and `parse_action` are pure fns, unit-tested below.
//! - `build_envelope` is pure; serialization happens in `handle`.

use parrot_config::ExternalHookConfig;
use parrot_core::error::AgentError;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use std::path::Path;
use std::time::Duration;

/// Discriminator enum for the wire JSON returned by the child process.
/// Tag is `action` (not `kind`, which is HookAction's serde tag) — matches
/// hook author intuition: `{"action": "block", "reason": "..."}`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WireAction {
    Noop,
    Block {
        reason: String,
    },
    InjectMessages {
        messages: Vec<parrot_core::types::ChatMessage>,
    },
    ReplaceResult {
        content: String,
        is_error: bool,
    },
    ReplaceContext {
        messages: Vec<parrot_core::types::ChatMessage>,
    },
}

impl WireAction {
    fn into_hook_action(self) -> HookAction {
        match self {
            WireAction::Noop => HookAction::NoOp,
            WireAction::Block { reason } => HookAction::Block { reason },
            WireAction::InjectMessages { messages } => HookAction::InjectMessages { messages },
            WireAction::ReplaceResult { content, is_error } => {
                HookAction::ReplaceResult { content, is_error }
            }
            WireAction::ReplaceContext { messages } => HookAction::ReplaceContext { messages },
        }
    }
}

#[derive(Debug)]
pub struct ExternalHook {
    id: String,
    command: Vec<String>,
    points: HookPoints,
    timeout_override: Option<Duration>,
    config: serde_json::Value,
}

impl ExternalHook {
    /// Build an `ExternalHook` from config. Unknown `events` strings return
    /// `Err`; the caller (`build_registry`) converts that to a warn + skip.
    pub fn new(cfg: &ExternalHookConfig, _global_timeout: Duration) -> Result<Self, String> {
        let points = parse_points(&cfg.events)?;
        let timeout_override = cfg.timeout_seconds.map(|s| Duration::from_secs(s.max(1)));
        let config = toml_to_json(&cfg.config)?;
        Ok(Self {
            id: cfg.id.clone(),
            command: cfg.command.clone(),
            points,
            timeout_override,
            config,
        })
    }

    fn resolve_timeout(&self, ctx: &HookCtx<'_>) -> Duration {
        self.timeout_override.unwrap_or(ctx.timeout)
    }
}

#[async_trait::async_trait]
impl Hook for ExternalHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported(&self) -> HookPoints {
        self.points
    }

    async fn handle(
        &self,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookAction, AgentError> {
        // Filled in Task 6. Stub exercises helpers so they're not flagged
        // dead-code before Task 6 wires them up. Returns NoOp.
        let _timeout = self.resolve_timeout(ctx);
        let _command = &self.command;
        let env = build_envelope(&event, &self.config, &self.id, ctx.working_dir, 0);
        let _envelope_str = serde_json::to_string(&env).ok();
        let _last = last_non_empty_line(&[]);
        Ok(parse_action(r#"{"action":"noop"}"#).unwrap_or(HookAction::NoOp))
    }
}

/// Convert `toml::Value` to `serde_json::Value` via serde Serialize bridge.
/// `toml::Value` and `serde_json::Value` both implement Serialize; the
/// serde serializer walks the toml tree and emits a parallel json tree.
/// Datetime variants serialize as strings (toml's own Datetime Serialize
/// impl emits ISO strings).
fn toml_to_json(v: &toml::Value) -> Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| format!("toml->json: {e}"))
}

/// Pure: parse HookPoints bitflag from event-kind strings.
pub(crate) fn parse_points(events: &[String]) -> Result<HookPoints, String> {
    let mut p = HookPoints::empty();
    for ev in events {
        let bit = match ev.as_str() {
            "agent_start" => HookPoints::AGENT_START,
            "agent_end" => HookPoints::AGENT_END,
            "turn_start" => HookPoints::TURN_START,
            "tool_call" => HookPoints::TOOL_CALL,
            "tool_execution_start" => HookPoints::TOOL_EXECUTION_START,
            "tool_result" => HookPoints::TOOL_RESULT,
            "context_ready" => HookPoints::CONTEXT_READY,
            other => return Err(format!("unknown event kind: {other}")),
        };
        p |= bit;
    }
    Ok(p)
}

/// Pure: parse the last non-empty line of stdout into a HookAction.
/// Empty / whitespace-only input ⇒ `Ok(HookAction::NoOp)` (silent allow).
pub(crate) fn parse_action(last_line: &str) -> Result<HookAction, String> {
    let trimmed = last_line.trim();
    if trimmed.is_empty() {
        return Ok(HookAction::NoOp);
    }
    let wire: WireAction =
        serde_json::from_str(trimmed).map_err(|e| format!("parse error: {e}"))?;
    Ok(wire.into_hook_action())
}

/// Pure: build the JSON envelope written to the child process's stdin.
/// Returned `Value` is `to_string`'d by `handle`; no IO here.
pub(crate) fn build_envelope(
    event: &HookEvent<'_>,
    config: &serde_json::Value,
    hook_id: &str,
    working_dir: &Path,
    timeout_ms: u64,
) -> serde_json::Value {
    let event_json = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "event": event_json,
        "config": config,
        "hook_id": hook_id,
        "working_dir": working_dir.display().to_string(),
        "timeout_ms": timeout_ms,
    })
}

/// Helper: take last non-empty line from a byte buffer. Pure.
fn last_non_empty_line(stdout: &[u8]) -> &str {
    let s = std::str::from_utf8(stdout).unwrap_or("");
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_core::types::{ChatMessage, ChatRole};

    #[test]
    fn parse_points_known_events() {
        let p = parse_points(&["tool_call".into(), "tool_result".into()]).unwrap();
        assert_eq!(p, HookPoints::TOOL_CALL | HookPoints::TOOL_RESULT);
    }

    #[test]
    fn parse_points_all_seven() {
        let all = [
            "agent_start",
            "agent_end",
            "turn_start",
            "tool_call",
            "tool_execution_start",
            "tool_result",
            "context_ready",
        ];
        let p = parse_points(&all.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        assert_eq!(p, HookPoints::all());
    }

    #[test]
    fn parse_points_unknown_errors() {
        let err = parse_points(&["foo_bar".into()]).unwrap_err();
        assert!(err.contains("unknown event kind: foo_bar"));
    }

    #[test]
    fn parse_points_empty_is_empty_flag() {
        let p = parse_points(&[]).unwrap();
        assert_eq!(p, HookPoints::empty());
    }

    #[test]
    fn parse_action_block() {
        let a = parse_action(r#"{"action":"block","reason":"x"}"#).unwrap();
        assert_eq!(a, HookAction::Block { reason: "x".into() });
    }

    #[test]
    fn parse_action_noop_explicit() {
        let a = parse_action(r#"{"action":"noop"}"#).unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    fn sample_msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }
    }

    #[test]
    fn parse_action_inject_messages() {
        let msg = sample_msg(ChatRole::System, "hi");
        let body = format!(
            r#"{{"action":"inject_messages","messages":[{}]}}"#,
            serde_json::json!(msg)
        );
        let a = parse_action(&body).unwrap();
        match a {
            HookAction::InjectMessages { messages } => assert_eq!(messages.len(), 1),
            other => panic!("expected InjectMessages, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_replace_result() {
        let a =
            parse_action(r#"{"action":"replace_result","content":"y","is_error":true}"#).unwrap();
        assert_eq!(
            a,
            HookAction::ReplaceResult {
                content: "y".into(),
                is_error: true
            }
        );
    }

    #[test]
    fn parse_action_replace_context() {
        let msg = sample_msg(ChatRole::System, "ctx");
        let body = format!(
            r#"{{"action":"replace_context","messages":[{}]}}"#,
            serde_json::json!(msg)
        );
        let a = parse_action(&body).unwrap();
        match a {
            HookAction::ReplaceContext { messages } => assert_eq!(messages.len(), 1),
            other => panic!("expected ReplaceContext, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_empty_is_silent_allow() {
        let a = parse_action("").unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    #[test]
    fn parse_action_whitespace_only_is_noop() {
        let a = parse_action("   \n  \t ").unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    #[test]
    fn parse_action_unknown_action_errors() {
        let err = parse_action(r#"{"action":"record_kv"}"#).unwrap_err();
        assert!(err.contains("parse error") || err.contains("unknown variant"));
    }

    #[test]
    fn parse_action_missing_field_errors() {
        let err = parse_action(r#"{"action":"block"}"#).unwrap_err();
        assert!(err.contains("parse error") || err.contains("missing field"));
    }

    #[test]
    fn parse_action_non_json_errors() {
        let err = parse_action("this is just a debug line").unwrap_err();
        assert!(err.contains("parse error"));
    }

    #[test]
    fn build_envelope_carries_hook_id_and_path() {
        let event = HookEvent::AgentStart {
            session_id: uuid::Uuid::nil(),
            model: "claude-x",
            provider: "anthropic",
        };
        let cfg = serde_json::json!({"sink": "stderr"});
        let env = build_envelope(&event, &cfg, "my-hook", Path::new("/tmp/proj"), 3000);
        assert_eq!(env["hook_id"], "my-hook");
        assert_eq!(env["working_dir"], "/tmp/proj");
        assert_eq!(env["timeout_ms"], 3000);
        assert_eq!(env["config"]["sink"], "stderr");
        assert_eq!(env["event"]["type"], "agent_start");
    }

    #[test]
    fn last_non_empty_line_picks_trailing_json() {
        let stdout = b"debug info\nmore log\n{\"action\":\"noop\"}\n";
        let s = last_non_empty_line(stdout);
        assert_eq!(s, "{\"action\":\"noop\"}");
    }

    #[test]
    fn last_non_empty_line_empty_buffer() {
        assert_eq!(last_non_empty_line(b""), "");
    }

    #[test]
    fn external_hook_new_parses_known_events() {
        let cfg = ExternalHookConfig {
            id: "test".into(),
            command: vec!["echo".into()],
            events: vec!["tool_call".into(), "tool_result".into()],
            timeout_seconds: Some(2),
            config: toml::Value::Table(toml::value::Table::new()),
        };
        let h = ExternalHook::new(&cfg, Duration::from_secs(5)).unwrap();
        assert_eq!(h.id, "test");
        assert_eq!(h.points, HookPoints::TOOL_CALL | HookPoints::TOOL_RESULT);
        assert_eq!(h.timeout_override, Some(Duration::from_secs(2)));
    }

    #[test]
    fn external_hook_new_unknown_event_errs() {
        let cfg = ExternalHookConfig {
            id: "bad".into(),
            command: vec!["echo".into()],
            events: vec!["not_a_real_event".into()],
            timeout_seconds: None,
            config: toml::Value::Table(toml::value::Table::new()),
        };
        let err = ExternalHook::new(&cfg, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("unknown event kind: not_a_real_event"));
    }

    #[test]
    fn external_hook_new_timeout_zero_clamped() {
        let cfg = ExternalHookConfig {
            id: "t".into(),
            command: vec!["echo".into()],
            events: vec![],
            timeout_seconds: Some(0),
            config: toml::Value::Table(toml::value::Table::new()),
        };
        let h = ExternalHook::new(&cfg, Duration::from_secs(5)).unwrap();
        assert_eq!(h.timeout_override, Some(Duration::from_secs(1)));
    }
}
