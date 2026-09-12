//! ReAct loop integration test using a MockProvider.
//!
//! Drives the full ReAct engine through one tool-call cycle without touching
//! the network. With the unified `AgentEvent` model, this asserts the
//! lifecycle bracket structure from spec §9:
//!   AgentStart → TurnStart → MessageStart → MessageDelta* → MessageEnd
//!   → ToolStart → ToolEnd → MessageStart → MessageDelta* → MessageEnd
//!   → TurnEnd → AgentEnd

use async_trait::async_trait;
use parrot_core::engine::ReActEngine;
use parrot_core::error::AgentError;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderRegistry, ProviderStreamEvent};
use parrot_core::session::SessionCmd;
use parrot_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry};
use parrot_core::types::GenerateConfig;
use parrot_protocol::agent_event::{AgentEndReason, AgentEvent, MessageStopReason, TurnStopReason};
use parrot_protocol::types::Usage;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;

use parrot_core::provider::ProviderStopReason;

struct MockProvider {
    call_count: AtomicU32,
    captured_messages: tokio::sync::Mutex<Vec<parrot_core::types::ChatMessage>>,
    /// Every provider call's messages (chat + chat_stream), in order.
    captured_calls: tokio::sync::Mutex<Vec<Vec<parrot_core::types::ChatMessage>>>,
    /// Every chat_stream call's model id, in order.
    captured_models: tokio::sync::Mutex<Vec<String>>,
    /// Every chat_stream call's GenerateConfig, in order.
    captured_configs: tokio::sync::Mutex<Vec<GenerateConfig>>,
    /// Scripted first-call tool call; `None` ⇒ every call is plain text.
    first_tool: Option<(String, Value)>,
    /// Reported via `list_models`; caps the engine's context budget.
    context_window: u32,
    /// Per-model context windows; when non-empty, `list_models` reports one
    /// entry per pair instead of the single default `mock-model`. Lets tests
    /// give two model ids different windows.
    model_windows: Vec<(String, u32)>,
    /// Text returned by the non-stream `chat()` (compaction summary call).
    summary_text: Option<String>,
    /// When true, `chat()` returns Err (simulates summary failure).
    fail_chat: bool,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
            captured_messages: tokio::sync::Mutex::new(Vec::new()),
            captured_calls: tokio::sync::Mutex::new(Vec::new()),
            captured_models: tokio::sync::Mutex::new(Vec::new()),
            captured_configs: tokio::sync::Mutex::new(Vec::new()),
            first_tool: Some(("echo".to_string(), json!({"message": "hello"}))),
            context_window: 200_000,
            model_windows: Vec::new(),
            summary_text: None,
            fail_chat: false,
        }
    }

    fn text_only(mut self) -> Self {
        self.first_tool = None;
        self
    }

    fn with_first_tool(mut self, name: &str, args: Value) -> Self {
        self.first_tool = Some((name.to_string(), args));
        self
    }

    fn with_context_window(mut self, window: u32) -> Self {
        self.context_window = window;
        self
    }

    fn with_model_window(mut self, id: &str, window: u32) -> Self {
        self.model_windows.push((id.to_string(), window));
        self
    }

    fn with_summary(mut self, text: &str) -> Self {
        self.summary_text = Some(text.to_string());
        self
    }

    fn fail_summary(mut self) -> Self {
        self.fail_chat = true;
        self
    }

    async fn captured_calls(&self) -> Vec<Vec<parrot_core::types::ChatMessage>> {
        self.captured_calls.lock().await.clone()
    }

    async fn captured_models(&self) -> Vec<String> {
        self.captured_models.lock().await.clone()
    }

    async fn captured_configs(&self) -> Vec<GenerateConfig> {
        self.captured_configs.lock().await.clone()
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

    async fn list_models(
        &self,
    ) -> Result<Vec<parrot_core::types::ModelInfo>, parrot_core::error::ProviderError> {
        if !self.model_windows.is_empty() {
            return Ok(self
                .model_windows
                .iter()
                .map(|(id, window)| parrot_core::types::ModelInfo {
                    id: id.clone(),
                    name: id.clone(),
                    provider: "mock".to_string(),
                    context_window: *window,
                    max_output_tokens: 8192,
                })
                .collect());
        }
        Ok(vec![parrot_core::types::ModelInfo {
            id: "mock-model".to_string(),
            name: "Mock Model".to_string(),
            provider: "mock".to_string(),
            context_window: self.context_window,
            max_output_tokens: 8192,
        }])
    }

    async fn chat_stream(
        &self,
        model: &str,
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatStream, parrot_core::error::ProviderError> {
        self.captured_models.lock().await.push(model.to_string());
        self.captured_configs.lock().await.push(config.clone());
        self.captured_calls.lock().await.push(messages.to_vec());
        *self.captured_messages.lock().await = messages.to_vec();
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let first_tool = self.first_tool.clone();

        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);

        tokio::spawn(async move {
            match (n, first_tool) {
                (0, Some((name, args))) => {
                    tx.send(ProviderStreamEvent::ToolCallStart {
                        id: "tc_mock_1".to_string(),
                        name: name.clone(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallDelta {
                        id: "tc_mock_1".to_string(),
                        args_delta: args.to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallEnd {
                        id: "tc_mock_1".to_string(),
                        arguments: args,
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
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<parrot_core::types::ChatMessage, parrot_core::error::ProviderError> {
        self.captured_calls.lock().await.push(messages.to_vec());
        if self.fail_chat {
            return Err(parrot_core::error::ProviderError::Network(
                "summary call failed".to_string(),
            ));
        }
        Ok(parrot_core::types::ChatMessage {
            role: parrot_core::types::ChatRole::Assistant,
            content: self
                .summary_text
                .clone()
                .unwrap_or_else(|| "SUMMARY".to_string()),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        })
    }
}

struct EchoTool {
    captured_working_dir: Arc<tokio::sync::Mutex<Option<std::path::PathBuf>>>,
}

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

/// Collect `AgentEvent`s until `TurnEnd` is observed. Does NOT wait for
/// `AgentEnd` — the caller drops `cmd_tx` afterwards to let the engine
/// exit, then drains the trailing events separately.
async fn collect_until_turn_end(event_rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(ev) = event_rx.recv().await {
        let is_turn_end = matches!(ev, AgentEvent::TurnEnd { .. });
        events.push(ev);
        if is_turn_end {
            break;
        }
    }
    events
}

/// Drain remaining events (post-TurnEnd) until `AgentEnd` is observed.
async fn drain_until_agent_end(event_rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(ev) = event_rx.recv().await {
        let is_agent_end = matches!(ev, AgentEvent::AgentEnd { .. });
        events.push(ev);
        if is_agent_end {
            break;
        }
    }
    events
}

fn variant_name(ev: &AgentEvent) -> &'static str {
    match ev {
        AgentEvent::AgentStart { .. } => "AgentStart",
        AgentEvent::AgentEnd { .. } => "AgentEnd",
        AgentEvent::TurnStart { .. } => "TurnStart",
        AgentEvent::TurnEnd { .. } => "TurnEnd",
        AgentEvent::MessageStart { .. } => "MessageStart",
        AgentEvent::MessageDelta { .. } => "MessageDelta",
        AgentEvent::MessageEnd { .. } => "MessageEnd",
        AgentEvent::ToolStart { .. } => "ToolStart",
        AgentEvent::ToolUpdate { .. } => "ToolUpdate",
        AgentEvent::ToolEnd { .. } => "ToolEnd",
        AgentEvent::ToolConfirmRequired { .. } => "ToolConfirmRequired",
        AgentEvent::ReplayIntegrityWarning { .. } => "ReplayIntegrityWarning",
        AgentEvent::HookFired { .. } => "HookFired",
        AgentEvent::CompactionStart { .. } => "CompactionStart",
        AgentEvent::CompactionSummary { .. } => "CompactionSummary",
    }
}

#[tokio::test]
async fn react_loop_emits_lifecycle_brackets() {
    let tmp = TempDir::new().expect("create temp dir");
    let working_dir = tmp.path().join("working");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let tool_registry = Arc::new(ToolRegistry::new());
    let captured_wd = Arc::new(tokio::sync::Mutex::new(None));
    tool_registry
        .register(Arc::new(EchoTool {
            captured_working_dir: Arc::clone(&captured_wd),
        }))
        .await;

    let mock = Arc::new(MockProvider::new());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("You are a test assistant.".to_string()),
        data_dir.clone(),
        working_dir.clone(),
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);

    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    cmd_tx
        .send(SessionCmd::Chat {
            message: "please echo hello".to_string(),
        })
        .await
        .unwrap();

    let mut events = collect_until_turn_end(&mut event_rx).await;

    // Drop cmd_tx to let the engine's run loop exit and emit AgentEnd.
    drop(cmd_tx);
    let trailing = drain_until_agent_end(&mut event_rx).await;
    events.extend(trailing);

    let _ = engine_task.await;

    // Assert the lifecycle bracket sequence.
    let bracket: Vec<&str> = events.iter().map(variant_name).collect();
    assert!(
        bracket.starts_with(&["AgentStart", "TurnStart", "MessageStart"]),
        "expected AgentStart → TurnStart → MessageStart prefix, got: {:?}",
        bracket
    );
    assert!(
        bracket.contains(&"MessageEnd"),
        "expected MessageEnd in: {:?}",
        bracket
    );
    assert!(
        bracket.contains(&"ToolStart"),
        "expected ToolStart in: {:?}",
        bracket
    );
    assert!(
        bracket.contains(&"ToolEnd"),
        "expected ToolEnd in: {:?}",
        bracket
    );
    // After ToolEnd, a second MessageStart..MessageEnd for the final assistant message.
    let tool_end_idx = bracket
        .iter()
        .position(|n| *n == "ToolEnd")
        .expect("ToolEnd present");
    let after_tool = &bracket[tool_end_idx + 1..];
    assert!(
        after_tool.contains(&"MessageStart"),
        "expected second MessageStart after ToolEnd: {:?}",
        bracket
    );
    assert!(
        after_tool.contains(&"MessageEnd"),
        "expected second MessageEnd after ToolEnd: {:?}",
        bracket
    );
    // TurnEnd then AgentEnd at the tail.
    assert!(
        bracket.contains(&"TurnEnd"),
        "expected TurnEnd in: {:?}",
        bracket
    );
    assert_eq!(
        bracket.last().copied(),
        Some("AgentEnd"),
        "expected AgentEnd last: {:?}",
        bracket
    );

    // Inspect the MessageEnd of the first LLM call: stop_reason = ToolUse,
    // tool_calls has one entry with parsed args.
    let first_msg_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::MessageEnd {
                stop_reason,
                tool_calls,
                final_content,
                ..
            } if *stop_reason == MessageStopReason::ToolUse => {
                Some((tool_calls.clone(), final_content.clone()))
            }
            _ => None,
        })
        .expect("first MessageEnd with ToolUse");
    assert_eq!(first_msg_end.0.len(), 1, "expected one tool call");
    assert_eq!(first_msg_end.0[0].tool_name, "echo");
    assert_eq!(first_msg_end.0[0].arguments, json!({"message": "hello"}));

    // Inspect the ToolEnd: result.content = "echo: hello", is_error = false.
    let tool_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::ToolEnd { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("ToolEnd present");
    assert_eq!(tool_end.content, "echo: hello");
    assert!(!tool_end.is_error);

    // Inspect the TurnEnd: stop_reason = EndTurn.
    let turn_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("TurnEnd present");
    assert_eq!(turn_end, TurnStopReason::EndTurn);

    // Inspect the AgentEnd: reason = ClientDisconnect (cmd_tx dropped).
    let agent_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::AgentEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("AgentEnd present");
    assert_eq!(agent_end, AgentEndReason::ClientDisconnect);

    // Bug 1: ToolContext.working_dir is the engine's working dir.
    let captured = captured_wd
        .lock()
        .await
        .clone()
        .expect("tool was not called");
    assert_eq!(
        captured, working_dir,
        "ToolContext.working_dir must equal the engine's working_dir"
    );
    assert_ne!(captured, data_dir);

    // Bug 2 (indirect): on the second call, the provider received a
    // context containing the assistant tool_use message followed by the
    // tool_result.
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

/// Tool whose output vastly exceeds the engine's cap.
struct BigOutputTool;

#[async_trait]
impl Tool for BigOutputTool {
    fn name(&self) -> &str {
        "big"
    }

    fn description(&self) -> &str {
        "Returns a huge output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } },
            "required": ["n"]
        })
    }

    async fn call(&self, _arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(ToolOutput {
            content: "x".repeat(200_000),
            is_error: false,
        })
    }
}

/// Run a two-turn text-only conversation and return the context captured
/// on the LAST provider call (the mock overwrites captures per call).
async fn run_two_turns_and_capture_last_context(
    mock: Arc<MockProvider>,
    engine: ReActEngine,
) -> Vec<parrot_core::types::ChatMessage> {
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    for q in ["q1", "q2"] {
        cmd_tx
            .send(SessionCmd::Chat {
                message: q.to_string(),
            })
            .await
            .unwrap();
        collect_until_turn_end(&mut event_rx).await;
    }
    drop(cmd_tx);
    drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;
    mock.captured_messages().await
}

fn temp_dirs() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    let working_dir = tmp.path().join("working");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    (tmp, working_dir, data_dir)
}

#[tokio::test]
async fn oversized_tool_output_is_truncated_in_event_and_context() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry.register(Arc::new(BigOutputTool)).await;

    let mock = Arc::new(MockProvider::new().with_first_tool("big", json!({"n": 1})));
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("You are a test assistant.".to_string()),
        data_dir,
        working_dir,
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    cmd_tx
        .send(SessionCmd::Chat {
            message: "run the big tool".to_string(),
        })
        .await
        .unwrap();
    let mut events = collect_until_turn_end(&mut event_rx).await;
    drop(cmd_tx);
    let trailing = drain_until_agent_end(&mut event_rx).await;
    events.extend(trailing);
    let _ = engine_task.await;

    // ToolEnd (event log + client) is capped with a truncation marker.
    let tool_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::ToolEnd { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("ToolEnd present");
    assert!(
        tool_end.content.len() <= parrot_core::tool_output::MAX_TOOL_OUTPUT_BYTES + 64,
        "ToolEnd content must be capped, got {} bytes",
        tool_end.content.len()
    );
    assert!(tool_end.content.contains("(truncated, total 200000 bytes)"));
    assert!(!tool_end.is_error);

    // The context on the second LLM call received the truncated version.
    let captured = mock.captured_messages().await;
    let tool_msg = captured
        .iter()
        .find(|m| m.role == parrot_core::types::ChatRole::Tool)
        .expect("tool message present in second-call context");
    assert!(tool_msg.content.contains("(truncated, total 200000 bytes)"));
    assert!(tool_msg.content.len() <= parrot_core::tool_output::MAX_TOOL_OUTPUT_BYTES + 64);
}

#[tokio::test]
async fn context_limits_from_builder_drive_pruning() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    )
    .with_context_limits(parrot_core::compaction::ContextLimits {
        max_history_tokens: 1,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: false,
            ..Default::default()
        },
    });

    let captured = run_two_turns_and_capture_last_context(mock, engine).await;

    assert!(
        captured.iter().any(|m| m.content == "q2"),
        "latest turn must remain, got: {:?}",
        captured
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        captured.iter().all(|m| m.content != "q1"),
        "old turn must be pruned under a 1-token budget"
    );
}

#[tokio::test]
async fn model_context_window_caps_prune_budget() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only().with_context_window(1));
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    )
    .with_context_limits(parrot_core::compaction::ContextLimits {
        max_history_tokens: u32::MAX,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: false,
            ..Default::default()
        },
    });

    let captured = run_two_turns_and_capture_last_context(mock, engine).await;

    // Config budget is u32::MAX; only the model's 1-token window can force
    // this pruning. If the window were ignored, q1 would still be present.
    assert!(
        captured.iter().all(|m| m.content != "q1"),
        "old turn must be pruned when the model window is the binding limit"
    );
    assert!(captured.iter().any(|m| m.content == "q2"));
}

