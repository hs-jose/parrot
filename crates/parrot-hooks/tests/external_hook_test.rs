// Integration tests for ExternalHook::handle invoking real child processes
// (the mock_hook_* bins declared in Cargo.toml). Validates spawn / stdin /
// stdout / timeout / stderr / exit-code paths against the fail-open matrix
// in spec §6.1.
//
// Mock bins are built as separate [[bin]] targets in the parrot-hooks
// package; CARGO_BIN_EXE_<name> is the canonical cargo path for locating
// them. They're declared with test=false so cargo doesn't try to run them
// as test binaries.

use parrot_config::ExternalHookConfig;
use parrot_core::error::AgentError;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use parrot_hooks::external::ExternalHook;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

fn make_hook_with_timeout(command: Vec<String>, timeout: Option<u64>) -> ExternalHook {
    let cfg = ExternalHookConfig {
        id: "test-hook".into(),
        command,
        events: vec!["tool_call".into()],
        timeout_seconds: timeout,
        config: toml::Value::Table(toml::value::Table::new()),
    };
    ExternalHook::new(&cfg, Duration::from_secs(5)).unwrap()
}

fn ctx_with_timeout(timeout: Duration) -> HookCtx<'static> {
    // leak a small Path so the HookCtx can borrow 'static — acceptable in tests.
    let working_dir: &'static Path = Box::leak(Path::new(".").to_path_buf().into_boxed_path());
    HookCtx {
        session_id: Uuid::nil(),
        working_dir,
        timeout,
    }
}

fn tool_call_event_with<'a>(args: &'a serde_json::Value) -> HookEvent<'a> {
    HookEvent::ToolCall {
        session_id: Uuid::nil(),
        turn_id: Uuid::nil(),
        parent_message_id: Uuid::nil(),
        tool_call_id: "tc_test",
        tool_name: "shell_exec",
        arguments: args,
    }
}

#[tokio::test]
async fn mock_block_returns_block() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_block").to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({"command":"rm -rf /"});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(
        out,
        HookAction::Block {
            reason: "test-block".into()
        }
    );
}

#[tokio::test]
async fn mock_noop_returns_noop() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_noop").to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({"command":"ls"});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(out, HookAction::NoOp);
}

#[tokio::test]
async fn mock_silent_returns_noop_silent_allow() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_silent").to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({"command":"ls"});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(out, HookAction::NoOp);
}

#[tokio::test]
async fn mock_exit1_failopen_err() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_exit1").to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        AgentError::ExternalHook { detail, .. } => {
            assert!(detail.starts_with("exit="));
        }
        other => panic!("expected ExternalHook err, got {other:?}"),
    }
}

#[tokio::test]
async fn mock_timeout_failopen_err() {
    // Use the ctx timeout (no override) so the test controls the budget.
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_sleep").to_string()];
    let hook = make_hook_with_timeout(cmd, None);
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_millis(100));
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        AgentError::ExternalHook { detail, .. } => {
            assert_eq!(detail, "timeout");
        }
        other => panic!("expected timeout err, got {other:?}"),
    }
}

#[tokio::test]
async fn mock_unknown_action_failopen_err() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_unknown").to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        AgentError::ExternalHook { detail, .. } => {
            assert!(
                detail.contains("unknown variant") || detail.contains("parse error"),
                "expected unknown variant / parse error, got: {detail}"
            );
        }
        other => panic!("expected ExternalHook err, got {other:?}"),
    }
}

#[tokio::test]
async fn external_hook_id_supported() {
    let hook = make_hook_with_timeout(vec![env!("CARGO_BIN_EXE_mock_hook_noop").to_string()], None);
    assert_eq!(hook.id(), "test-hook");
    assert_eq!(hook.supported(), HookPoints::TOOL_CALL);
}

#[tokio::test]
async fn spawn_failure_for_nonexistent_command_err() {
    let cmd = vec!["definitely_not_a_real_binary_xyz".to_string()];
    let hook = make_hook_with_timeout(cmd, Some(2));
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let c = ctx_with_timeout(Duration::from_secs(5));
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        AgentError::ExternalHook { detail, .. } => {
            assert!(detail.starts_with("spawn:"), "got: {detail}");
        }
        other => panic!("expected ExternalHook err, got {other:?}"),
    }
}
