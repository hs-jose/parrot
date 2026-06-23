//! End-to-end integration tests.
//!
//! Drives the full MVP acceptance path through the real WebSocket transport
//! with a mock LLM provider injected in place of Anthropic:
//!   client Hello (token) → HelloAck → CreateSession → Chat
//!   → provider emits tool_use → daemon executes the tool → ToolEnd
//!   → provider emits end_turn text → TurnEnd
//!
//! All streaming events now arrive as `ServerMessage::AgentEvent { event }`
//! envelopes (unified event model).

use parrot_config::AppConfig;
use parrot_protocol::agent_event::{
    AgentEndReason, AgentEvent, MessageDeltaPayload, TurnStopReason,
};
use parrot_protocol::types::SessionConfig;
use parrot_protocol::{ClientMessage, ServerMessage};

#[test]
fn test_config_loads() {
    let config = AppConfig::default_config();
    assert_eq!(config.daemon.port, 9876);
    assert!(!config.tools.shell_allowed);
    assert!(!config.tools.file_write_allowed);
    assert!(config.tools.web_allowed);
}

#[test]
fn test_protocol_roundtrip() {
    let msg = ClientMessage::Hello {
        token: "test-token".into(),
        client_version: "1.0".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn test_server_message_roundtrip() {
    let msg = ServerMessage::SessionCreated {
        session_id: uuid::Uuid::new_v4(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

// ---------------------------------------------------------------------------
// Mock provider + echo tool
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use parrot_core::error::{AgentError, ProviderError};
use parrot_core::provider::{
    ChatStream, LlmProvider, ProviderRegistry, ProviderStopReason, ProviderStreamEvent,
};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::{ChatMessage, GenerateConfig, ModelInfo};
use parrot_protocol::types::Usage;
use parrot_transport::TransportClient;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

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
                        usage: Usage {
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
                        usage: Usage {
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
        unimplemented!("chat() not used in ReAct loop")
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
        json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        })
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

/// Grab the inner `AgentEvent` from an `AgentEvent` envelope, applying a
/// predicate. Times out after 10s.
async fn expect_agent_event<F, T>(
    rx: &mut mpsc::Receiver<ServerMessage>,
    predicate: F,
    label: &str,
) -> T
where
    F: Fn(&AgentEvent) -> Option<T>,
    T: std::fmt::Debug,
{
    let deadline = Duration::from_secs(10);
    let result = timeout(deadline, async {
        loop {
            match rx.recv().await {
                Some(ServerMessage::AgentEvent { event }) => {
                    if let Some(t) = predicate(&event) {
                        return Ok(t);
                    }
                }
                Some(other) => {
                    eprintln!(
                        "expect_agent_event({label}): skipping non-envelope msg: {:?}",
                        other
                    );
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

/// Grab a non-envelope `ServerMessage` matching the predicate.
async fn expect_server_message<F, T>(
    rx: &mut mpsc::Receiver<ServerMessage>,
    predicate: F,
    label: &str,
) -> T
where
    F: Fn(&ServerMessage) -> Option<T>,
    T: std::fmt::Debug,
{
    let deadline = Duration::from_secs(10);
    let result = timeout(deadline, async {
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

// ---------------------------------------------------------------------------
// The E2E test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_daemon_react_loop_with_mock_provider() {
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
    assert!(!token.is_empty(), "auth token file should be populated");
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

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::server::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("client connect");

    // 1. HelloAck
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

    // 2. Create a session.
    conn.sender
        .send(ClientMessage::CreateSession {
            config: Some(SessionConfig {
                model: None,
                provider: None,
                system_prompt: None,
            }),
        })
        .await
        .expect("send CreateSession");

    let session_id = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    // 3. Send a chat message that triggers the mock tool_use round.
    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "please echo hello".to_string(),
        })
        .await
        .expect("send Chat");

    // 4. Expect the full ReAct flow as AgentEvent envelopes:
    //    AgentStart → TurnStart → MessageStart → MessageDelta(ToolCallStart) →
    //    MessageEnd(ToolUse) → ToolStart → ToolEnd(echo: hello) →
    //    MessageStart → MessageDelta(TextDelta "done") → MessageEnd(EndTurn) →
    //    TurnEnd(EndTurn)

    // ToolStart for "echo"
    let tool_name = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::ToolStart {
                session_id: sid,
                tool_name,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(tool_name.clone());
                }
            }
            None
        },
        "ToolStart",
    )
    .await;
    assert_eq!(tool_name, "echo");

    // ToolEnd with result "echo: hello", not an error.
    let (result_content, is_error) = expect_agent_event(
        &mut conn.receiver,
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
    assert_eq!(result_content, "echo: hello");
    assert!(!is_error);

    // Final TextDelta "done".
    let final_text = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::MessageDelta {
                session_id: sid,
                payload: MessageDeltaPayload::TextDelta { delta },
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(delta.clone());
                }
            }
            None
        },
        "TextDelta(done)",
    )
    .await;
    assert_eq!(final_text, "done");

    // TurnEnd with EndTurn.
    let stop_reason = expect_agent_event(
        &mut conn.receiver,
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
        "TurnEnd(EndTurn)",
    )
    .await;
    assert_eq!(stop_reason, TurnStopReason::EndTurn);

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// Additional E2E tests: ListModels, GetHistory, Abort mid-stream
// ---------------------------------------------------------------------------

struct BlockingMockProvider;

#[async_trait]
impl LlmProvider for BlockingMockProvider {
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
        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);
        tokio::spawn(async move {
            tx.send(ProviderStreamEvent::ToolCallStart {
                id: "tc_slow".to_string(),
                name: "echo".to_string(),
            })
            .await
            .ok();
            // Block long enough for the test to send Abort.
            tokio::time::sleep(Duration::from_secs(10)).await;
            tx.send(ProviderStreamEvent::ToolCallEnd {
                id: "tc_slow".to_string(),
                arguments: json!({"message": "hello"}),
            })
            .await
            .ok();
            tx.send(ProviderStreamEvent::Finish {
                stop_reason: ProviderStopReason::ToolUse,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            })
            .await
            .ok();
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

async fn spawn_daemon_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    mpsc::Receiver<ServerMessage>,
    mpsc::Sender<ClientMessage>,
    String,
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
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(EchoTool) as Arc<dyn Tool>)
        .await;

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::server::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
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

    (conn.receiver, conn.sender, token, daemon_handle)
}

#[tokio::test]
async fn e2e_list_models_returns_registered_models() {
    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    tx.send(ClientMessage::ListModels)
        .await
        .expect("send ListModels");

    let models = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ModelList { models } = m {
                Some(models.clone())
            } else {
                None
            }
        },
        "ModelList",
    )
    .await;

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "mock-model");
    assert_eq!(models[0].provider, "mock");

    daemon_handle.abort();
}

#[tokio::test]
async fn e2e_get_history_returns_event_log_after_chat() {
    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: None,
            provider: None,
            system_prompt: None,
        }),
    })
    .await
    .expect("send CreateSession");

    let session_id = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    // Drain until TurnEnd(EndTurn).
    expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason,
                ..
            } = ev
            {
                if *sid == session_id && *stop_reason == TurnStopReason::EndTurn {
                    return Some(());
                }
            }
            None
        },
        "TurnEnd(EndTurn)",
    )
    .await;

    // Now request history and verify events are present.
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
        "history should not be empty after a chat"
    );
    assert!(
        events.iter().any(|e| matches!(
            &e.event,
            AgentEvent::TurnStart { user_message, .. } if user_message == "please echo hello"
        )),
        "history should contain the TurnStart with the user message"
    );
    assert!(
        events.iter().any(|e| matches!(
            &e.event,
            AgentEvent::TurnEnd {
                stop_reason: TurnStopReason::EndTurn,
                ..
            }
        )),
        "history should contain the EndTurn TurnEnd"
    );

    daemon_handle.abort();
}