// ---------------------------------------------------------------------------
// Compaction (structured summary) tests
// ---------------------------------------------------------------------------

fn compaction_engine(
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    limits: parrot_core::compaction::ContextLimits,
    data_dir: std::path::PathBuf,
    working_dir: std::path::PathBuf,
) -> ReActEngine {
    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    ReActEngine::new(
        uuid::Uuid::new_v4(),
        tool_registry,
        provider_registry,
        config,
        Some("You are a test assistant.".to_string()),
        data_dir,
        working_dir,
    )
    .with_context_limits(limits)
}

fn small_budget_limits() -> parrot_core::compaction::ContextLimits {
    // est budget 1000 tokens ⇒ threshold 900; keep 300 tokens ⇒ 1 kept turn.
    parrot_core::compaction::ContextLimits {
        max_history_tokens: 1000,
        keep_recent_turns: 10,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: true,
            threshold: 0.9,
            keep_recent_tokens: 300,
            summary_max_tokens: 1024,
        },
    }
}

/// Drive 3 text-only turns; returns (all events, all provider calls).
async fn run_three_turns(
    mock: Arc<MockProvider>,
    engine: ReActEngine,
) -> (Vec<AgentEvent>, Vec<Vec<parrot_core::types::ChatMessage>>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    let mut events = Vec::new();
    for i in 0..3 {
        // ~1000 est tokens each: over the 900-token threshold by turn 3.
        let q = format!("q{}{}", i, "x".repeat(4000));
        cmd_tx.send(SessionCmd::Chat { message: q }).await.unwrap();
        events.extend(collect_until_turn_end(&mut event_rx).await);
    }
    drop(cmd_tx);
    events.extend(drain_until_agent_end(&mut event_rx).await);
    let _ = engine_task.await;
    let calls = mock.captured_calls().await;
    (events, calls)
}

