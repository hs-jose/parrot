use crate::context::ContextManager;
use crate::error::AgentError;
use crate::event_log::{EventLog, EventLogEntry, StreamEvent};
use crate::provider::ProviderRegistry;
use crate::session::{ConfirmConfig, SessionCmd};
use crate::tool::{ToolContext, ToolOutput, ToolRegistry};
use crate::types::{ChatMessage, ChatRole, GenerateConfig};
use parrot_protocol::types::{ConfirmDecision, StopReason, Usage};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub const MAX_REACT_ITERATIONS: u32 = 20;

pub struct ReActEngine {
    session_id: Uuid,
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    config: GenerateConfig,
    system_prompt: Option<String>,
    data_dir: std::path::PathBuf,
    working_dir: std::path::PathBuf,
    confirm_config: ConfirmConfig,
    /// Pre-built context for resumed sessions. Empty for fresh sessions
    /// (`create_session`); populated by `create_session_with_context`
    /// (`ResumeSession`). The system prompt is NOT included here — it's
    /// injected at the head from `system_prompt` so the engine stays the
    /// single owner of that decision.
    initial_context: Vec<ChatMessage>,
}

impl ReActEngine {
    pub fn new(
        session_id: Uuid,
        tool_registry: Arc<ToolRegistry>,
        provider_registry: Arc<ProviderRegistry>,
        config: GenerateConfig,
        system_prompt: Option<String>,
        data_dir: std::path::PathBuf,
        working_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            session_id,
            tool_registry,
            provider_registry,
            config,
            system_prompt,
            data_dir,
            working_dir,
            confirm_config: ConfirmConfig::default(),
            initial_context: Vec::new(),
        }
    }

    /// Phase 1.5: attach confirmation config. Builder-style so the existing
    /// `ReActEngine::new(...)` call sites in tests don't all need updating.
    pub fn with_confirm_config(mut self, config: ConfirmConfig) -> Self {
        self.confirm_config = config;
        self
    }

    /// Phase 1.5: attach a pre-built context (used by `ResumeSession`).
    pub fn with_initial_context(mut self, context: Vec<ChatMessage>) -> Self {
        self.initial_context = context;
        self
    }

    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<SessionCmd>,
        event_tx: mpsc::Sender<StreamEvent>,
    ) {
        // Start from the pre-built context (empty for fresh sessions; the
        // replayed event-log contents for resumed sessions). The system
        // prompt is injected at the head below. We use `mem::take` to move
        // `initial_context` out without partially moving `self`, so the
        // later `self.handle_chat(...)` call can borrow `&self` cleanly.
        let mut context: Vec<ChatMessage> = std::mem::take(&mut self.initial_context);

        // Inject the system prompt at the head of the context. The context
        // pruner (§4.6) never removes System messages — it removes complete
        // turns starting at the oldest User message — so this survives the
        // whole session. If `system_prompt` is None, daemon is expected to
        // have already supplied a default before constructing the engine
        // (engines are responsible only for emitting whatever prompt they're
        // handed, not for choosing one).
        if let Some(prompt) = &self.system_prompt {
            context.insert(
                0,
                ChatMessage {
                    role: ChatRole::System,
                    content: prompt.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                },
            );
        }

        let mut event_log = EventLog::new(self.data_dir.clone());
        let context_manager = ContextManager::new(100_000, 10);

        // Use `loop { match cmd_rx.recv().await }` rather than `while let` so
        // we can hand `&mut cmd_rx` to `handle_chat` inside the body without
        // fighting the desugared `while let` borrow.
        loop {
            match cmd_rx.recv().await {
                Some(SessionCmd::Chat { message }) => {
                    let ctx_len = context.len();
                    let result = self
                        .handle_chat(
                            &message,
                            &mut context,
                            &context_manager,
                            &event_tx,
                            &mut event_log,
                            &mut cmd_rx,
                        )
                        .await;
                    if let Err(e) = result {
                        tracing::error!("Chat handling error: {}", e);
                        // Roll back any partial context additions from the aborted turn.
                        context.truncate(ctx_len);
                        let _ = event_tx
                            .send(StreamEvent::TextDelta {
                                delta: format!("\nError: {}\n", e),
                            })
                            .await;
                        let _ = event_tx
                            .send(StreamEvent::Finish {
                                stop_reason: StopReason::Aborted,
                                usage: Usage {
                                    input_tokens: 0,
                                    output_tokens: 0,
                                },
                            })
                            .await;
                    }
                }
                Some(SessionCmd::Abort) => {
                    // Top-level abort with no active chat in flight.
                    let _ = event_tx
                        .send(StreamEvent::Finish {
                            stop_reason: StopReason::Aborted,
                            usage: Usage {
                                input_tokens: 0,
                                output_tokens: 0,
                            },
                        })
                        .await;
                }
                None => break, // cmd sender dropped — session is closing
            }
        }
    }

    async fn handle_chat(
        &self,
        message: &str,
        context: &mut Vec<ChatMessage>,
        context_manager: &ContextManager,
        event_tx: &mpsc::Sender<StreamEvent>,
        event_log: &mut EventLog,
        cmd_rx: &mut mpsc::Receiver<SessionCmd>,
    ) -> Result<(), AgentError> {
        // 1. Add user message to context
        context.push(ChatMessage {
            role: ChatRole::User,
            content: message.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let _ = event_log.append(EventLogEntry::UserMessage {
            content: message.to_string(),
        });

        // 2. Get tool definitions
        let tool_defs = self.tool_registry.list_definitions().await;

        // 3. ReAct loop
        for _iteration in 0..MAX_REACT_ITERATIONS {
            // Prune context (system prompt at head is preserved by ContextManager).
            context_manager.prune(context);

            // 4. Resolve provider for the configured model.
            let provider = self
                .provider_registry
                .resolve(&self.config.model)
                .await
                .ok_or_else(|| {
                    AgentError::Config(format!(
                        "No provider found for model: {}",
                        self.config.model
                    ))
                })?;

            let mut stream = provider
                .chat_stream(&self.config.model, context, &tool_defs, &self.config)
                .await
                .map_err(AgentError::Provider)?;

            // 5. Collect stream events, concurrently listening for Abort.
            //    `biased` polls the abort branch first so a pending abort is
            //    observed before the next stream event is processed.
            let mut accumulated_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut usage = Usage {
                input_tokens: 0,
                output_tokens: 0,
            };
            let mut aborted = false;

            loop {
                tokio::select! {
                    biased;

                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            SessionCmd::Abort => {
                                tracing::info!("Abort received mid-stream, cancelling ReAct turn");
                                // Drop the stream receiver first — this causes the
                                // spawned SSE-parsing task to exit (its sender drops).
                                drop(stream);
                                aborted = true;
                                break;
                            }
                            SessionCmd::Chat { .. } => {
                                // A new Chat arriving mid-stream is rejected — the
                                // caller must wait for the current turn's Finished
                                // before sending another Chat. Log and continue
                                // processing the in-flight stream.
                                tracing::warn!(
                                    "Chat command received while a turn is in flight; ignoring"
                                );
                            }
                        }
                    }

                    event = stream.inner.recv() => {
                        let Some(event) = event else { break; };
                        match event {
                            StreamEvent::TextDelta { delta } => {
                                accumulated_text.push_str(&delta);
                                let _ = event_tx.send(StreamEvent::TextDelta { delta }).await;
                            }
                            StreamEvent::ToolCallStart { id, name } => {
                                tool_calls.push(PendingToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: String::new(),
                                    arguments_json: None,
                                });
                                let _ = event_tx.send(StreamEvent::ToolCallStart { id, name }).await;
                            }
                            StreamEvent::ToolCallDelta { id, args_delta } => {
                                if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                                    tc.arguments.push_str(&args_delta);
                                }
                                let _ = event_tx
                                    .send(StreamEvent::ToolCallDelta { id, args_delta })
                                    .await;
                            }
                            StreamEvent::ToolCallEnd { id, arguments: _ } => {
                                let parsed_args = if let Some(tc) =
                                    tool_calls.iter_mut().find(|tc| tc.id == id)
                                {
                                    let parsed = serde_json::from_str(&tc.arguments).unwrap_or(
                                        serde_json::Value::Object(Default::default()),
                                    );
                                    tc.arguments_json = Some(parsed.clone());
                                    parsed
                                } else {
                                    serde_json::Value::Object(Default::default())
                                };
                                let _ = event_tx
                                    .send(StreamEvent::ToolCallEnd { id, arguments: parsed_args })
                                    .await;
                            }
                            StreamEvent::Finish { stop_reason: sr, usage: u } => {
                                stop_reason = sr;
                                usage = u;
                                break;
                            }
                            StreamEvent::ToolResult { .. } => {
                                // ToolResult from provider stream isn't expected here
                                // (provider streams only emit text/tool-call/finish).
                            }
                            StreamEvent::ToolCallConfirmationRequired { .. } => {
                                // Confirmation requests are emitted by the
                                // engine itself during the tool-execution
                                // phase below, never by the provider stream.
                                // A well-behaved provider never sends this.
                                tracing::warn!(
                                    "provider stream emitted ToolCallConfirmationRequired; ignoring"
                                );
                            }
                        }
                    }
                }
            }

            if aborted {
                // Record the abort in the event log so replay reflects reality.
                let _ = event_log.append(EventLogEntry::Finish {
                    stop_reason: StopReason::Aborted,
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                });
                let _ = event_tx
                    .send(StreamEvent::Finish {
                        stop_reason: StopReason::Aborted,
                        usage: Usage {
                            input_tokens: 0,
                            output_tokens: 0,
                        },
                    })
                    .await;
                return Err(AgentError::Aborted);
            }

            // 6. Handle tool calls if stop reason is ToolUse
            if stop_reason == StopReason::ToolUse && !tool_calls.is_empty() {
                // Signal the client that the LLM's turn ended with tool_use so
                // they can render a separator before tool execution output.
                let _ = event_tx
                    .send(StreamEvent::Finish {
                        stop_reason: StopReason::ToolUse,
                        usage: usage.clone(),
                    })
                    .await;

                let tc_info: Vec<crate::types::ToolCallInfo> = tool_calls
                    .iter()
                    .map(|tc| crate::types::ToolCallInfo {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                    })
                    .collect();

                // Add assistant message with text + tool_use blocks.
                context.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: accumulated_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: if tc_info.is_empty() {
                        None
                    } else {
                        Some(tc_info)
                    },
                });

                if !accumulated_text.is_empty() {
                    let _ = event_log.append(EventLogEntry::AssistantText {
                        content: accumulated_text.clone(),
                    });
                }

                // Process each tool call. Abort is honored between and during
                // tool execution via `select!` — dropping the tool future
                // cancels it (tokio::fs and most IO futures are cancel-safe).
                for tc in &tool_calls {
                    let tool_ctx = ToolContext {
                        working_dir: self.working_dir.clone(),
                        max_file_size_bytes: 10 * 1024 * 1024,
                    };

                    let _ = event_log.append(EventLogEntry::ToolCall {
                        tool_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                    });

                    let args = tc.arguments_json.clone().unwrap_or(serde_json::Value::Null);
                    let tool_name_for_err = tc.name.clone();
                    let tool_id = tc.id.clone();

                    // Phase 1.5: if the tool name matches a
                    // `require_confirmation` prefix, ask the client before
                    // executing. The engine emits
                    // `StreamEvent::ToolCallConfirmationRequired`, registers
                    // a oneshot receiver on the `ConfirmRouter`, and waits
                    // up to `confirm_config.timeout` for a decision. If no
                    // router is configured or no patterns match, we skip
                    // straight to execution (the common case).
                    let needs_confirm = self
                        .confirm_config
                        .require_confirmation
                        .iter()
                        .any(|pat| tc.name.starts_with(pat.as_str()))
                        && self.confirm_config.router.is_some();

                    let decision = if needs_confirm {
                        let router = self.confirm_config.router.as_ref().unwrap().clone();
                        let (ctx_tx, ctx_rx) = oneshot::channel::<ConfirmDecision>();

                        router
                            .register(self.session_id, tool_id.clone(), ctx_tx)
                            .await;

                        let _ = event_tx
                            .send(StreamEvent::ToolCallConfirmationRequired {
                                tool_id: tool_id.clone(),
                                tool_name: tc.name.clone(),
                                arguments: args.clone(),
                            })
                            .await;

                        // Await decision with timeout. `select!` also lets
                        // us honor Abort during the confirmation wait. The
                        // confirmation recv (with timeout) is pinned so both
                        // branches can poll the same future without moving
                        // it twice — `tokio::select!` compiles both arms
                        // even though only one runs, so an unpinned future
                        // would be "moved" by the first arm and rejected in
                        // the second.
                        let mut confirm_fut = Box::pin(async {
                            match tokio::time::timeout(self.confirm_config.timeout, ctx_rx).await {
                                Ok(Ok(d)) => d,
                                Ok(Err(_)) => ConfirmDecision::Timeout,
                                Err(_) => ConfirmDecision::Timeout,
                            }
                        });

                        let waited = tokio::select! {
                            biased;

                            Some(cmd) = cmd_rx.recv() => {
                                match cmd {
                                    SessionCmd::Abort => {
                                        tracing::info!(
                                            "Abort received during confirmation wait for '{}'",
                                            tool_name_for_err
                                        );
                                        router.unregister(&self.session_id, &tool_id).await;
                                        let aborted_output = ToolOutput {
                                            content: "aborted before execution".to_string(),
                                            is_error: true,
                                        };
                                        let _ = event_tx
                                            .send(StreamEvent::ToolResult {
                                                id: tool_id.clone(),
                                                result: aborted_output.clone(),
                                            })
                                            .await;
                                        let _ = event_log.append(EventLogEntry::ToolResult {
                                            tool_id: tool_id.clone(),
                                            output: aborted_output,
                                        });
                                        let _ = event_log.append(EventLogEntry::Finish {
                                            stop_reason: StopReason::Aborted,
                                            usage: Usage { input_tokens: 0, output_tokens: 0 },
                                        });
                                        let _ = event_tx
                                            .send(StreamEvent::Finish {
                                                stop_reason: StopReason::Aborted,
                                                usage: Usage { input_tokens: 0, output_tokens: 0 },
                                            })
                                            .await;
                                        return Err(AgentError::Aborted);
                                    }
                                    SessionCmd::Chat { .. } => {
                                        tracing::warn!(
                                            "Chat command received during confirmation wait; ignoring"
                                        );
                                        // Drop the cmd; keep waiting for the
                                        // confirmation via `confirm_fut` below.
                                        confirm_fut.as_mut().await
                                    }
                                }
                            }

                            d = confirm_fut.as_mut() => d,
                        };

                        // Clean up the router entry in case the client
                        // responded but we hit the timeout race, or the
                        // client never responded.
                        router.unregister(&self.session_id, &tool_id).await;

                        waited
                    } else {
                        ConfirmDecision::Approve
                    };

                    // Honor Reject / Timeout without executing the tool.
                    if !matches!(decision, ConfirmDecision::Approve) {
                        let reason = if matches!(decision, ConfirmDecision::Reject) {
                            "user rejected"
                        } else {
                            "confirmation timeout"
                        };
                        let output = ToolOutput {
                            content: reason.to_string(),
                            is_error: true,
                        };
                        let _ = event_tx
                            .send(StreamEvent::ToolResult {
                                id: tc.id.clone(),
                                result: output.clone(),
                            })
                            .await;
                        let _ = event_log.append(EventLogEntry::ToolResult {
                            tool_id: tc.id.clone(),
                            output: output.clone(),
                        });
                        context.push(ChatMessage {
                            role: ChatRole::Tool,
                            content: output.content,
                            tool_call_id: Some(tc.id.clone()),
                            tool_name: Some(tc.name.clone()),
                            tool_calls: None,
                        });
                        continue;
                    }

                    let args_for_tool = args.clone();

                    let result = tokio::select! {
                        biased;

                        Some(cmd) = cmd_rx.recv() => {
                            match cmd {
                                SessionCmd::Abort => {
                                    tracing::info!(
                                        "Abort received during tool execution '{}'; cancelling",
                                        tool_name_for_err
                                    );
                                    // Emit a ToolResult noting the abort, so the
                                    // event log records that this tool was
                                    // scheduled but not run.
                                    let aborted_output = ToolOutput {
                                        content: "aborted before execution".to_string(),
                                        is_error: true,
                                    };
                                    let _ = event_tx
                                        .send(StreamEvent::ToolResult {
                                            id: tool_id.clone(),
                                            result: aborted_output.clone(),
                                        })
                                        .await;
                                    let _ = event_log.append(EventLogEntry::ToolResult {
                                        tool_id: tool_id.clone(),
                                        output: aborted_output,
                                    });
                                    let _ = event_log.append(EventLogEntry::Finish {
                                        stop_reason: StopReason::Aborted,
                                        usage: Usage { input_tokens: 0, output_tokens: 0 },
                                    });
                                    let _ = event_tx
                                        .send(StreamEvent::Finish {
                                            stop_reason: StopReason::Aborted,
                                            usage: Usage { input_tokens: 0, output_tokens: 0 },
                                        })
                                        .await;
                                    return Err(AgentError::Aborted);
                                }
                                SessionCmd::Chat { .. } => {
                                    tracing::warn!(
                                        "Chat command received during tool execution; ignoring"
                                    );
                                    // Fall through to actually run the tool.
                                    self.execute_tool(&tc.name, args.clone(), &tool_ctx).await
                                }
                            }
                        }

                        r = async {
                            self.execute_tool(&tc.name, args_for_tool, &tool_ctx).await
                        } => r,
                    };

                    let output = result.unwrap_or_else(|e| crate::tool::ToolOutput {
                        content: format!("Error: {}", e),
                        is_error: true,
                    });

                    let _ = event_tx
                        .send(StreamEvent::ToolResult {
                            id: tc.id.clone(),
                            result: output.clone(),
                        })
                        .await;

                    let _ = event_log.append(EventLogEntry::ToolResult {
                        tool_id: tc.id.clone(),
                        output: output.clone(),
                    });

                    // Add tool result to context for the next ReAct iteration.
                    context.push(ChatMessage {
                        role: ChatRole::Tool,
                        content: output.content,
                        tool_call_id: Some(tc.id.clone()),
                        tool_name: Some(tc.name.clone()),
                        tool_calls: None,
                    });
                }
                // Continue the outer loop to get the next LLM response.
                continue;
            }

            // 7. End of turn — push accumulated text as assistant message.
            if !accumulated_text.is_empty() {
                context.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: accumulated_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                });
                let _ = event_log.append(EventLogEntry::AssistantText {
                    content: accumulated_text,
                });
            }

            let _ = event_tx
                .send(StreamEvent::Finish {
                    stop_reason: stop_reason.clone(),
                    usage: usage.clone(),
                })
                .await;

            let _ = event_log.append(EventLogEntry::Finish { stop_reason, usage });

            // Checkpoint a snapshot if we've crossed a 100-event boundary
            // (design doc §7). Best-effort — a failed snapshot doesn't fail
            // the turn.
            let _ = event_log.maybe_snapshot(context);

            return Ok(());
        }

        // Max iterations reached — emit a MaxTokens finish and bail.
        let _ = event_tx
            .send(StreamEvent::Finish {
                stop_reason: StopReason::MaxTokens,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            })
            .await;

        Err(AgentError::Config(
            "Max ReAct iterations reached".to_string(),
        ))
    }

    /// Look up a tool by name and call it. Shared between the normal path and
    /// the "Chat received during tool exec" fallback path.
    async fn execute_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<crate::tool::ToolOutput, AgentError> {
        match self.tool_registry.get(name).await {
            Some(tool) => tool.call(args, ctx).await,
            None => Err(AgentError::ToolExecution {
                tool: name.to_string(),
                message: "Tool not found".to_string(),
            }),
        }
    }
}

struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
    arguments_json: Option<serde_json::Value>,
}