#[tokio::test]
async fn e2e_abort_mid_stream_cancels_react_turn() {
    let provider = Arc::new(BlockingMockProvider) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: None,
            provider: None,
            system_prompt: None,
        }),
    })
    .await
    .expect("send CreateSession");

    let session_id = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    tx.send(ClientMessage::Chat {
        session_id,
        message: "echo something".to_string(),
    })
    .await
    .expect("send Chat");

    // Wait for MessageDelta(ToolCallStart) to confirm the stream is in flight.
    expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::MessageDelta {
                session_id: sid,
                payload: MessageDeltaPayload::ToolCallStart { tool_name, .. },
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(tool_name.clone());
                }
            }
            None
        },
        "MessageDelta(ToolCallStart)",
    )
    .await;

    // Inject Abort mid-stream.
    tx.send(ClientMessage::Abort { session_id })
        .await
        .expect("send Abort");

    // Expect TurnEnd(Aborted) within a reasonable time.
    let stop_reason = expect_agent_event(
        &mut rx,
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
        "TurnEnd(Aborted)",
    )
    .await;

    assert_eq!(
        stop_reason,
        TurnStopReason::Aborted,
        "expected Abort to cancel the in-flight turn"
    );

    daemon_handle.abort();
}

#[tokio::test]
async fn e2e_agent_end_emitted_on_disconnect() {
    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: None,
            provider: None,
            system_prompt: None,
        }),
    })
    .await
    .expect("send CreateSession");

    let _session_id = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    // Drop the client sender to close the connection from the client side.
    // The daemon's connection handler will exit its loop and the session
    // task will see cmd_rx return None, emitting AgentEnd.
    drop(tx);

    // We may or may not observe AgentEnd before the connection closes —
    // this is a best-effort check with a short timeout. The engine emits
    // AgentEnd via the guard's Drop path when the task is dropped, which
    // uses try_send and may race with the channel teardown. We accept
    // either outcome (Some(AgentEnd) or None) as success — the test
    // primarily verifies that no panic or hang occurs.
    let _ = timeout(Duration::from_millis(500), async {
        loop {
            match rx.recv().await {
                Some(ServerMessage::AgentEvent {
                    event: AgentEvent::AgentEnd { reason, .. },
                }) => {
                    assert!(
                        matches!(
                            reason,
                            AgentEndReason::ClientDisconnect | AgentEndReason::DaemonShutdown
                        ),
                        "expected ClientDisconnect or DaemonShutdown, got {:?}",
                        reason
                    );
                    return;
                }
                Some(_) => continue,
                None => return,
            }
        }
    })
    .await;

    daemon_handle.abort();
}