/// Whether a captured provider call is the compaction summary call:
/// exactly [System(SUMMARIZATION_PROMPT), User(<conversation>)].
fn is_summary_call(call: &[parrot_core::types::ChatMessage]) -> bool {
    call.len() == 2
        && call[0].role == parrot_core::types::ChatRole::System
        && call[0].content.contains("压缩助手")
}

#[tokio::test]
async fn compaction_summarizes_old_turns_and_keeps_recent() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(
        MockProvider::new()
            .text_only()
            .with_summary("## 目标与任务\nfix bug"),
    );
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let engine = compaction_engine(
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        small_budget_limits(),
        data_dir,
        working_dir,
    );

    let (events, calls) = run_three_turns(mock, engine).await;

    // 1) A CompactionSummary event was emitted, BEFORE the 3rd turn's TurnStart.
    let cs_idx = events
        .iter()
        .position(|ev| matches!(ev, AgentEvent::CompactionSummary { .. }))
        .expect("CompactionSummary event expected");
    let third_turn_start = events
        .iter()
        .position(|ev| {
            matches!(
                ev,
                AgentEvent::TurnStart { user_message, .. } if user_message.starts_with("q2")
            )
        })
        .expect("3rd TurnStart");
    assert!(
        cs_idx < third_turn_start,
        "CompactionSummary must precede TurnStart"
    );

    // 2) The summary event content carries the marker and counts.
    if let AgentEvent::CompactionSummary {
        summary,
        dropped_message_count,
        kept_message_count,
        ..
    } = &events[cs_idx]
    {
        assert!(summary.starts_with("[CONVERSATION SUMMARY]"));
        assert!(summary.contains("fix bug"));
        assert!(
            *dropped_message_count >= 2,
            "turn-1 user+assistant summarized"
        );
        assert!(*kept_message_count >= 2, "turn-2 user+assistant kept");
    } else {
        panic!("unreachable");
    }

    // 3) The summary call itself: 2 messages (system prompt + conversation).
    let summary_call = calls
        .iter()
        .find(|c| is_summary_call(c))
        .expect("summary call captured");
    assert!(summary_call[1].content.contains("<conversation>"));

    // 4) The LAST main call (turn 3): contains summary message, no q0 turn,
    //    keeps q1 turn verbatim.
    let last_main = calls.last().expect("main call");
    assert!(
        last_main
            .iter()
            .any(|m| m.content.starts_with("[CONVERSATION SUMMARY]")),
        "context must contain the summary message"
    );
    assert!(
        !last_main.iter().any(|m| m.content.starts_with("q0")),
        "summarized turn must be gone"
    );
    assert!(
        last_main.iter().any(|m| m.content.starts_with("q1")),
        "kept turn must remain verbatim"
    );
    assert_eq!(
        last_main.first().map(|m| m.role.clone()),
        Some(parrot_core::types::ChatRole::System),
        "system stays on top"
    );
}

