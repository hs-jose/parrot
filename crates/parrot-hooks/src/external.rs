//! ExternalHook：每个 hook 事件调用一次用户提供的子进程。
//!
//! 生命周期：spawn 子进程 → 把 JSON 信封写进 stdin（tokio writer 任务）
//! → 带 timeout 等 stdout → 把最后一个非空行解析成 JSON action →
//! 任何失败都 fail-open 返回 `Err(AgentError::ExternalHook{...})`。
//!
//! 关注点严格分离：
//! - `parse_points` 与 `parse_action` 是纯函数，下方有单元测试。
//! - `build_envelope` 是纯函数；序列化发生在 `handle` 里。

use parrot_config::ExternalHookConfig;
use parrot_core::error::AgentError;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use std::path::Path;
use std::time::Duration;

/// 子进程返回的线上 JSON 的判别枚举。tag 是 `action`（不是 `kind`，
/// 后者是 HookAction 的 serde tag）——贴合 hook 作者直觉：
/// `{"action": "block", "reason": "..."}`。
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
    /// 从配置构建 `ExternalHook`。未知的 `events` 字符串返回 `Err`，
    /// 调用方（`build_registry`）把它转成 warn + 跳过。
    pub fn new(cfg: &ExternalHookConfig) -> Result<Self, String> {
        let points = parse_points(&cfg.events)?;
        let timeout_override = cfg.timeout_seconds.map(|s| Duration::from_secs(s.max(1)));
        let config = serde_json::to_value(&cfg.config).map_err(|e| format!("toml->json: {e}"))?;
        Ok(Self {
            id: cfg.id.clone(),
            command: cfg.command.clone(),
            points,
            timeout_override,
            config,
        })
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
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        let timeout = self.timeout_override.unwrap_or(ctx.timeout);
        let timeout_ms = timeout.as_millis() as u64;
        let envelope = build_envelope(&event, &self.config, &self.id, ctx.working_dir, timeout_ms);
        let envelope_str =
            serde_json::to_string(&envelope).map_err(|e| AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: format!("envelope serialize: {e}"),
            })?;

        if self.command.is_empty() {
            return Err(AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: "empty command".into(),
            });
        }

        // kill_on_drop(true)：超时 future 被 drop 时确保子进程被
        // SIGKILL 并收割（超时分支拿不到 child，因为 wait_with_output
        // 按所有权消费它）。
        let mut child = Command::new(&self.command[0])
            .args(&self.command[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: format!("spawn: {e}"),
            })?;

        // writer 任务：把 JSON 信封写进 stdin 然后关闭。尽力而为
        // （writer 任务的错误被忽略）。drop 时关闭 stdin，若有子进程
        // 在读 stdin 则解除其阻塞。
        let stdin = child.stdin.take().expect("piped");
        let envelope_bytes = format!("{}\n", envelope_str).into_bytes();
        let writer_handle: tokio::task::JoinHandle<std::io::Result<()>> =
            tokio::spawn(async move {
                let mut stdin = stdin;
                stdin.write_all(&envelope_bytes).await
            });

        // 带 timeout 等输出。wait_with_output 会消费 `child`；
        // future 被取消时由 kill_on_drop(true) 负责清理。
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                let _ = writer_handle.await;
                self.log_stderr(Vec::new());
                return Err(AgentError::ExternalHook {
                    hook_id: self.id.clone(),
                    detail: format!("wait: {e}"),
                });
            }
            Err(_) => {
                // 内层 future（持有 child）在此被 drop；child 随 drop 被杀。
                let _ = writer_handle.await;
                self.log_stderr(Vec::new());
                return Err(AgentError::ExternalHook {
                    hook_id: self.id.clone(),
                    detail: "timeout".into(),
                });
            }
        };

        let _ = writer_handle.await;
        self.log_stderr(output.stderr.clone());

        if !output.status.success() {
            return Err(AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: format!("exit={}", output.status.code().unwrap_or(-1)),
            });
        }

        let last_line = last_non_empty_line(&output.stdout);
        parse_action(last_line).map_err(|detail| AgentError::ExternalHook {
            hook_id: self.id.clone(),
            detail,
        })
    }
}

impl ExternalHook {
    /// 把子进程 stderr 截到 4KB 后经 tracing::warn 转发。空 stderr 静默
    /// 丢弃。hook 成功输出不在这里记日志——stdout 已被 parse_action 消费。
    fn log_stderr(&self, stderr: Vec<u8>) {
        let stderr = String::from_utf8_lossy(&stderr);
        if stderr.is_empty() {
            return;
        }
        let trimmed = if stderr.len() > 4096 {
            // 找 <= 4096 的最后一个字符边界，避免多字节 UTF-8
            // （如 hook stderr 里的中文/emoji）导致 panic。
            let mut end = 4096;
            while end > 0 && !stderr.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}...(truncated {} bytes total)",
                &stderr[..end],
                stderr.len()
            )
        } else {
            stderr.to_string()
        };
        tracing::warn!(
            target: "parrotd::ext_hook",
            hook_id = %self.id,
            stderr = %trimmed,
            "external hook produced stderr"
        );
    }
}

/// 纯函数：把 event-kind 字符串解析成 HookPoints 位标志。
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

/// 纯函数：把 stdout 最后一个非空行解析成 HookAction。
/// 空/纯空白输入 ⇒ `Ok(HookAction::NoOp)`（静默放行）。
pub(crate) fn parse_action(last_line: &str) -> Result<HookAction, String> {
    let trimmed = last_line.trim();
    if trimmed.is_empty() {
        return Ok(HookAction::NoOp);
    }
    let wire: WireAction =
        serde_json::from_str(trimmed).map_err(|e| format!("parse error: {e}"))?;
    Ok(wire.into_hook_action())
}

/// 纯函数：构造写到子进程 stdin 的 JSON 信封。返回的 `Value` 由
/// `handle` 做 `to_string`；此处无 IO。
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

/// 辅助：从字节缓冲取最后一个非空行。纯函数。
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
        ChatMessage::new(role, content)
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
        let h = ExternalHook::new(&cfg).unwrap();
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
        let err = ExternalHook::new(&cfg).unwrap_err();
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
        let h = ExternalHook::new(&cfg).unwrap();
        assert_eq!(h.timeout_override, Some(Duration::from_secs(1)));
    }
}
