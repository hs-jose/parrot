//! Phase 1.5 integration tests: ListSessions, ResumeSession, ToolList, and
//! the ConfirmToolCall flow (approve / reject / timeout).
//!
//! Each test spins up a daemon with a mock provider + echo tool (and for the
//! confirm tests, a `require_confirmation` pattern matching `echo` plus a
//! short timeout). A real WS client drives the protocol end-to-end. No
//! network, no API key.

use async_trait::async_trait;
use parrot_config::AppConfig;
use parrot_core::error::{AgentError, ProviderError};
use parrot_core::event_log::StreamEvent;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderRegistry};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::{ChatMessage, GenerateConfig, ModelInfo};
use parrot_protocol::types::{ConfirmDecision, SessionConfig, StopReason};
use parrot_protocol::{ClientMessage, ServerMessage};
use parrot_transport::TransportClient;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Mock provider + echo tool
// ---------------------------------------------------------------------------

struct MockProvider {
    call_count: AtomicU32,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo {
            id: "mock-model".to_string(),
            name: "Mock Model".to_string(),
            provider: "mock".to_string(),
            context_window: 200_000,
            max_output_tokens: 8192,
        }])
    }

    async fn chat_stream(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<StreamEvent>(16);
        tokio::spawn(async move {
            match n {
                0 => {
                    tx.send(StreamEvent::ToolCallStart {
                        id: "tc_1".to_string(),
                        name: "echo".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(StreamEvent::ToolCallDelta {
                        id: "tc_1".to_string(),
                        args_delta: r#"{"message":"hello"}"#.to_string(),
                    })
                    .await
                    .ok();
                    tx.send(StreamEvent::ToolCallEnd {
                        id: "tc_1".to_string(),
                        arguments: json!({"message": "hello"}),
                    })
                    .await
                    .ok();
                    tx.send(StreamEvent::Finish {
                        stop_reason: StopReason::ToolUse,
                        usage: parrot_protocol::types::Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                        },
                    })
                    .await
                    .ok();
                }
                _ => {
                    tx.send(StreamEvent::TextDelta {
                        delta: "done".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(StreamEvent::Finish {
                        stop_reason: StopReason::EndTurn,
                        usage: parrot_protocol::types::Usage {
                            input_tokens: 20,
                            output_tokens: 10,
                        },
                    })
                    .await
                    .ok();
                }
            }
        });
        Ok(ChatStream { inner: rx })
    }

    async fn chat(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        unimplemented!()
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes the message argument back."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "message": { "type": "string" } }, "required": ["message"] })
    }
    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let msg = arguments
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(ToolOutput {
            content: format!("echo: {msg}"),
            is_error: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn test_config(port: u16, data_dir: &std::path::Path, token_path: &std::path::Path) -> AppConfig {
    let mut config = AppConfig::default_config();
    config.daemon.host = "127.0.0.1".to_string();
    config.daemon.port = port;
    config.daemon.auth_token_file = token_path.to_string_lossy().to_string();
    config.session.data_dir = data_dir.to_string_lossy().to_string();
    config.providers.push(parrot_config::ProviderConfig {
        id: "mock".to_string(),
        api_key: String::new(),
        default_model: "mock-model".to_string(),
        base_url: None,
        models: vec!["mock-model".to_string()],
    });
    config
}

async fn expect_server_message<F, T>(
    rx: &mut mpsc::Receiver<ServerMessage>,
    predicate: F,
    label: &str,
) -> T
where
    F: Fn(&ServerMessage) -> Option<T>,
    T: std::fmt::Debug,
{
    let result = timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(msg) => {
                    if let Some(t) = predicate(&msg) {
                        return Ok(t);
                    }
                }
                None => return Err("channel closed".to_string()),
            }
        }
    })
    .await;
    match result {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => panic!("expect_server_message({label}): {e}"),
        Err(_) => panic!("expect_server_message({label}): timed out after 10s"),
    }
}

/// Spin up a daemon with the mock provider + echo tool. `confirm_patterns`
/// controls `require_confirmation` (empty = no confirmations); `confirm_timeout`
/// controls how long the engine waits for a `ConfirmToolCall` response.
async fn spawn_daemon(
    confirm_patterns: Vec<String>,
    confirm_timeout: Duration,
) -> (
    mpsc::Receiver<ServerMessage>,
    mpsc::Sender<ClientMessage>,
    tokio::task::JoinHandle<()>,
) {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = parrot::auth::Auth::new(&token_path)
        .await
        .expect("init auth");
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    let auth = Arc::new(auth);

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;
    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(EchoTool) as Arc<dyn Tool>)
        .await;

    let mut config = test_config(port, &data_dir, &token_path);
    config.tools.sandbox.require_confirmation = confirm_patterns;

    let daemon_handle = tokio::spawn(async move {
        parrot::server::run_with_confirm_timeout(
            config,
            auth,
            provider_registry,
            tool_registry,
            confirm_timeout,
        )
        .await
        .expect("daemon run_with_confirm_timeout");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("client connect");
    expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::HelloAck { .. } = m {
                Some(())
            } else {
                None
            }
        },
        "HelloAck",
    )
    .await;

    std::mem::forget(tmp);
    (conn.receiver, conn.sender, daemon_handle)
}

