//! ReAct loop integration test using a MockProvider.
//!
//! Drives the full ReAct engine through one tool-call cycle without touching
//! the network. Validates:
//! - Bug 1: `ToolContext.working_dir` is the engine's working dir, not the
//!   session data dir.
//! - Bug 2: context pruning preserves tool_use/tool_result pair boundaries
//!   (verified indirectly — the MockProvider inspects the messages it receives
//!   and the test asserts the tool_result message is preceded by the
//!   assistant tool_use message).
//! - Bug 4: `StreamEvent::ToolCallEnd` carries the parsed JSON arguments
//!   emitted by the provider, not an empty object.

use async_trait::async_trait;
use parrot_core::engine::ReActEngine;
use parrot_core::error::AgentError;
use parrot_core::event_log::StreamEvent;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderRegistry};
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::GenerateConfig;
use parrot_protocol::types::{StopReason, Usage};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A provider that emits a canned stream on each call.
/// Call 1: tool_use (echo tool with {"message": "hello"}).
/// Call 2: end_turn with text "done".
struct MockProvider {
    call_count: AtomicU32,
    /// Captured messages from the most recent call (for assertions via the test).
    captured_messages: tokio::sync::Mutex<Vec<parrot_core::types::ChatMessage>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
            captured_messages: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn captured_messages(&self) -> Vec<parrot_core::types::ChatMessage> {
        self.captured_messages.lock().await.clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    async fn list_models(&self) -> Result<Vec<parrot_core::types::ModelInfo>, parrot_core::error::ProviderError> {
        Ok(vec![parrot_core::types::ModelInfo {
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
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, parrot_core::error::ProviderError> {
        *self.captured_messages.lock().await = messages.to_vec();
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<StreamEvent>(16);

        tokio::spawn(async move {
            match n {
                0 => {
                    // First call: emit a tool_use block
                    tx.send(StreamEvent::ToolCallStart {
                        id: "tc_mock_1".to_string(),
                        name: "echo".to_string(),
                    }).await.ok();
                    tx.send(StreamEvent::ToolCallDelta {
                        id: "tc_mock_1".to_string(),
                        args_delta: r#"{"message":"hello"}"#.to_string(),
                    }).await.ok();
                    tx.send(StreamEvent::ToolCallEnd {
                        id: "tc_mock_1".to_string(),
                        arguments: json!({"message": "hello"}),
                    }).await.ok();
                    tx.send(StreamEvent::Finish {
                        stop_reason: StopReason::ToolUse,
                        usage: Usage { input_tokens: 10, output_tokens: 5 },
                    }).await.ok();
                }
                _ => {
                    // Subsequent calls: end_turn with final text
                    tx.send(StreamEvent::TextDelta {
                        delta: "done".to_string(),
                    }).await.ok();
                    tx.send(StreamEvent::Finish {
                        stop_reason: StopReason::EndTurn,
                        usage: Usage { input_tokens: 20, output_tokens: 10 },
                    }).await.ok();
                }
            }
        });

        Ok(ChatStream { inner: rx })
    }

    async fn chat(
        &self,
        _model: &str,
        _messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<parrot_core::types::ChatMessage, parrot_core::error::ProviderError> {
        unimplemented!("chat() is not used in ReAct loop; use chat_stream")
    }
}

/// Echo tool: returns the `message` argument as its output content.
/// Also stashes the ToolContext.working_dir it was called with into a shared
/// cell so the test can assert it matches the engine's working dir (not the
/// session data dir).
struct EchoTool {
    captured_working_dir: Arc<tokio::sync::Mutex<Option<std::path::PathBuf>>>,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo" }

    fn description(&self) -> &str { "Echoes the message argument back." }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        *self.captured_working_dir.lock().await = Some(ctx.working_dir.clone());
        let msg = arguments
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(ToolOutput {
            content: format!("echo: {}", msg),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn react_loop_executes_tool_call_and_finishes() {
    // Layout:
    //   tmpdir/working  — engine working_dir (where tools resolve paths)
    //   tmpdir/data     — session data dir (event log storage)
    let tmp = TempDir::new().expect("create temp dir");
    let working_dir = tmp.path().join("working");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Registries
    let tool_registry = Arc::new(ToolRegistry::new());
    let captured_wd = Arc::new(tokio::sync::Mutex::new(None));
    tool_registry
        .register(Arc::new(EchoTool { captured_working_dir: Arc::clone(&captured_wd) }))
        .await;

    let mock = Arc::new(MockProvider::new());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    // Engine
    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        data_dir.clone(),
        working_dir.clone(),
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(64);

    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Send a chat message
    cmd_tx
        .send(parrot_core::session::SessionCmd::Chat {
            message: "please echo hello".to_string(),
        })
        .await
        .unwrap();

    // Collect events until we see two Finish events (tool_use round, then end_turn round)
    let mut text_deltas = Vec::new();
    let mut tool_call_starts = Vec::new();
    let mut tool_call_ends = Vec::new();
    let mut tool_results = Vec::new();
    let mut finishes = Vec::new();

    while finishes.len() < 2 {
        match event_rx.recv().await {
            Some(StreamEvent::TextDelta { delta }) => text_deltas.push(delta),
            Some(StreamEvent::ToolCallStart { id, name }) => tool_call_starts.push((id, name)),
            Some(StreamEvent::ToolCallDelta { .. }) => {}
            Some(StreamEvent::ToolCallEnd { id, arguments }) => tool_call_ends.push((id, arguments)),
            Some(StreamEvent::ToolResult { id, result }) => tool_results.push((id, result)),
            Some(StreamEvent::Finish { stop_reason, usage }) => finishes.push((stop_reason, usage)),
            None => break,
        }
    }

    // Drop cmd_tx to let the engine's run loop exit.
    drop(cmd_tx);
    let _ = engine_task.await;

    // Assertions — Bug 4: ToolCallEnd carries parsed args from the provider
    assert_eq!(tool_call_starts.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_call_starts[0].1, "echo");
    assert_eq!(tool_call_ends.len(), 1, "expected exactly one ToolCallEnd");
    assert_eq!(
        tool_call_ends[0].1,
        json!({"message": "hello"}),
        "ToolCallEnd arguments must be the parsed JSON, not an empty object"
    );

    // Tool was actually executed
    assert_eq!(tool_results.len(), 1, "expected one ToolResult event");
    assert_eq!(tool_results[0].1.content, "echo: hello");
    assert!(!tool_results[0].1.is_error);

    // Two finish events: ToolUse round, then EndTurn round
    assert_eq!(finishes.len(), 2, "expected two Finish events (tool_use + end_turn)");
    assert_eq!(finishes[0].0, StopReason::ToolUse);
    assert_eq!(finishes[1].0, StopReason::EndTurn);

    // Final text delivered
    assert_eq!(text_deltas.concat(), "done");

    // Bug 1: ToolContext.working_dir is the engine's working dir, not the session data dir
    let captured = captured_wd.lock().await.clone().expect("tool was not called");
    assert_eq!(
        captured, working_dir,
        "ToolContext.working_dir must equal the engine's working_dir, not the session data dir"
    );
    assert_ne!(
        captured, data_dir,
        "working_dir must not accidentally be the session data dir"
    );

    // Bug 2 (indirect): on the second call, the provider received a context
    // containing the assistant tool_use message followed by the tool_result
    let captured_second = mock.captured_messages().await;
    let mut found_pair = false;
    for i in 0..captured_second.len().saturating_sub(1) {
        if let Some(tc) = &captured_second[i].tool_calls {
            if !tc.is_empty()
                && captured_second[i + 1].role == parrot_core::types::ChatRole::Tool
                && captured_second[i + 1].tool_call_id == Some(tc[0].id.clone())
            {
                found_pair = true;
                break;
            }
        }
    }
    assert!(
        found_pair,
        "second provider call must have an assistant tool_use followed by its matching tool_result"
    );
}