#[tokio::test]
async fn compaction_skipped_under_threshold() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    // Huge budget ⇒ never over threshold.
    let limits = parrot_core::compaction::ContextLimits {
        max_history_tokens: 1_000_000,
        ..small_budget_limits()
    };
    let engine = compaction_engine(
        tool_registry,
        provider_registry,
        limits,
        data_dir,
        working_dir,
    );

    let (events, calls) = run_three_turns(mock, engine).await;
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. })),
        "no compaction under threshold"
    );
    assert!(
        calls.iter().all(|c| !is_summary_call(c)),
        "no summary-style call captured"
    );
}

#[tokio::test]
async fn compaction_failure_falls_back_to_prune() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only().fail_summary());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    // Budget 1 token ⇒ always over threshold. keep_recent_turns=2 keeps two
    // complete turns alive through prune, so turn 3's maybe_compact fires a
    // summary call (which fails) before prune drops the oldest turn.
    let limits = parrot_core::compaction::ContextLimits {
        max_history_tokens: 1,
        keep_recent_turns: 2,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: true,
            ..Default::default()
        },
    };
    let engine = compaction_engine(
        tool_registry,
        provider_registry,
        limits,
        data_dir,
        working_dir,
    );

    let (events, calls) = run_three_turns(mock, engine).await;

    // No CompactionSummary event (fail-open), turns still complete.
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev, AgentEvent::TurnEnd { .. }))
            .count(),
        3,
        "all turns must complete despite summary failure"
    );
    // Prune fallback: the last main call must not contain q0 (pruned).
    let last_main = calls.last().expect("main call");
    assert!(
        !last_main.iter().any(|m| m.content.starts_with("q0")),
        "q0 must be pruned by the fallback"
    );
    assert!(
        calls.iter().any(|c| is_summary_call(c)),
        "the summary call must have been attempted (and failed) for this test to exercise fail-open"
    );
}