async fn create_session(
    tx: &mpsc::Sender<ClientMessage>,
    rx: &mut mpsc::Receiver<ServerMessage>,
) -> uuid::Uuid {
    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: None,
            provider: None,
            system_prompt: None,
        }),
    })
    .await
    .expect("send CreateSession");
    expect_server_message(
        rx,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await
}

/// Drain the full ReAct flow for one chat turn until the final EndTurn
/// Finished. The engine may emit an intermediate `Finish{ToolUse}` before
/// tool execution; this skips those and returns only on EndTurn. All other
/// messages (ToolCall*, ToolResult, TextDelta, intermediate ToolUse
/// finishes) are consumed and discarded.
async fn drain_until_end_turn(rx: &mut mpsc::Receiver<ServerMessage>, session_id: uuid::Uuid) {
    loop {
        let stop = expect_server_message(
            rx,
            |m| {
                if let ServerMessage::Finished {
                    session_id: sid,
                    stop_reason,
                    ..
                } = m
                {
                    if *sid == session_id {
                        Some(stop_reason.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
            "Finished(EndTurn)",
        )
        .await;
        if stop == StopReason::EndTurn {
            return;
        }
        // Intermediate ToolUse/Aborted/MaxTokens finishes are consumed; keep
        // waiting for EndTurn. (Aborted would break the loop in practice
        // since the engine returns, but for our mock that won't happen.)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_tool_list_returns_dedicated_message() {
    let (mut rx, tx, daemon) = spawn_daemon(Vec::new(), Duration::from_secs(60)).await;
    let session_id = create_session(&tx, &mut rx).await;
    tx.send(ClientMessage::ListTools { session_id })
        .await
        .expect("send ListTools");

    let tools = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolList {
                session_id: sid,
                tools,
            } = m
            {
                if *sid == session_id {
                    Some(tools.clone())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolList",
    )
    .await;

    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected echo tool in list: {:?}",
        tools
    );
    daemon.abort();
}

#[tokio::test]
async fn e2e_list_sessions_sees_created_session() {
    let (mut rx, tx, daemon) = spawn_daemon(Vec::new(), Duration::from_secs(60)).await;
    let session_id = create_session(&tx, &mut rx).await;

    tx.send(ClientMessage::ListSessions)
        .await
        .expect("send ListSessions");
    let sessions = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionList { sessions } = m {
                Some(sessions.clone())
            } else {
                None
            }
        },
        "SessionList",
    )
    .await;

    assert!(
        sessions.iter().any(|s| s.id == session_id),
        "expected created session {} in list: {:?}",
        session_id,
        sessions
    );
    daemon.abort();
}

#[tokio::test]
async fn e2e_resume_session_replays_event_log() {
    let (mut rx, tx, daemon) = spawn_daemon(Vec::new(), Duration::from_secs(60)).await;
    let session_id = create_session(&tx, &mut rx).await;

    // Run a full chat turn so events.log has content. The engine emits an
    // intermediate Finish{ToolUse} before tool execution, then loops and
    // emits Finish{EndTurn} after the second provider call. Drain until
    // EndTurn to flush the whole turn.
    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");
    drain_until_end_turn(&mut rx, session_id).await;

    // Request history to verify the event log persisted.
    tx.send(ClientMessage::GetHistory { session_id })
        .await
        .expect("send GetHistory");
    let entries = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::History {
                session_id: sid,
                entries,
            } = m
            {
                if *sid == session_id {
                    Some(entries.clone())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "History",
    )
    .await;
    assert!(
        !entries.is_empty(),
        "history should not be empty after a chat turn"
    );

    // Now resume the session. The daemon should acknowledge and the session
    // should accept a new Chat (proving the engine task was spawned with the
    // replayed context).
    tx.send(ClientMessage::ResumeSession { session_id })
        .await
        .expect("send ResumeSession");
    expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionResumed { session_id: sid } = m {
                if *sid == session_id {
                    Some(())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "SessionResumed",
    )
    .await;

    // Send a follow-up chat; the resumed engine should respond. The mock
    // provider's call_count is now >0 so it returns "done"+EndTurn directly
    // (no tool_use), so a single drain flushes the turn.
    tx.send(ClientMessage::Chat {
        session_id,
        message: "thanks".to_string(),
    })
    .await
    .expect("send follow-up Chat");
    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}

#[tokio::test]
async fn e2e_confirm_tool_call_approve_executes_tool() {
    // require_confirmation matches "echo"; timeout is generous so the test
    // must respond before it fires.
    let (mut rx, tx, daemon) =
        spawn_daemon(vec!["echo".to_string()], Duration::from_secs(10)).await;
    let session_id = create_session(&tx, &mut rx).await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    // Expect a confirmation request for the echo tool call.
    let (tool_id, tool_name) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolCallConfirmationRequired {
                session_id: sid,
                tool_id,
                tool_name,
                ..
            } = m
            {
                if *sid == session_id {
                    Some((tool_id.clone(), tool_name.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolCallConfirmationRequired",
    )
    .await;
    assert_eq!(tool_name, "echo");

    // Approve it.
    tx.send(ClientMessage::ConfirmToolCall {
        session_id,
        tool_id,
        decision: ConfirmDecision::Approve,
    })
    .await
    .expect("send ConfirmToolCall(Approve)");

    // The engine should now execute the tool and emit a ToolResult, then
    // loop and finish with EndTurn (after the second provider call).
    let (result_content, is_error) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolResult {
                session_id: sid,
                result,
                ..
            } = m
            {
                if *sid == session_id {
                    Some((result.content.clone(), result.is_error))
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolResult",
    )
    .await;
    assert!(!is_error, "approved tool should not error");
    assert_eq!(result_content, "echo: hello");

    // Drain the remaining turn until EndTurn (the Finish{ToolUse} was
    // already consumed while waiting for the confirmation request above).
    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}

#[tokio::test]
async fn e2e_confirm_tool_call_reject_skips_tool() {
    let (mut rx, tx, daemon) =
        spawn_daemon(vec!["echo".to_string()], Duration::from_secs(10)).await;
    let session_id = create_session(&tx, &mut rx).await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    let (tool_id, _) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolCallConfirmationRequired {
                session_id: sid,
                tool_id,
                ..
            } = m
            {
                if *sid == session_id {
                    Some((tool_id.clone(), ()))
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolCallConfirmationRequired",
    )
    .await;

    tx.send(ClientMessage::ConfirmToolCall {
        session_id,
        tool_id,
        decision: ConfirmDecision::Reject,
    })
    .await
    .expect("send ConfirmToolCall(Reject)");

    // The engine should emit a ToolResult marked as an error with
    // "user rejected" content, NOT "echo: hello".
    let (result_content, is_error) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolResult {
                session_id: sid,
                result,
                ..
            } = m
            {
                if *sid == session_id {
                    Some((result.content.clone(), result.is_error))
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolResult(rejected)",
    )
    .await;
    assert!(is_error, "rejected tool should be an error");
    assert!(
        result_content.contains("rejected"),
        "expected 'rejected' in content, got: {result_content}"
    );

    // Drain remaining turn until EndTurn.
    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}

#[tokio::test]
async fn e2e_confirm_tool_call_timeout_skips_tool() {
    // Use a short timeout so the test doesn't wait 60s. The test does NOT
    // send a ConfirmToolCall response — the engine should time out and
    // treat it as Reject.
    let (mut rx, tx, daemon) =
        spawn_daemon(vec!["echo".to_string()], Duration::from_millis(300)).await;
    let session_id = create_session(&tx, &mut rx).await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    // Expect the confirmation request, then do nothing and wait for the
    // engine's timeout to fire — it should emit a ToolResult with
    // "confirmation timeout" content.
    let _ = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolCallConfirmationRequired {
                session_id: sid, ..
            } = m
            {
                if *sid == session_id {
                    Some(())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolCallConfirmationRequired",
    )
    .await;

    let (result_content, is_error) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolResult {
                session_id: sid,
                result,
                ..
            } = m
            {
                if *sid == session_id {
                    Some((result.content.clone(), result.is_error))
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolResult(timeout)",
    )
    .await;
    assert!(is_error, "timed-out tool should be an error");
    assert!(
        result_content.contains("timeout"),
        "expected 'timeout' in content, got: {result_content}"
    );

    // Drain remaining turn until EndTurn.
    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}
