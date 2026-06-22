//! End-to-end integration tests.
//!
//! The async test below drives the full MVP acceptance path through the real
//! WebSocket transport with a mock LLM provider injected in place of Anthropic:
//!   client Hello (token) → HelloAck → CreateSession → Chat
//!   → provider emits tool_use → daemon executes the tool → ToolResult
//!   → provider emits end_turn text → Finished(EndTurn)
//!
//! No network, no API key, no cassette — the mock provider produces a canned
//! stream per call. This validates the spec's MVP criteria: CLI-class client →
//! daemon → engine → tool → streaming response, with token verification.

use parrot_config::AppConfig;
use parrot_protocol::types::{SessionConfig, StopReason};
use parrot_protocol::{ClientMessage, ServerMessage};

#[test]
fn test_config_loads() {
    // Use default config instead of loading from file (which requires ANTHROPIC_API_KEY)
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
    let msg = ServerMessage::Finished {
        session_id: uuid::Uuid::new_v4(),
        stop_reason: parrot_protocol::types::StopReason::EndTurn,
        usage: parrot_protocol::types::Usage {
            input_tokens: 100,
            output_tokens: 50,
        },
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

// ---------------------------------------------------------------------------
// Mock provider + echo tool (mirrors crates/parrot-core/tests/react_loop.rs)
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use parrot_core::error::{AgentError, ProviderError};
use parrot_core::event_log::StreamEvent;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderRegistry};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::{ChatMessage, GenerateConfig, ModelInfo};
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

/// Grab a free TCP port by binding once and dropping the listener.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Build an AppConfig suitable for an ephemeral test daemon.
fn test_config(port: u16, data_dir: &std::path::Path, token_path: &std::path::Path) -> AppConfig {
    let mut config = AppConfig::default_config();
    config.daemon.host = "127.0.0.1".to_string();
    config.daemon.port = port;
    config.daemon.auth_token_file = token_path.to_string_lossy().to_string();
    config.session.data_dir = data_dir.to_string_lossy().to_string();
    // The mock provider is injected directly; no provider config needed. But
    // run_with derives the default GenerateConfig.model from the first provider
    // config, so supply a stub pointing at the mock model.
    config.providers.push(parrot_config::ProviderConfig {
        id: "mock".to_string(),
        api_key: String::new(),
        default_model: "mock-model".to_string(),
        base_url: None,
        models: vec!["mock-model".to_string()],
    });
    config
}

/// Collect server messages until the predicate returns Some, or the deadline.
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
                    // keep draining until we hit the one we want
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

    // Initialize auth (writes a token to token_path on first run).
    let auth = parrot::auth::Auth::new(&token_path)
        .await
        .expect("init auth");
    let token = {
        // Auth.token is private; read it back from the file the daemon wrote.
        std::fs::read_to_string(&token_path)
            .unwrap()
            .trim()
            .to_string()
    };
    assert!(!token.is_empty(), "auth token file should be populated");
    let auth = Arc::new(auth);

    // Registries: mock provider + echo tool.
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

    // Start the daemon.
    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot::server::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Connect a real WS client.
    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("client connect");

    // 1. Expect HelloAck (client auto-sent Hello with the real token).
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

    // 4. Expect the full ReAct flow relayed to the client:
    //    ToolCallStart(echo) → ToolResult(echo: hello) → TextDelta("done") → Finished(EndTurn)
    let tool_start = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::ToolCallStart {
                session_id: sid,
                tool_name,
                ..
            } = m
            {
                if *sid == session_id {
                    return Some(tool_name.clone());
                }
            }
            None
        },
        "ToolCallStart",
    )
    .await;
    assert_eq!(tool_start, "echo");

    let tool_result = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::ToolResult {
                session_id: sid,
                result,
                ..
            } = m
            {
                if *sid == session_id {
                    return Some((result.content.clone(), result.is_error));
                }
            }
            None
        },
        "ToolResult",
    )
    .await;
    assert_eq!(tool_result.0, "echo: hello");
    assert!(!tool_result.1, "tool result should not be an error");

    let final_text = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::TextDelta {
                session_id: sid,
                delta,
            } = m
            {
                if *sid == session_id {
                    return Some(delta.clone());
                }
            }
            None
        },
        "TextDelta",
    )
    .await;
    assert_eq!(final_text, "done");

    let (stop_reason, _usage) = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::Finished {
                session_id: sid,
                stop_reason,
                usage,
            } = m
            {
                if *sid == session_id {
                    return Some((stop_reason.clone(), usage.clone()));
                }
            }
            None
        },
        "Finished",
    )
    .await;
    assert_eq!(stop_reason, StopReason::EndTurn);

    // The daemon task runs an accept loop forever; abort it to clean up.
    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// Additional E2E tests: ListModels, GetHistory, Abort mid-stream