#[tokio::test]
async fn compaction_disabled_matches_old_behavior() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let mut limits = small_budget_limits();
    limits.compaction.enabled = false;
    let engine = compaction_engine(
        tool_registry,
        provider_registry,
        limits,
        data_dir,
        working_dir,
    );

    let (events, _calls) = run_three_turns(mock, engine).await;
    assert!(!events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. })));
}

/// Regression test for the `with_end_reason` builder + shared-Arc plumbing
/// (daemon graceful-shutdown prep). The engine's `None` branch overwrites
/// `end_reason` to `ClientDisconnect`; when the Arc is shared via
/// `with_end_reason`, that overwrite must be visible to an external holder,
/// and the same value must flow to `AgentEnd`. Pins the invariant that the
/// guard reads from the threaded Arc, not a private local.
#[tokio::test]
async fn with_end_reason_shares_arc_with_engine_guard() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };

    // Init to a non-default value so the engine's None-branch overwrite is
    // detectable through this shared Arc. `ClientClose` (not `DaemonShutdown`)
    // is used because the None branch preserves a pre-set `DaemonShutdown`
    // (see `none_branch_preserves_daemon_shutdown`); any other variant still
    // gets overwritten to `ClientDisconnect`.
    let external_reason = Arc::new(std::sync::Mutex::new(AgentEndReason::ClientClose));

    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    )
    .with_end_reason(Arc::clone(&external_reason));

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Dropping cmd_tx makes the engine's recv() return None, firing the
    // None-branch overwrite to ClientDisconnect then AgentEnd.
    drop(cmd_tx);
    let trailing = drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;

    // The external holder observes the overwrite ⇒ the Arc is shared.
    let observed = external_reason.lock().unwrap().clone();
    assert_eq!(
        observed,
        AgentEndReason::ClientDisconnect,
        "engine None-branch overwrite must be visible via the shared end_reason Arc"
    );

    // The guard emitted the same value from the shared Arc.
    let agent_end_reason = trailing
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::AgentEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("AgentEnd present");
    assert_eq!(agent_end_reason, AgentEndReason::ClientDisconnect);
}

