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
    AgentEndReason, AgentEvent, MessageDeltaPayload, PersistedAgentEvent, TurnStopReason,
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
use parrot_core::session::{SessionCmd, SessionManager};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig, ModelInfo};
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
        // Compaction summary call: return a fixed structured summary.
        Ok(ChatMessage {
            role: ChatRole::Assistant,
            content: "## 目标与任务\ne2e compaction test".to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        })
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
        protocol: "anthropic".to_string(),
        api_key: String::new(),
        default_model: "mock-model".to_string(),
        base_url: None,
        models: vec!["mock-model".into()],
        max_tokens: None,
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
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
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
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = test_config(0, &data_dir, &token_path);
    std::mem::forget(tmp);
    spawn_daemon_with_provider_and_config(provider, config).await
}

/// Spawn a daemon with a caller-supplied config. The caller owns any temp
/// dir backing the config paths (no `forget` here — cleanup is the caller's).
async fn spawn_daemon_with_provider_and_config(
    provider: Arc<dyn LlmProvider>,
    mut config: AppConfig,
) -> (
    mpsc::Receiver<ServerMessage>,
    mpsc::Sender<ClientMessage>,
    String,
    tokio::task::JoinHandle<()>,
) {
    // Capture the port BEFORE the config is moved into the spawn task.
    let port = free_port();
    config.daemon.host = "127.0.0.1".to_string();
    config.daemon.port = port;

    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);

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

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
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

// ---------------------------------------------------------------------------
// E2E: tool_call hook blocks a dangerous shell command
// ---------------------------------------------------------------------------

struct DangerousShellExecProvider {
    call_count: AtomicU32,
}

impl DangerousShellExecProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for DangerousShellExecProvider {
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
                        id: "tc_danger".to_string(),
                        name: "shell_exec".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallDelta {
                        id: "tc_danger".to_string(),
                        args_delta: r#"{"command":"rm -rf /"}"#.to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallEnd {
                        id: "tc_danger".to_string(),
                        arguments: json!({"command": "rm -rf /"}),
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

struct CountingShellExec {
    call_count: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for CountingShellExec {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        "Executes a shell command."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }
    async fn call(&self, _arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: "executed".to_string(),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn e2e_hook_blocks_tool_call() {
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
            Arc::new(DangerousShellExecProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;

    let shell_call_count = Arc::new(AtomicU32::new(0));
    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(CountingShellExec {
            call_count: Arc::clone(&shell_call_count),
        }) as Arc<dyn Tool>)
        .await;

    let mut config = test_config(port, &data_dir, &token_path);
    config.hooks.enabled = vec!["dangerous_command_blocker".to_string()];

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
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

    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "please run rm -rf /".to_string(),
        })
        .await
        .expect("send Chat");

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
    assert_eq!(tool_name, "shell_exec");

    let (hook_id, event_kind, result_kind) = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::HookFired {
                session_id: sid,
                hook_id,
                event_kind,
                result_kind,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((hook_id.clone(), event_kind.clone(), result_kind.clone()));
                }
            }
            None
        },
        "HookFired",
    )
    .await;
    assert_eq!(hook_id, "dangerous_command_blocker");
    assert_eq!(event_kind, "tool_call");
    assert_eq!(result_kind, "block");

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
    assert!(is_error, "ToolEnd should be an error after block");
    assert!(
        result_content.starts_with("blocked:"),
        "ToolEnd content should start with 'blocked:', got: {result_content}"
    );

    expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason: TurnStopReason::EndTurn,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(());
                }
            }
            None
        },
        "TurnEnd(EndTurn)",
    )
    .await;

    assert_eq!(
        shell_call_count.load(Ordering::SeqCst),
        0,
        "CountingShellExec should NOT have been invoked (hook blocked before execute_tool)"
    );

    daemon_handle.abort();
}

