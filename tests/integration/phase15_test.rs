//! Phase 1.5 integration tests: ListSessions, ResumeSession, ToolList, and
//! the ConfirmToolCall flow (approve / reject / timeout).
//!
//! All streaming events arrive as `ServerMessage::AgentEvent { event }`
//! envelopes (unified event model).

use async_trait::async_trait;
use parrot_config::AppConfig;
use parrot_core::error::{AgentError, ProviderError};
use parrot_core::provider::{
    ChatStream, LlmProvider, ProviderRegistry, ProviderStopReason, ProviderStreamEvent,
};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::{ChatMessage, GenerateConfig, ModelInfo};
use parrot_protocol::agent_event::{AgentEvent, TurnStopReason};
use parrot_protocol::types::{ConfirmDecision, SessionConfig};
use parrot_protocol::{ClientMessage, ServerMessage};
use parrot_transport::TransportClient;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

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
        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);
        tokio::spawn(async move {
            match n {
                0 => {
                    tx.send(ProviderStreamEvent::ToolCallStart {
                        id: "tc_1".to_string(),
                        name: "echo".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallDelta {
                        id: "tc_1".to_string(),
                        args_delta: r#"{"message":"hello"}"#.to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallEnd {
                        id: "tc_1".to_string(),
                        arguments: json!({"message": "hello"}),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::Finish {
                        stop_reason: ProviderStopReason::ToolUse,
                        usage: parrot_protocol::types::Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                        },
                    })
                    .await
                    .ok();
                }
                _ => {
                    tx.send(ProviderStreamEvent::TextDelta {
                        delta: "done".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::Finish {
                        stop_reason: ProviderStopReason::EndTurn,
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

async fn expect_agent_event<F, T>(
    rx: &mut mpsc::Receiver<ServerMessage>,
    predicate: F,
    label: &str,
) -> T
where
    F: Fn(&AgentEvent) -> Option<T>,
    T: std::fmt::Debug,
{
    let result = timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(ServerMessage::AgentEvent { event }) => {
                    if let Some(t) = predicate(&event) {
                        return Ok(t);
                    }
                }
                Some(other) => {
                    eprintln!("expect_agent_event({label}): skipping: {:?}", other);
                }
                None => return Err("channel closed".to_string()),
            }
        }
    })
    .await;
    match result {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => panic!("expect_agent_event({label}): {e}"),
        Err(_) => panic!("expect_agent_event({label}): timed out after 10s"),
    }
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

    let auth = parrot_daemon::auth::Auth::new(&token_path)
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
        parrot_daemon::server::run_with_confirm_timeout(
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

/// Drain the full ReAct flow for one chat turn until TurnEnd(EndTurn).
/// All other AgentEvent envelopes are consumed and discarded.
async fn drain_until_end_turn(rx: &mut mpsc::Receiver<ServerMessage>, session_id: uuid::Uuid) {
    loop {
        let stop = expect_agent_event(
            rx,
            |ev| {
                if let AgentEvent::TurnEnd {
                    session_id: sid,
                    stop_reason,
                    ..
                } = ev
                {
                    if *sid == session_id {
                        return Some(stop_reason.clone());
                    }
                }
                None
            },
            "TurnEnd",
        )
        .await;
        if stop == TurnStopReason::EndTurn {
            return;
        }
    }
}

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
    let events = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::History {
                session_id: sid,
                events,
            } = m
            {
                if *sid == session_id {
                    Some(events.clone())
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
        !events.is_empty(),
        "history should not be empty after a chat turn"
    );

    // Now resume the session.
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

    // Send a follow-up chat; the resumed engine should respond.
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
    let (mut rx, tx, daemon) =
        spawn_daemon(vec!["echo".to_string()], Duration::from_secs(10)).await;
    let session_id = create_session(&tx, &mut rx).await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    // Expect a ToolConfirmRequired for the echo tool call.
    let (tool_call_id, tool_name) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolConfirmRequired {
                session_id: sid,
                tool_call_id,
                tool_name,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((tool_call_id.clone(), tool_name.clone()));
                }
            }
            None
        },
        "ToolConfirmRequired",
    )
    .await;
    assert_eq!(tool_name, "echo");

    // Approve it.
    tx.send(ClientMessage::ConfirmToolCall {
        session_id,
        tool_id: tool_call_id,
        decision: ConfirmDecision::Approve,
    })
    .await
    .expect("send ConfirmToolCall(Approve)");

    // The engine should now execute the tool and emit a ToolEnd, then
    // loop and finish with EndTurn.
    let (result_content, is_error) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((result.content.clone(), result.is_error));
                }
            }
            None
        },
        "ToolEnd",
    )
    .await;
    assert!(!is_error, "approved tool should not error");
    assert_eq!(result_content, "echo: hello");

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

    let (tool_call_id, _) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolConfirmRequired {
                session_id: sid,
                tool_call_id,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((tool_call_id.clone(), ()));
                }
            }
            None
        },
        "ToolConfirmRequired",
    )
    .await;

    tx.send(ClientMessage::ConfirmToolCall {
        session_id,
        tool_id: tool_call_id,
        decision: ConfirmDecision::Reject,
    })
    .await
    .expect("send ConfirmToolCall(Reject)");

    // The engine should emit a ToolEnd marked as an error with
    // "user rejected" content.
    let (result_content, is_error) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((result.content.clone(), result.is_error));
                }
            }
            None
        },
        "ToolEnd(rejected)",
    )
    .await;
    assert!(is_error, "rejected tool should be an error");
    assert!(
        result_content.contains("rejected"),
        "expected 'rejected' in content, got: {result_content}"
    );

    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}

#[tokio::test]
async fn e2e_confirm_tool_call_timeout_skips_tool() {
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
    // engine's timeout to fire.
    let _ = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolConfirmRequired {
                session_id: sid, ..
            } = ev
            {
                if *sid == session_id {
                    return Some(());
                }
            }
            None
        },
        "ToolConfirmRequired",
    )
    .await;

    let (result_content, is_error) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((result.content.clone(), result.is_error));
                }
            }
            None
        },
        "ToolEnd(timeout)",
    )
    .await;
    assert!(is_error, "timed-out tool should be an error");
    assert!(
        result_content.contains("timeout"),
        "expected 'timeout' in content, got: {result_content}"
    );

    drain_until_end_turn(&mut rx, session_id).await;

    daemon.abort();
}
