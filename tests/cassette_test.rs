//! Cassette-based provider tests.
//!
//! This file implements the `CassetteProvider` test harness described in the
//! design doc §10: a `LlmProvider` impl that replays a recorded sequence of
//! `ProviderStreamEvent`s from a JSON file under `tests/cassettes/anthropic/`.
//! No network, no API key — the test is fully deterministic.
//!
//! Cassette format (one JSON file per recorded turn):
//! ```json
//! {
//!   "name": "chat_stream_tool_use",
//!   "events": [
//!     { "ToolCallStart": { "id": "tc_1", "name": "echo" } },
//!     { "Finish": { "stop_reason": "ToolUse", "usage": { ... } } }
//!   ]
//! }
//! ```
//! The `events` array is a `Vec<ProviderStreamEvent>` serialized in serde's
//! default (internally-tagged) enum format. `CassetteProvider::chat_stream`
//! pushes them into an mpsc channel in order, mimicking a real provider.
//!
//! Recording new cassettes (RECORD mode) is a follow-up; for now cassettes
//! are hand-authored to match the Anthropic SSE → ProviderStreamEvent mapping
//! in `crates/parrot-providers/src/anthropic.rs::parse_sse_stream`.

use async_trait::async_trait;
use parrot_core::error::{AgentError, ProviderError};
use parrot_core::provider::{ChatStream, LlmProvider, ProviderStreamEvent};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use parrot_core::types::{ChatMessage, GenerateConfig, ModelInfo};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A cassette: a named sequence of `StreamEvent`s to replay on one
/// `chat_stream` call. The `description` field is for humans; the test
/// harness ignores it.
#[derive(Debug, Deserialize)]
struct Cassette {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
    events: Vec<ProviderStreamEvent>,
}

fn load_cassette(name: &str) -> Cassette {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "cassettes",
        "anthropic",
        &format!("{name}.json"),
    ]
    .iter()
    .collect();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read cassette {name} at {path:?}: {e}"));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("failed to parse cassette {name}: {e}"))
}

/// A provider that replays cassettes in sequence. The first `chat_stream`
/// call replays `cassettes[0]`, the second replays `cassettes[1]`, etc.
/// If more calls are made than cassettes provided, the last cassette is
/// reused (useful for "always emit EndTurn" fallbacks).
struct CassetteProvider {
    cassettes: Vec<Cassette>,
    call_count: AtomicU32,
}

impl CassetteProvider {
    fn new(cassettes: Vec<Cassette>) -> Self {
        Self {
            cassettes,
            call_count: AtomicU32::new(0),
        }
    }

    /// Convenience: load multiple cassettes by name from the standard
    /// `tests/cassettes/anthropic/` directory.
    fn load(names: &[&str]) -> Self {
        let cassettes = names.iter().map(|n| load_cassette(n)).collect();
        Self::new(cassettes)
    }
}