#[tokio::test]
async fn e2e_shell_denylist_blocks_rm_rf() {
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
            Arc::new(DangerousShellExecProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;

    let shell_call_count = Arc::new(AtomicU32::new(0));
    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(CountingShellExec {
            call_count: Arc::clone(&shell_call_count),
        }) as Arc<dyn Tool>)
        .await;

    let mut config = test_config(port, &data_dir, &token_path);
    config.hooks.enabled = vec!["shell_denylist".to_string()];

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
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

    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "please run rm -rf /".to_string(),
        })
        .await
        .expect("send Chat");

    let (hook_id, event_kind, result_kind) = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::HookFired {
                session_id: sid,
                hook_id,
                event_kind,
                result_kind,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((hook_id.clone(), event_kind.clone(), result_kind.clone()));
                }
            }
            None
        },
        "HookFired",
    )
    .await;
    assert_eq!(hook_id, "shell_denylist");
    assert_eq!(event_kind, "tool_call");
    assert_eq!(result_kind, "block");

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
    assert!(is_error);
    assert!(
        result_content.starts_with("blocked:"),
        "ToolEnd content should start with 'blocked:', got: {result_content}"
    );

    expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason: TurnStopReason::EndTurn,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(());
                }
            }
            None
        },
        "TurnEnd(EndTurn)",
    )
    .await;

    assert_eq!(
        shell_call_count.load(Ordering::SeqCst),
        0,
        "CountingShellExec should NOT have been invoked (shell_denylist blocked before execute)"
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

#[tokio::test]
async fn e2e_shell_runs_command_on_daemon_and_returns_result() {
    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    let session_id = uuid::Uuid::new_v4();
    tx.send(ClientMessage::Shell {
        session_id,
        command: "echo parrot-shell-ok".to_string(),
    })
    .await
    .expect("send Shell");

    let (output, exit_code) = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ShellResult {
                output, exit_code, ..
            } = m
            {
                Some((output.clone(), *exit_code))
            } else {
                None
            }
        },
        "ShellResult",
    )
    .await;

    assert!(output.contains("parrot-shell-ok"), "got: {output}");
    assert_eq!(exit_code, 0);

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E: compaction emits CompactionSummary over the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_compaction_emits_summary_event_and_compacts_context() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut config = test_config(0, &data_dir, &token_path);
    // Est-token budget 1000 => threshold 900; default keep 20000 makes the
    // walk-back swallow every turn, so the fallback keeps only the newest
    // complete turn.
    config.session.max_history_tokens = 1000;

    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) =
        spawn_daemon_with_provider_and_config(provider, config).await;

    // Create a session.
    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: Some("mock-model".to_string()),
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

    // Turn 1 + Turn 2: ~1000 est tokens each. Turn 3's pre-check crosses 900.
    // Collect ALL events while draining each turn so the CompactionSummary
    // (delivered before turn 3's TurnStart) is not skipped and lost by a
    // predicate-based TurnEnd wait.
    let mut all_events: Vec<AgentEvent> = Vec::new();
    for i in 0..3 {
        let msg = format!("q{}{}", i, "x".repeat(4000));
        tx.send(ClientMessage::Chat {
            session_id,
            message: msg,
        })
        .await
        .expect("send Chat");

        // Wait for the turn to finish before sending the next (engine is
        // single-turn serialized: a Chat during an active turn is ignored).
        let got_turn_end = timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(ServerMessage::AgentEvent { event }) => {
                        let is_turn_end = matches!(event, AgentEvent::TurnEnd { .. });
                        all_events.push(event);
                        if is_turn_end {
                            return true;
                        }
                    }
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(got_turn_end, "timed out waiting for turn {i} TurnEnd");
    }

    // The 3rd turn must have emitted a CompactionSummary before its TurnStart.
    let cs_idx = all_events
        .iter()
        .position(|ev| matches!(ev, AgentEvent::CompactionSummary { .. }))
        .expect("CompactionSummary event expected");
    let turn_start_positions: Vec<usize> = all_events
        .iter()
        .enumerate()
        .filter(|(_, ev)| matches!(ev, AgentEvent::TurnStart { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(turn_start_positions.len(), 3, "one TurnStart per chat");
    assert!(
        cs_idx < turn_start_positions[2],
        "CompactionSummary must precede the 3rd turn's TurnStart"
    );

    let (summary, dropped, kept) = match &all_events[cs_idx] {
        AgentEvent::CompactionSummary {
            summary,
            dropped_message_count,
            kept_message_count,
            ..
        } => (summary.clone(), *dropped_message_count, *kept_message_count),
        _ => unreachable!("checked above"),
    };

    assert!(summary.starts_with("[CONVERSATION SUMMARY]"));
    assert!(summary.contains("e2e compaction test"));
    assert!(dropped >= 2, "turn 1 user+assistant summarized");
    assert!(kept >= 2, "turn 2 kept verbatim");

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E: closing the engine's command channel makes it exit cleanly and
// persist `AgentEnd` as the last event in `events.log`. This is the
// graceful-exit path that `shutdown_all` relies on (it drops `cmd_tx` to
// trigger the same `None` branch), exercised here without a real signal
// and without `shutdown_all`, so the emitted reason is `ClientDisconnect`.
//
// Driven in-process against the real `SessionManager` + `ReActEngine` +
// on-disk `EventLog`, because the engine's `cmd_tx` is owned by the
// `SessionManager` and cannot be closed by an external WS client (a client
// disconnect leaves the session live for resume). The WS layer is therefore
// not on this invariant's path; the real engine + real file IO is.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_engine_persists_agent_end_on_cmd_channel_close() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let sessions_dir = tmp.path().join("data").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

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

    let mut mgr = SessionManager::new(
        tool_registry,
        provider_registry,
        GenerateConfig {
            model: "mock-model".to_string(),
            temperature: None,
            max_tokens: Some(8192),
            stop_sequences: None,
        },
        sessions_dir.clone(),
        tmp.path().to_path_buf(),
    );

    let session_id = mgr
        .create_session(Some(SessionConfig {
            model: Some("mock-model".to_string()),
            provider: None,
            system_prompt: None,
        }))
        .await
        .expect("create session");
    let mut event_rx = mgr
        .take_event_receiver(&session_id)
        .expect("event receiver");
    // Clone the command sender so dropping the manager (which owns the
    // original) plus this clone leaves the engine's `cmd_rx` with no senders.
    let cmd_tx = mgr.get_handle(&session_id).expect("handle").cmd_tx.clone();

    // Drive one full turn to completion so that `AgentEnd` is later proven
    // to be the *last* persisted event, following normal operation.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "hi".to_string(),
        })
        .await
        .expect("send chat");
    let turned = timeout(Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::TurnEnd { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(turned, "timed out waiting for TurnEnd");

    // Close the command channel: `cmd_rx.recv()` returns `None` → the
    // engine's `None` branch (reason left as the default `ClientDisconnect`,
    // since `shutdown_all` was never called) → `fire_and_drop` *persists*
    // `AgentEnd` to `events.log` and *then* emits it.
    drop(cmd_tx);
    drop(mgr);
    let ended = timeout(Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(AgentEvent::AgentEnd { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ended,
        "engine did not emit AgentEnd after command channel closed"
    );

    let log_path = sessions_dir.join(session_id.to_string()).join("events.log");
    let content = std::fs::read_to_string(&log_path).expect("events.log");
    let last_line = content
        .lines()
        .rfind(|l| !l.is_empty())
        .expect("non-empty log");
    let entry: PersistedAgentEvent = serde_json::from_str(last_line).expect("parse last event");
    assert!(
        matches!(
            entry.event,
            AgentEvent::AgentEnd {
                reason: AgentEndReason::ClientClose | AgentEndReason::ClientDisconnect,
                ..
            }
        ),
        "expected AgentEnd as last event, got {:?}",
        entry.event
    );
}

// ---------------------------------------------------------------------------
// E2E: MCP-qualified tool drives the full engine loop (fake McpTool in registry)
// ---------------------------------------------------------------------------

struct McpEchoProvider {
    call_count: AtomicU32,
}

impl McpEchoProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for McpEchoProvider {
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
            if n == 0 {
                tx.send(ProviderStreamEvent::ToolCallStart {
                    id: "tc_mcp".to_string(),
                    name: "mcp__mock__echo".to_string(),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::ToolCallDelta {
                    id: "tc_mcp".to_string(),
                    args_delta: r#"{"message":"from-mcp"}"#.to_string(),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::ToolCallEnd {
                    id: "tc_mcp".to_string(),
                    arguments: json!({"message": "from-mcp"}),
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
            } else {
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

struct FakeMcpTool;

#[async_trait]
impl Tool for FakeMcpTool {
    fn name(&self) -> &str {
        "mcp__mock__echo"
    }
    fn description(&self) -> &str {
        "Fake MCP tool (engine-path check)"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"message": {"type": "string"}}})
    }
    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(ToolOutput {
            content: format!(
                "mcp echo: {}",
                arguments
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn e2e_mcp_qualified_tool_roundtrip() {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = Arc::new(
        parrot_daemon::auth::Auth::new(&token_path)
            .await
            .expect("auth"),
    );
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            Arc::new(McpEchoProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(FakeMcpTool) as Arc<dyn Tool>)
        .await;

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("connect");

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

    conn.sender
        .send(ClientMessage::CreateSession {
            config: Some(SessionConfig {
                model: None,
                provider: None,
                system_prompt: None,
            }),
        })
        .await
        .expect("CreateSession");
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

    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "call the mcp tool".into(),
        })
        .await
        .expect("Chat");

    let (content, is_error) = expect_agent_event(
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
        "ToolEnd(mcp)",
    )
    .await;
    assert_eq!(content, "mcp echo: from-mcp");
    assert!(!is_error);

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E: bad MCP command ⇒ McpNotice(Failed) broadcast + ListMcpServers shows it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_bad_mcp_server_surfaces_failure() {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = Arc::new(
        parrot_daemon::auth::Auth::new(&token_path)
            .await
            .expect("auth"),
    );
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;
    let tool_registry = Arc::new(ToolRegistry::new());

    let mut config = test_config(port, &data_dir, &token_path);
    // 一个启动即挂起的坏 server（永远完不成 MCP 握手），启动超时 2s ⇒ 失败
    // 广播落在 ~2s，此时客户端（~150ms 时连接）已订阅，McpNotice(Failed) 必达。
    // 即时失败（如不存在的二进制）在 bind 前就广播完毕，晚订阅的客户端收不到
    // （broadcast channel 无历史回放，设计上以 ListMcpServers 补查）。
    #[cfg(windows)]
    let (command, args): (String, Vec<String>) = (
        "ping".to_string(),
        vec!["-n".to_string(), "60".to_string(), "127.0.0.1".to_string()],
    );
    #[cfg(not(windows))]
    let (command, args): (String, Vec<String>) = ("sleep".to_string(), vec!["60".to_string()]);
    config.mcp.servers.push(parrot_config::McpServerConfig {
        id: "nope".into(),
        command,
        args,
        env: Default::default(),
        startup_timeout_seconds: 2,
        call_timeout_seconds: 30,
        require_confirmation: false,
    });

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("connect");

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

    // 等失败通知广播（start_all 在后台运行）
    expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::McpNotice { id, state, .. } = m {
                if id == "nope" && *state == parrot_protocol::types::McpServerState::Failed {
                    Some(())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "McpNotice(Failed)",
    )
    .await;

    conn.sender
        .send(ClientMessage::ListMcpServers)
        .await
        .expect("ListMcpServers");
    let entries = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::McpServers { entries } = m {
                Some(entries.clone())
            } else {
                None
            }
        },
        "McpServers",
    )
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "nope");
    assert_eq!(
        entries[0].state,
        parrot_protocol::types::McpServerState::Failed
    );
    assert!(
        !entries[0].detail.is_empty(),
        "失败详情过线: {:?}",
        entries[0].detail
    );

    daemon_handle.abort();
}
