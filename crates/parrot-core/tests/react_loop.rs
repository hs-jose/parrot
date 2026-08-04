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
        _model: &str,
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, parrot_core::error::ProviderError> {
        *self.captured_messages.lock().await = messages.to_vec();
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);

        tokio::spawn(async move {
            match n {
                0 => {
                    tx.send(ProviderStreamEvent::ToolCallStart {
                        id: "tc_mock_1".to_string(),
                        name: "echo".to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallDelta {
                        id: "tc_mock_1".to_string(),
                        args_delta: r#"{"message":"hello"}"#.to_string(),
                    })
                    .await
                    .ok();
                    tx.send(ProviderStreamEvent::ToolCallEnd {
                        id: "tc_mock_1".to_string(),
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
        _messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<parrot_core::types::ChatMessage, parrot_core::error::ProviderError> {
        unimplemented!("chat() is not used in ReAct loop; use chat_stream")
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
