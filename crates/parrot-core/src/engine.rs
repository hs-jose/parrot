use crate::context::ContextManager;
use crate::error::AgentError;
use crate::event_log::EventLog;
use crate::provider::{ProviderRegistry, ProviderStreamEvent};
use crate::session::{ConfirmConfig, SessionCmd};
use crate::tool::{ToolContext, ToolRegistry};
use crate::types::{ChatMessage, ChatRole, GenerateConfig, ToolCallInfo as CoreToolCallInfo};
use parrot_protocol::agent_event::{
    AgentEndReason, AgentEvent, IntegrityIssue, MessageDeltaPayload, MessageStopReason,
    ToolCallInfo, TurnStopReason,
};
use parrot_protocol::types::{ConfirmDecision, Usage};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
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
    initial_context: Vec<ChatMessage>,
    /// SHA-256 of system_prompt (first 16 bytes hex). Empty when no prompt.
    system_prompt_hash: String,
    /// `Some(n)` when this engine was created by a resume that replayed up
    /// to seq `n`. Emitted in `AgentStart` for client visibility.
    resumed_from_seq: Option<u64>,
    /// Set by `with_pending_integrity_warning` when a resume detected
    /// corruption. Emitted once as `ReplayIntegrityWarning` after
    /// `AgentStart`, then cleared.
    pending_integrity_warning: Option<IntegrityIssue>,
    /// The seq the EventLog was at after the resume replay. Used to keep
    /// `current_seq` continuous when appending new events post-resume.
    resume_seq_offset: u64,
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
        let system_prompt_hash = match &system_prompt {
            Some(p) => {
                let mut hasher = Sha256::new();
                hasher.update(p.as_bytes());
                let full = hasher.finalize();
                hex::encode(&full[..16])
            }
            None => String::new(),
        };
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
            system_prompt_hash,
            resumed_from_seq: None,
            pending_integrity_warning: None,
            resume_seq_offset: 0,
        }
    }

    pub fn with_confirm_config(mut self, config: ConfirmConfig) -> Self {
        self.confirm_config = config;
        self
    }

    pub fn with_initial_context(mut self, context: Vec<ChatMessage>) -> Self {
        self.initial_context = context;
        self
    }

    /// Mark this engine as resumed: `resumed_from_seq` is the seq the
    /// EventLog was at after replay (the next appended event will be seq
    /// `resumed_from_seq`).
    pub fn with_resumed_from(mut self, seq: u64) -> Self {
        self.resumed_from_seq = Some(seq);
        self.resume_seq_offset = seq;
        self
    }

    pub fn with_pending_integrity_warning(mut self, issue: IntegrityIssue) -> Self {
        self.pending_integrity_warning = Some(issue);
        self
    }

    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<SessionCmd>,
        event_tx: mpsc::Sender<AgentEvent>,
    ) {
        let session_id = self.session_id;

        let mut context: Vec<ChatMessage> = std::mem::take(&mut self.initial_context);

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
        // Resume continuity: if we replayed `n` events, the EventLog's
        // internal seq counter must start at `n` so new appends get seq
        // `n`, `n+1`, ... matching the persisted log.
        if self.resume_seq_offset > 0 {
            event_log = event_log.with_start_seq(self.resume_seq_offset);
        }

        // AgentStart — emitted before any turn.
        let agent_start = AgentEvent::AgentStart {
            session_id,
            model: self.config.model.clone(),
            provider: self.resolve_provider_id().await,
            system_prompt_hash: self.system_prompt_hash.clone(),
            resumed_from_seq: self.resumed_from_seq,
        };
        let _ = event_tx.send(agent_start.clone()).await;
        if let Err(e) = event_log.append(agent_start) {
            tracing::warn!(error = ?e, "failed to persist AgentStart");
        }

        // Replay integrity warning (if any) — emitted once after AgentStart,
        // before the first TurnStart. Persisted so a subsequent resume
        // doesn't re-detect it.
        if let Some(issue) = self.pending_integrity_warning.take() {
            let warning = AgentEvent::ReplayIntegrityWarning {
                session_id,
                issue: issue.clone(),
            };
            let _ = event_tx.send(warning.clone()).await;
            if let Err(e) = event_log.append(warning) {
                tracing::warn!(error = ?e, "failed to persist ReplayIntegrityWarning");
            }
            tracing::warn!(session_id = %session_id, kind = ?issue.kind, "replay integrity warning emitted");
        }

        let context_manager = ContextManager::new(100_000, 10);

        // RAII guard: ensures AgentEnd fires even on panic / task abort.
        let total_usage: Arc<Mutex<Usage>> = Arc::new(Mutex::new(Usage::default()));
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));
        let event_tx_for_guard = event_tx.clone();
        let guard = AgentEndGuard {
            session_id,
            event_tx: event_tx_for_guard,
            fired: false,
            total_usage: Arc::clone(&total_usage),
            reason: Arc::clone(&end_reason),
        };

        loop {
            match cmd_rx.recv().await {
                Some(SessionCmd::Chat { message }) => {
                    let turn_id = Uuid::new_v4();
                    let _ = event_tx
                        .send(AgentEvent::TurnStart {
                            session_id,
                            turn_id,
                            user_message: message.clone(),
                        })
                        .await
                        .ok();
                    // Persist TurnStart (lifecycle events are persistent).
                    let _ = event_log.append(AgentEvent::TurnStart {
                        session_id,
                        turn_id,
                        user_message: message.clone(),
                    });

                    let turn_result = self
                        .handle_turn(
                            turn_id,
                            &message,
                            &mut context,
                            &context_manager,
                            &event_tx,
                            &mut event_log,
                            &mut cmd_rx,
                        )
                        .await;

                    let (stop_reason, turn_usage) = match turn_result {
                        Ok((sr, u)) => (sr, u),
                        Err(AgentError::Aborted) => (TurnStopReason::Aborted, Usage::default()),
                        Err(e) => (TurnStopReason::Error(e.to_string()), Usage::default()),
                    };

                    {
                        let mut total = total_usage.lock().unwrap();
                        total.input_tokens += turn_usage.input_tokens;
                        total.output_tokens += turn_usage.output_tokens;
                    }

                    let turn_end = AgentEvent::TurnEnd {
                        session_id,
                        turn_id,
                        stop_reason: stop_reason.clone(),
                        usage: turn_usage.clone(),
                    };
                    let _ = event_tx.send(turn_end.clone()).await.ok();
                    let _ = event_log.append(turn_end);

                    if matches!(stop_reason, TurnStopReason::Error(_)) {
                        // A fatal turn error ends the session.
                        *end_reason.lock().unwrap() =
                            AgentEndReason::FatalError("turn error".into());
                        break;
                    }
                }
                Some(SessionCmd::Abort) => {
                    // Top-level abort with no active turn — ignore.
                }
                None => {
                    *end_reason.lock().unwrap() = AgentEndReason::ClientDisconnect;
                    break;
                }
            }
        }

        guard.fire_and_drop(&event_tx).await;
    }

    async fn resolve_provider_id(&self) -> String {
        self.provider_registry
            .resolve(&self.config.model)
            .await
            .map(|p| p.provider_id().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_turn(
        &self,
        turn_id: Uuid,
        user_msg: &str,
        context: &mut Vec<ChatMessage>,
        context_manager: &ContextManager,
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
        cmd_rx: &mut mpsc::Receiver<SessionCmd>,
    ) -> Result<(TurnStopReason, Usage), AgentError> {
        let session_id = self.session_id;

        // Push user message to context.
        context.push(ChatMessage {
            role: ChatRole::User,
            content: user_msg.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let mut turn_usage = Usage::default();
        let tool_defs = self.tool_registry.list_definitions().await;

        for iteration in 0..MAX_REACT_ITERATIONS {
            context_manager.prune(context);

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

            let message_id = Uuid::new_v4();
            let _ = event_tx
                .send(AgentEvent::MessageStart {
                    session_id,
                    turn_id,
                    message_id,
                })
                .await
                .ok();

            let mut accumulated_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();
            let mut msg_stop = MessageStopReason::EndTurn;
            let mut msg_usage = Usage::default();
            let mut aborted = false;

            loop {
                tokio::select! {
                    biased;
                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            SessionCmd::Abort => {
                                tracing::info!("Abort received mid-stream, cancelling turn");
                                drop(stream);
                                aborted = true;
                                break;
                            }
                            SessionCmd::Chat { .. } => {
                                tracing::warn!(
                                    "Chat command received while a turn is in flight; ignoring"
                                );
                            }
                        }
                    }
                    event = stream.inner.recv() => {
                        let Some(event) = event else { break };
                        match event {
                            ProviderStreamEvent::TextDelta { delta } => {
                                accumulated_text.push_str(&delta);
                                let _ = event_tx.send(AgentEvent::MessageDelta {
                                    session_id,
                                    message_id,
                                    payload: MessageDeltaPayload::TextDelta { delta },
                                }).await.ok();
                            }
                            ProviderStreamEvent::ToolCallStart { id, name } => {
                                tool_calls.push(PendingToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: String::new(),
                                    arguments_json: None,
                                });
                                let _ = event_tx.send(AgentEvent::MessageDelta {
                                    session_id,
                                    message_id,
                                    payload: MessageDeltaPayload::ToolCallStart {
                                        tool_call_id: id,
                                        tool_name: name,
                                    },
                                }).await.ok();
                            }
                            ProviderStreamEvent::ToolCallDelta { id, args_delta } => {
                                if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                                    tc.arguments.push_str(&args_delta);
                                }
                                let _ = event_tx.send(AgentEvent::MessageDelta {
                                    session_id,
                                    message_id,
                                    payload: MessageDeltaPayload::ToolCallArgsDelta {
                                        tool_call_id: id,
                                        args_delta,
                                    },
                                }).await.ok();
                            }
                            ProviderStreamEvent::ToolCallEnd { id, arguments: _ } => {
                                if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                                    tc.arguments_json = Some(
                                        serde_json::from_str(&tc.arguments)
                                            .unwrap_or(serde_json::Value::Object(Default::default())),
                                    );
                                }
                            }
                            ProviderStreamEvent::Finish { stop_reason, usage } => {
                                msg_stop = stop_reason.into();
                                msg_usage = usage;
                                break;
                            }
                        }
                    }
                }
            }

            if aborted {
                // No MessageEnd on abort — the stream was interrupted, so
                // final_content would be incomplete. The client uses
                // TurnEnd{Aborted} as the boundary.
                return Err(AgentError::Aborted);
            }

            // Emit MessageEnd with final snapshot.
            let tool_calls_info: Vec<ToolCallInfo> = tool_calls
                .iter()
                .map(|tc| ToolCallInfo {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                })
                .collect();

            let message_end = AgentEvent::MessageEnd {
                session_id,
                turn_id,
                message_id,
                final_content: accumulated_text.clone(),
                tool_calls: tool_calls_info.clone(),
                stop_reason: msg_stop.clone(),
                usage: msg_usage.clone(),
            };
            let _ = event_tx.send(message_end.clone()).await.ok();
            let _ = event_log.append(message_end);

            turn_usage.input_tokens += msg_usage.input_tokens;
            turn_usage.output_tokens += msg_usage.output_tokens;

            // Push assistant message to context.
            let core_tcs: Vec<CoreToolCallInfo> = tool_calls
                .iter()
                .map(|tc| CoreToolCallInfo {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                })
                .collect();
            context.push(ChatMessage {
                role: ChatRole::Assistant,
                content: accumulated_text.clone(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: if core_tcs.is_empty() {
                    None
                } else {
                    Some(core_tcs)
                },
            });

            // If no tool calls (or EndTurn), turn ends here.
            if msg_stop == MessageStopReason::EndTurn || tool_calls.is_empty() {
                let _ = event_log.maybe_snapshot(context);
                return Ok((TurnStopReason::EndTurn, turn_usage));
            }

            // Execute each tool: ToolStart → [ToolConfirmRequired] → ToolEnd.
            for tc in &tool_calls {
                let tool_start = AgentEvent::ToolStart {
                    session_id,
                    turn_id,
                    parent_message_id: message_id,
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                };
                let _ = event_tx.send(tool_start.clone()).await.ok();
                let _ = event_log.append(tool_start);

                let tool_ctx = ToolContext {
                    working_dir: self.working_dir.clone(),
                    max_file_size_bytes: 10 * 1024 * 1024,
                };

                let args = tc.arguments_json.clone().unwrap_or(serde_json::Value::Null);
                let tool_name_for_err = tc.name.clone();
                let tool_id = tc.id.clone();

                // Phase 1.5: confirmation flow.
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
                        .send(AgentEvent::ToolConfirmRequired {
                            session_id,
                            turn_id,
                            tool_call_id: tool_id.clone(),
                            tool_name: tc.name.clone(),
                            arguments: args.clone(),
                        })
                        .await
                        .ok();

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
                                    let result = parrot_protocol::types::ToolOutput {
                                        content: "aborted before execution".to_string(),
                                        is_error: true,
                                    };
                                    let tool_end = AgentEvent::ToolEnd {
                                        session_id,
                                        turn_id,
                                        tool_call_id: tool_id.clone(),
                                        result: result.clone(),
                                    };
                                    let _ = event_tx.send(tool_end.clone()).await.ok();
                                    let _ = event_log.append(tool_end);
                                    context.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: result.content,
                                        tool_call_id: Some(tool_id.clone()),
                                        tool_name: Some(tc.name.clone()),
                                        tool_calls: None,
                                    });
                                    return Err(AgentError::Aborted);
                                }
                                SessionCmd::Chat { .. } => {
                                    tracing::warn!(
                                        "Chat command received during confirmation wait; ignoring"
                                    );
                                    confirm_fut.as_mut().await
                                }
                            }
                        }
                        d = confirm_fut.as_mut() => d,
                    };

                    router.unregister(&self.session_id, &tool_id).await;
                    waited
                } else {
                    ConfirmDecision::Approve
                };

                let result = if matches!(decision, ConfirmDecision::Approve) {
                    let args_for_tool = args.clone();
                    let tool_name_for_exec = tc.name.clone();
                    let tool_id_for_exec = tool_id.clone();

                    let result = tokio::select! {
                        biased;
                        Some(cmd) = cmd_rx.recv() => {
                            match cmd {
                                SessionCmd::Abort => {
                                    tracing::info!(
                                        "Abort received during tool execution '{}'; cancelling",
                                        tool_name_for_err
                                    );
                                    let aborted_output = parrot_protocol::types::ToolOutput {
                                        content: "aborted before execution".to_string(),
                                        is_error: true,
                                    };
                                    let tool_end = AgentEvent::ToolEnd {
                                        session_id,
                                        turn_id,
                                        tool_call_id: tool_id_for_exec.clone(),
                                        result: aborted_output.clone(),
                                    };
                                    let _ = event_tx.send(tool_end.clone()).await.ok();
                                    let _ = event_log.append(tool_end);
                                    context.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: aborted_output.content,
                                        tool_call_id: Some(tool_id_for_exec.clone()),
                                        tool_name: Some(tool_name_for_exec.clone()),
                                        tool_calls: None,
                                    });
                                    return Err(AgentError::Aborted);
                                }
                                SessionCmd::Chat { .. } => {
                                    tracing::warn!(
                                        "Chat command received during tool execution; ignoring"
                                    );
                                    self.execute_tool(&tc.name, args.clone(), &tool_ctx).await
                                }
                            }
                        }
                        r = async {
                            self.execute_tool(&tc.name, args_for_tool, &tool_ctx).await
                        } => r,
                    };

                    result.unwrap_or_else(|e| parrot_protocol::types::ToolOutput {
                        content: format!("Error: {}", e),
                        is_error: true,
                    })
                } else {
                    let reason = if matches!(decision, ConfirmDecision::Reject) {
                        "user rejected"
                    } else {
                        "confirmation timeout"
                    };
                    parrot_protocol::types::ToolOutput {
                        content: reason.to_string(),
                        is_error: true,
                    }
                };

                let tool_end = AgentEvent::ToolEnd {
                    session_id,
                    turn_id,
                    tool_call_id: tc.id.clone(),
                    result: result.clone(),
                };
                let _ = event_tx.send(tool_end.clone()).await.ok();
                let _ = event_log.append(tool_end);

                context.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: result.content,
                    tool_call_id: Some(tc.id.clone()),
                    tool_name: Some(tc.name.clone()),
                    tool_calls: None,
                });
            }

            // Continue the ReAct loop for the next LLM call.
            let _ = event_log.maybe_snapshot(context);
            // `iteration` is consumed — loop continues.
            let _ = iteration;
        }

        // Max iterations reached.
        Ok((TurnStopReason::MaxIterations, turn_usage))
    }

    async fn execute_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<parrot_protocol::types::ToolOutput, AgentError> {
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

/// RAII guard that emits `AgentEnd` on drop unless already fired. Ensures
/// `AgentEnd` is always sent even on panic or task abort.
struct AgentEndGuard {
    session_id: Uuid,
    event_tx: mpsc::Sender<AgentEvent>,
    fired: bool,
    total_usage: Arc<Mutex<Usage>>,
    reason: Arc<Mutex<AgentEndReason>>,
}

impl AgentEndGuard {
    /// Fire the `AgentEnd` event explicitly (used for normal shutdown paths
    /// where we want to consume the guard without dropping it silently).
    async fn fire_and_drop(mut self, _tx: &mpsc::Sender<AgentEvent>) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = {
            let r = self.reason.lock().unwrap();
            // If reason is still the default ClientDisconnect but we got
            // here via a normal break, that's fine — the caller set the
            // reason before breaking. Take it as-is.
            r.clone()
        };
        let _ = self
            .event_tx
            .send(AgentEvent::AgentEnd {
                session_id: self.session_id,
                reason,
                total_usage: usage,
            })
            .await
            .ok();
        self.fired = true;
    }
}

impl Drop for AgentEndGuard {
    fn drop(&mut self) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        let _ = self.event_tx.try_send(AgentEvent::AgentEnd {
            session_id: self.session_id,
            reason,
            total_usage: usage,
        });
    }
}

/// Compute the SHA-256 hash (first 16 bytes hex) of a system prompt.
/// Exposed for tests / daemon use.
pub fn system_prompt_hash(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let full = hasher.finalize();
    hex::encode(&full[..16])
}