// ---------------------------------------------------------------------------

/// A mock provider that emits ToolCallStart then blocks for a long time before
/// emitting anything else. Used by the Abort test to give the client a window
/// to inject `Abort` while the stream is in flight.
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
        let (tx, rx) = mpsc::channel::<StreamEvent>(16);
        tokio::spawn(async move {
            tx.send(StreamEvent::ToolCallStart {
                id: "tc_slow".to_string(),
                name: "echo".to_string(),
            })
            .await
            .ok();
            // Block long enough for the test to send Abort. If the engine
            // drops the receiver due to Abort, these sends fail silently.
            tokio::time::sleep(Duration::from_secs(10)).await;
            tx.send(StreamEvent::ToolCallEnd {
                id: "tc_slow".to_string(),
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

/// Shared setup: spin up a daemon with the given provider + echo tool,
/// connect a WS client, return the connection + auth token + daemon handle.
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
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(EchoTool) as Arc<dyn Tool>)
        .await;

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot::server::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("client connect");

    // Wait for HelloAck so the caller gets a clean authenticated channel.
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

    // Leak the tempdir so it persists for the daemon's session writes. The
    // daemon handle is aborted by the caller; tempdir cleanup happens when
    // the process exits. Acceptable for tests.
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

    // Create a session and run a full chat so events.log gets populated.
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

    // Drain the full ReAct flow until Finished(EndTurn).
    expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::Finished { stop_reason, .. } = m {
                if *stop_reason == StopReason::EndTurn {
                    Some(())
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

    // Now request history and verify entries are present.
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

    // The engine appends: UserMessage, (ToolCall, ToolResult), Finish.
    // At minimum we should see the UserMessage and the Finish entries.
    assert!(
        !entries.is_empty(),
        "history should not be empty after a chat"
    );
    assert!(entries.iter().any(|e| {
        matches!(
            &e.entry,
            parrot_protocol::types::EventLogEntry::UserMessage { content } if content == "please echo hello"
        )
    }), "history should contain the user message");
    assert!(
        entries.iter().any(|e| {
            matches!(
                &e.entry,
                parrot_protocol::types::EventLogEntry::Finish {
                    stop_reason: StopReason::EndTurn,
                    ..
                }
            )
        }),
        "history should contain the EndTurn finish"
    );

    daemon_handle.abort();
}

#[tokio::test]
async fn e2e_abort_mid_stream_cancels_react_turn() {
    let provider = Arc::new(BlockingMockProvider) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    // Create a session.
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

    // Send a chat — the blocking provider will emit ToolCallStart then hang.
    tx.send(ClientMessage::Chat {
        session_id,
        message: "echo something".to_string(),
    })
    .await
    .expect("send Chat");

    // Wait for ToolCallStart to confirm the stream is in flight.
    expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::ToolCallStart {
                session_id: sid,
                tool_name,
                ..
            } = m
            {
                if *sid == session_id {
                    Some(tool_name.clone())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "ToolCallStart",
    )
    .await;

    // Inject Abort mid-stream. The engine's `tokio::select!` on cmd_rx
    // should observe this, drop the stream, and emit Finished(Aborted).
    tx.send(ClientMessage::Abort { session_id })
        .await
        .expect("send Abort");

    // We expect a Finished(Aborted) within a reasonable time. The test
    // proves the Abort actually cancels the in-flight turn rather than
    // waiting for the 10-second blocking provider to finish.
    let stop_reason = expect_server_message(
        &mut rx,
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
        "Finished(Aborted)",
    )
    .await;

    assert_eq!(
        stop_reason,
        StopReason::Aborted,
        "expected Abort to cancel the in-flight turn"
    );

    daemon_handle.abort();
}