/// Critical fix for daemon graceful-shutdown (Task 2): when `end_reason`
/// is pre-set to `DaemonShutdown` via the shared Arc (as
/// `SessionManager::shutdown_all` does before dropping `cmd_tx`), the
/// `None` branch must NOT clobber it back to `ClientDisconnect`. The
/// emitted `AgentEnd` must carry `DaemonShutdown`.
#[tokio::test]
async fn none_branch_preserves_daemon_shutdown() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };

    // Pre-set to DaemonShutdown, as `shutdown_all` would do before dropping
    // the cmd_tx sender (which makes recv() return None).
    let external_reason = Arc::new(std::sync::Mutex::new(AgentEndReason::DaemonShutdown));

    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    )
    .with_end_reason(Arc::clone(&external_reason));

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Dropping cmd_tx makes recv() return None, firing the None branch.
    drop(cmd_tx);
    let trailing = drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;

    // The None branch must NOT overwrite a pre-set DaemonShutdown.
    let observed = external_reason.lock().unwrap().clone();
    assert_eq!(
        observed,
        AgentEndReason::DaemonShutdown,
        "None branch must preserve a pre-set DaemonShutdown, not clobber to ClientDisconnect"
    );

    // And the emitted AgentEnd must carry DaemonShutdown.
    let agent_end_reason = trailing
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::AgentEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("AgentEnd present");
    assert_eq!(
        agent_end_reason,
        AgentEndReason::DaemonShutdown,
        "AgentEnd must carry DaemonShutdown when shutdown_all pre-set it"
    );
}

