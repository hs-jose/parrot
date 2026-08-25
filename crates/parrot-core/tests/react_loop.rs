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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;

use parrot_core::provider::ProviderStopReason;

struct MockProvider {
    call_count: AtomicU32,
    captured_messages: tokio::sync::Mutex<Vec<parrot_core::types::ChatMessage>>,
    /// Every provider call's messages (chat + chat_stream), in order.
    captured_calls: tokio::sync::Mutex<Vec<Vec<parrot_core::types::ChatMessage>>>,
    /// Scripted first-call tool call; `None` ⇒ every call is plain text.
    first_tool: Option<(String, Value)>,
    /// Reported via `list_models`; caps the engine's context budget.
    context_window: u32,
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
            first_tool: Some(("echo".to_string(), json!({"message": "hello"}))),
            context_window: 200_000,
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
        _model: &str,
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, parrot_core::error::ProviderError> {
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