#[async_trait]
impl LlmProvider for CassetteProvider {
    fn provider_id(&self) -> &str {
        "anthropic" // pretend to be Anthropic so the registry routes "claude-*" models here
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo {
            id: "claude-sonnet-4-6".to_string(),
            name: "Claude Sonnet 4.6 (cassette)".to_string(),
            provider: "anthropic".to_string(),
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
        let idx = (n as usize).min(self.cassettes.len() - 1);
        let events = self.cassettes[idx].events.clone();

        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);
        tokio::spawn(async move {
            for event in events {
                if tx.send(event).await.is_err() {
                    // Engine dropped the receiver (e.g. Abort). Stop replaying.
                    break;
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
        unimplemented!("cassette tests use chat_stream only")
    }
}

// ---------------------------------------------------------------------------
// A simple echo tool for the ReAct loop to call
// ---------------------------------------------------------------------------

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
// Tests
// ---------------------------------------------------------------------------

/// Simplest path: a single-turn chat with no tool calls. The cassette emits
/// a TextDelta then Finish(EndTurn). Verifies the cassette harness wires up
/// correctly and the engine passes through text + finish.
#[tokio::test]
async fn cassette_simple_text_turn() {
    let provider = CassetteProvider::load(&["chat_stream_simple_text"]);
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_cassette_provider(provider).await;

    let session_id = create_session(&tx, &mut rx).await;
    tx.send(ClientMessage::Chat {
        session_id,
        message: "hi".to_string(),
    })
    .await
    .expect("send Chat");

    // Expect a TextDelta via the AgentEvent envelope.
    let text = expect_agent_event(
        &mut rx,
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
        "TextDelta",
    )
    .await;
    assert_eq!(text, "Hello from the cassette.");

    // Expect TurnEnd(EndTurn).
    let stop = expect_agent_event(
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
        "TurnEnd(EndTurn)",
    )
    .await;
    assert_eq!(stop, TurnStopReason::EndTurn);

    daemon_handle.abort();
}

/// Full ReAct loop: the first cassette emits a tool_use block, the engine
/// executes `echo`, the second cassette emits the final text + EndTurn.
/// This mirrors the existing mock-provider e2e test but drives it from a
/// JSON cassette on disk — proving the cassette framework can replace the
/// inline mock for Anthropic-shaped responses.
#[tokio::test]
async fn cassette_react_loop_with_tool_use() {
    let provider = CassetteProvider::load(&["chat_stream_tool_use", "chat_stream_end_turn"]);
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_cassette_provider(provider).await;

    let session_id = create_session(&tx, &mut rx).await;
    tx.send(ClientMessage::Chat {
        session_id,
        message: "please echo hello".to_string(),
    })
    .await
    .expect("send Chat");

    // 1. ToolStart(echo)
    let tool_name = expect_agent_event(
        &mut rx,
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

    // 2. ToolEnd(echo: hello)
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
    assert_eq!(result_content, "echo: hello");
    assert!(!is_error);

    // 3. Final TextDelta("done") from the second cassette
    let final_text = expect_agent_event(
        &mut rx,
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

    // 4. TurnEnd(EndTurn)
    let stop = expect_agent_event(
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
        "TurnEnd(EndTurn)",
    )
    .await;
    assert_eq!(stop, TurnStopReason::EndTurn);

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// Helpers (shared with the inline tests above; duplicated here so this test
// file is self-contained — cargo treats each integration test file as its
// own crate, so we can't share modules without #[path] gymnastics).
// ---------------------------------------------------------------------------

use parrot_config::AppConfig;
use parrot_core::provider::ProviderRegistry;
use parrot_core::tool::ToolRegistry;
use parrot_protocol::agent_event::{AgentEvent, MessageDeltaPayload, TurnStopReason};
use parrot_protocol::types::SessionConfig;
use parrot_protocol::{ClientMessage, ServerMessage};
use parrot_transport::TransportClient;
use tokio::time::{timeout, Duration};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn test_config(port: u16, data_dir: &Path, token_path: &Path) -> AppConfig {
    let mut config = AppConfig::default_config();
    config.daemon.host = "127.0.0.1".to_string();
    config.daemon.port = port;
    config.daemon.auth_token_file = token_path.to_string_lossy().to_string();
    config.session.data_dir = data_dir.to_string_lossy().to_string();
    config.providers.push(parrot_config::ProviderConfig {
        id: "anthropic".to_string(),
        protocol: "anthropic".to_string(),
        api_key: String::new(),
        default_model: "claude-sonnet-4-6".to_string(),
        base_url: None,
        models: vec!["claude-sonnet-4-6".into()],
        max_tokens: None,
    });
    config
}

async fn spawn_daemon_with_cassette_provider(
    provider: CassetteProvider,
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
        .register(
            Arc::new(provider) as Arc<dyn LlmProvider>,
            vec!["claude-sonnet-4-6".to_string()],
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

// ---------------------------------------------------------------------------
// file_edit e2e：真实 file_read / file_edit 工具经 cassette 驱动，完成一次
// 真实落盘编辑（read-before-edit 护栏必须放行：file_read 先成功）。
// 引擎 working_dir = 测试进程 CWD（包根），故用包根下专名文件并在结束时清理。
// ---------------------------------------------------------------------------

const E2E_FILE: &str = "parrot_file_edit_e2e.txt";

#[tokio::test]
async fn cassette_file_edit_rewrites_file_on_disk() {
    let path = std::path::PathBuf::from(E2E_FILE);
    let _ = std::fs::remove_file(&path); // 清理上次失败残留
    std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").expect("seed file");

    let provider = CassetteProvider::load(&[
        "chat_stream_file_read",
        "chat_stream_file_edit",
        "chat_stream_end_turn",
    ]);

    // spawn 逻辑与 spawn_daemon_with_cassette_provider 相同，但注册真实的
    // file_read / file_edit 工具（该文件自包含，不做共享重构）。
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
            Arc::new(provider) as Arc<dyn LlmProvider>,
            vec!["claude-sonnet-4-6".to_string()],
        )
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(parrot_tools::file_read::FileReadTool::new()) as Arc<dyn Tool>)
        .await;
    tool_registry
        .register(Arc::new(parrot_tools::file_edit::FileEditTool::new()) as Arc<dyn Tool>)
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
    let mut conn = match client.connect(&url, &token).await {
        Ok(c) => c,
        Err(e) => {
            daemon_handle.abort();
            let _ = std::fs::remove_file(&path);
            panic!("client connect: {e}");
        }
    };
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

    let session_id = create_session(&conn.sender, &mut conn.receiver).await;
    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "edit the file".to_string(),
        })
        .await
        .expect("send Chat");

    // 1. ToolStart(file_read) → ToolEnd 成功
    let name = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolStart {
                session_id: sid,
                tool_name,
                ..
            } if *sid == session_id => Some(tool_name.clone()),
            _ => None,
        },
        "ToolStart(file_read)",
    )
    .await;
    assert_eq!(name, "file_read");

    let (content, is_error) = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } if *sid == session_id => Some((result.content.clone(), result.is_error)),
            _ => None,
        },
        "ToolEnd(file_read)",
    )
    .await;
    assert!(!is_error, "file_read failed: {content}");

    // 2. ToolStart(file_edit) → ToolEnd "Successfully replaced 1 occurrence"
    let name = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolStart {
                session_id: sid,
                tool_name,
                ..
            } if *sid == session_id => Some(tool_name.clone()),
            _ => None,
        },
        "ToolStart(file_edit)",
    )
    .await;
    assert_eq!(name, "file_edit");

    let (content, is_error) = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } if *sid == session_id => Some((result.content.clone(), result.is_error)),
            _ => None,
        },
        "ToolEnd(file_edit)",
    )
    .await;
    assert!(!is_error, "file_edit failed: {content}");
    assert!(
        content.contains("Successfully replaced 1 occurrence"),
        "unexpected edit output: {content}"
    );

    // 3. TurnEnd(EndTurn)
    let stop = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason,
                ..
            } if *sid == session_id => Some(stop_reason.clone()),
            _ => None,
        },
        "TurnEnd(EndTurn)",
    )
    .await;
    assert_eq!(stop, TurnStopReason::EndTurn);

    // 4. 落盘断言：文件被真实改写
    let after = std::fs::read_to_string(&path).expect("read edited file");
    assert!(
        after.contains("println!(\"edited\")"),
        "file was not edited: {after}"
    );
    assert!(
        !after.contains("println!(\"hello\")"),
        "old text still present: {after}"
    );

    let _ = std::fs::remove_file(&path);
    daemon_handle.abort();
}