/// 永远挂起的工具：模拟执行中的长任务，供"工具执行中 Abort"测试使用。
struct HangingTool {
    started: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for HangingTool {
    fn name(&self) -> &str {
        "hang"
    }

    fn description(&self) -> &str {
        "Blocks forever once started."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        self.started.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("future 被取消，永不返回")
    }
}

/// 回归：工具执行中收到 Abort 时，事件流、磁盘日志与上下文都必须保持
/// 成对，且会话在 abort 后可继续对话：
///   1. ToolStart ↔ ToolEnd 成对，ToolEnd 补发 "aborted before execution"；
///   2. TurnStart ↔ TurnEnd{Aborted} 成对——events.log 是完整 turn 序列，
///      resume 时无需截断（不产生完整性问题）；
///   3. 上下文里 assistant(tool_calls) 后紧跟配对的 tool_result，
///      下一轮 LLM 调用不会因孤儿 tool_use 被拒。
#[tokio::test]
async fn abort_during_tool_execution_keeps_pairs_intact() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let started = Arc::new(AtomicBool::new(false));
    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(HangingTool {
            started: Arc::clone(&started),
        }))
        .await;

    let mock = Arc::new(MockProvider::new().with_first_tool("hang", json!({})));
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("You are a test assistant.".to_string()),
        data_dir.clone(),
        working_dir,
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    let mut events = Vec::new();

    // Turn 1：等 ToolStart（确认工具已进入执行）再发 Abort。
    cmd_tx
        .send(SessionCmd::Chat {
            message: "run the hanging tool".to_string(),
        })
        .await
        .unwrap();
    loop {
        let ev = event_rx.recv().await.expect("工具挂起期间引擎必须存活");
        let is_tool_start = matches!(&ev, AgentEvent::ToolStart { .. });
        events.push(ev);
        if is_tool_start {
            break;
        }
    }
    cmd_tx.send(SessionCmd::Abort).await.unwrap();
    events.extend(collect_until_turn_end(&mut event_rx).await);

    assert!(
        started.load(Ordering::SeqCst),
        "工具必须真的开始执行过，否则本测试没测到目标路径"
    );

    // 1) ToolStart ↔ ToolEnd 成对，且 ToolEnd 是 aborted 输出。
    let tool_start_ids: Vec<String> = events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::ToolStart { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let tool_end_ids: Vec<String> = events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::ToolEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_start_ids,
        vec!["tc_mock_1".to_string()],
        "恰好一个 ToolStart"
    );
    assert_eq!(
        tool_end_ids,
        vec!["tc_mock_1".to_string()],
        "abort 必须补发配对的 ToolEnd"
    );
    let aborted_end = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::ToolEnd { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("ToolEnd present");
    assert_eq!(aborted_end.content, "aborted before execution");
    assert!(aborted_end.is_error);

    // 2) TurnEnd{Aborted} 闭合 turn。
    let stop = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("TurnEnd present");
    assert_eq!(stop, TurnStopReason::Aborted);

    // Turn 2：abort 后会话继续可用，下一轮正常 EndTurn。
    cmd_tx
        .send(SessionCmd::Chat {
            message: "continue".to_string(),
        })
        .await
        .unwrap();
    let second = collect_until_turn_end(&mut event_rx).await;
    let second_stop = second
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("second TurnEnd present");
    assert_eq!(second_stop, TurnStopReason::EndTurn);

    drop(cmd_tx);
    let trailing = drain_until_agent_end(&mut event_rx).await;
    events.extend(trailing.clone());
    let _ = engine_task.await;
    assert_eq!(
        trailing.last().map(variant_name),
        Some("AgentEnd"),
        "AgentEnd 收尾"
    );

    // 3) 上下文配对：最后一次 provider 调用（turn 2 首个主调用）里，
    //    assistant(tool_calls 含 tc_mock_1) 的下一条必须是配对 tool_result。
    let last_call = mock.captured_messages().await;
    let mut pair_found = false;
    for i in 0..last_call.len().saturating_sub(1) {
        if let Some(tcs) = &last_call[i].tool_calls {
            if tcs.iter().any(|tc| tc.id == "tc_mock_1")
                && last_call[i + 1].role == parrot_core::types::ChatRole::Tool
                && last_call[i + 1].tool_call_id.as_deref() == Some("tc_mock_1")
            {
                pair_found = true;
                assert_eq!(
                    last_call[i + 1].content,
                    "aborted before execution",
                    "上下文里的 tool_result 必须是 abort 补发的那条"
                );
                break;
            }
        }
    }
    assert!(
        pair_found,
        "被中断的 tool_use 必须在上下文中有配对 tool_result: {:?}",
        last_call
            .iter()
            .map(|m| format!(
                "{:?}: {}",
                m.role,
                m.content.chars().take(40).collect::<String>()
            ))
            .collect::<Vec<_>>()
    );

    // 4) events.log 同样成对，且整条日志都是完整 turn（resume 无需截断）。
    let log = parrot_core::event_log::EventLog::new(data_dir);
    let entries = log.replay().unwrap();
    let log_starts: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.event {
            AgentEvent::ToolStart { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let log_ends: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.event {
            AgentEvent::ToolEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(log_starts, vec!["tc_mock_1"]);
    assert_eq!(log_ends, vec!["tc_mock_1"]);

    let total = entries.len();
    let (keep, dropped, issue) = parrot_core::event_log::truncate_to_last_complete_turn(entries);
    assert!(
        issue.is_none(),
        "abort 后日志必须是完整 turn 序列，不得触发截断: {issue:?}"
    );
    assert!(dropped.is_empty());
    assert_eq!(keep.len(), total, "不得丢弃任何事件");
}

// ---------------------------------------------------------------------------
// SessionCmd::SetModel (runtime model switch) tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_model_between_turns_switches_provider_model() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: Some(0.5),
        max_tokens: Some(4096),
        stop_sequences: Some(vec!["STOP".to_string()]),
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Turn 1 with the original model.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "q1".to_string(),
        })
        .await
        .unwrap();
    collect_until_turn_end(&mut event_rx).await;

    // Switch model at the turn boundary (idle between turns).
    cmd_tx
        .send(SessionCmd::SetModel {
            model: "new-model".to_string(),
        })
        .await
        .unwrap();

    // Turn 2 must reach the provider with the NEW model id.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "q2".to_string(),
        })
        .await
        .unwrap();
    collect_until_turn_end(&mut event_rx).await;

    drop(cmd_tx);
    drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;

    let models = mock.captured_models().await;
    assert_eq!(
        models,
        vec!["mock-model".to_string(), "new-model".to_string()],
        "turn 2 must be served with the switched model"
    );

    // Other GenerateConfig fields must survive the switch untouched.
    let configs = mock.captured_configs().await;
    assert_eq!(configs.len(), 2);
    assert_eq!(configs[1].max_tokens, Some(4096), "max_tokens preserved");
    assert_eq!(configs[1].temperature, Some(0.5), "temperature preserved");
    assert_eq!(
        configs[1].stop_sequences,
        Some(vec!["STOP".to_string()]),
        "stop_sequences preserved"
    );
}

/// Provider whose stream holds the `Finish` behind a gate, so the test can
/// deliver a command while the engine is inside the mid-stream select loop.
struct GatedProvider {
    captured_models: tokio::sync::Mutex<Vec<String>>,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl LlmProvider for GatedProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<parrot_core::types::ModelInfo>, parrot_core::error::ProviderError> {
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
        model: &str,
        _messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, parrot_core::error::ProviderError> {
        self.captured_models.lock().await.push(model.to_string());
        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);
        let gate = Arc::clone(&self.gate);
        tokio::spawn(async move {
            tx.send(ProviderStreamEvent::TextDelta {
                delta: "partial".to_string(),
            })
            .await
            .ok();
            gate.notified().await;
            tx.send(ProviderStreamEvent::Finish {
                stop_reason: ProviderStopReason::EndTurn,
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
        _messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<parrot_core::types::ChatMessage, parrot_core::error::ProviderError> {
        Ok(parrot_core::types::ChatMessage {
            role: parrot_core::types::ChatRole::Assistant,
            content: "ok".to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        })
    }
}

/// Mid-turn `SetModel` must be warned about and ignored: the current call
/// keeps the old model, and the command is NOT deferred — the next turn
/// also uses the old model.
#[tokio::test]
async fn set_model_mid_turn_is_ignored() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let mock = Arc::new(GatedProvider {
        captured_models: tokio::sync::Mutex::new(Vec::new()),
        gate: Arc::clone(&gate),
    });
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    );

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Start the turn; the first MessageDelta proves the engine is inside
    // the mid-stream select loop.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "q1".to_string(),
        })
        .await
        .unwrap();
    loop {
        let ev = event_rx.recv().await.expect("engine alive mid-stream");
        if matches!(ev, AgentEvent::MessageDelta { .. }) {
            break;
        }
    }

    // Deliver SetModel mid-stream, then release the gated Finish.
    cmd_tx
        .send(SessionCmd::SetModel {
            model: "new-model".to_string(),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    gate.notify_one();

    let events = collect_until_turn_end(&mut event_rx).await;
    let stop = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("first TurnEnd present");
    assert_eq!(stop, TurnStopReason::EndTurn, "turn completes normally");

    // The command was ignored, not deferred: turn 2 still uses the old model.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "q2".to_string(),
        })
        .await
        .unwrap();
    gate.notify_one();
    collect_until_turn_end(&mut event_rx).await;

    drop(cmd_tx);
    drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;

    let models = mock.captured_models.lock().await.clone();
    assert_eq!(
        models,
        vec!["mock-model".to_string(), "mock-model".to_string()],
        "mid-turn SetModel must not affect the current call nor the next turn"
    );
}

/// Switching model at a turn boundary must also switch the model's context
/// size config: the prune budget is recomputed from the new model's window.
/// `max_history_tokens` is huge here, so only the model window can bind.
#[tokio::test]
async fn set_model_recomputes_compaction_budget() {
    let (_tmp, working_dir, data_dir) = temp_dirs();

    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(
        MockProvider::new()
            .text_only()
            .with_model_window("large-model", 1_000_000)
            .with_model_window("small-model", 1),
    );
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            provider,
            vec!["large-model".to_string(), "small-model".to_string()],
        )
        .await;

    let config = GenerateConfig {
        model: "large-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    let engine = ReActEngine::new(
        uuid::Uuid::new_v4(),
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        config,
        Some("sys".to_string()),
        data_dir,
        working_dir,
    )
    .with_context_limits(parrot_core::compaction::ContextLimits {
        max_history_tokens: u32::MAX,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: false,
            ..Default::default()
        },
    });

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    // Two turns under the large window: nothing is pruned.
    for q in ["q1", "q2"] {
        cmd_tx
            .send(SessionCmd::Chat {
                message: q.to_string(),
            })
            .await
            .unwrap();
        collect_until_turn_end(&mut event_rx).await;
    }
    let before_switch = mock.captured_calls().await;
    assert!(
        before_switch[1].iter().any(|m| m.content == "q1"),
        "before the switch the 1M-token window must keep the old turn"
    );

    // Switch to the 1-token-window model at the turn boundary.
    cmd_tx
        .send(SessionCmd::SetModel {
            model: "small-model".to_string(),
        })
        .await
        .unwrap();

    // Along the same input as before, the small window must now prune
    // everything but the newest turn.
    cmd_tx
        .send(SessionCmd::Chat {
            message: "q3".to_string(),
        })
        .await
        .unwrap();
    collect_until_turn_end(&mut event_rx).await;

    drop(cmd_tx);
    drain_until_agent_end(&mut event_rx).await;
    let _ = engine_task.await;

    let calls = mock.captured_calls().await;
    let after_switch = calls.last().expect("turn 3 provider call");
    assert!(
        after_switch.iter().any(|m| m.content == "q3"),
        "the new turn must remain"
    );
    assert!(
        !after_switch.iter().any(|m| m.content.starts_with("q1")),
        "after the switch to a 1-token window q1 must be pruned, got: {:?}",
        after_switch
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        !after_switch.iter().any(|m| m.content.starts_with("q2")),
        "after the switch to a 1-token window q2 must be pruned"
    );
}
