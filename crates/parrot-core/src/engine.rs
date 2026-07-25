use crate::context::ContextManager;
use crate::error::AgentError;
use crate::event_log::EventLog;
use crate::hooks::{HookEvent, HookRegistry, HookResult};
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
use std::future::Future;
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
    hooks: Arc<HookRegistry>,
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
            hooks: Arc::new(HookRegistry::empty()),
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

    pub fn with_hooks(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hooks = registry;
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
        // resume 后续 seq 要从 replay 恢复点继续，保持与磁盘日志连续。
        if self.resume_seq_offset > 0 {
            event_log = event_log.with_start_seq(self.resume_seq_offset);
        }

        let provider_id = self.resolve_provider_id().await;
        let agent_start = AgentEvent::AgentStart {
            session_id,
            model: self.config.model.clone(),
            provider: provider_id.clone(),
            system_prompt_hash: self.system_prompt_hash.clone(),
            resumed_from_seq: self.resumed_from_seq,
        };
        let _ = event_tx.send(agent_start.clone()).await;
        if let Err(e) = event_log.append(agent_start) {
            tracing::warn!(error = ?e, "failed to persist AgentStart");
        }

        let mut emit = make_emit(session_id, &event_tx);
        self.hooks
            .run(
                HookEvent::AgentStart { session_id, model: &self.config.model, provider: &provider_id },
                &self.working_dir,
                &mut emit,
            )
            .await;

        // 若 resume 时检测到事件日志损坏，发一次 ReplayIntegrityWarning 即清空，
        // 落盘后下次 resume 不会再重复报。
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

        // RAII 守卫：确保 AgentEnd 即使 panic 也发。临界区都是 clone 取值后
        // 立刻丢锁，不在 await 间持锁，所以用 std::sync::Mutex 是安全的。
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
            hooks: Arc::clone(&self.hooks),
            working_dir: self.working_dir.clone(),
        };

        loop {
            match cmd_rx.recv().await {
                Some(SessionCmd::Chat { message }) => {
                    let turn_id = Uuid::new_v4();
                    let turn_start_outcome = self
                        .hooks
                        .run(
                            HookEvent::TurnStart { session_id, turn_id, user_message: &message },
                            &self.working_dir,
                            &mut emit,
                        )
                        .await;
                    match turn_start_outcome {
                        HookResult::Block { reason, .. } => {
                            let turn_end = AgentEvent::TurnEnd {
                                session_id,
                                turn_id,
                                stop_reason: TurnStopReason::BlockedHook(reason.clone()),
                                usage: Usage::default(),
                            };
                            let _ = event_tx.send(turn_end.clone()).await.ok();
                            let _ = event_log.append(turn_end);
                            continue;
                        }
                        HookResult::Inject { messages } => context.extend(messages),
                        _ => {}
                    }
                    let _ = event_tx
                        .send(AgentEvent::TurnStart {
                            session_id,
                            turn_id,
                            user_message: message.clone(),
                        })
                        .await
                        .ok();
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
                        Err(e) => {
                            tracing::error!(session_id = %session_id, error = %e, "turn failed");
                            (TurnStopReason::Error(e.to_string()), Usage::default())
                        }
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

        guard.fire_and_drop(&event_tx, &mut event_log).await;
    }

    async fn resolve_provider_id(&self) -> String {
        self.provider_registry
            .resolve(&self.config.model)
            .await
            .map(|p| p.provider_id().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// ReAct 主循环：跑一轮 LLM，把 assistant 消息塞进 context；
    /// 若有工具调用就依次跑，然后下一轮；直到 EndTurn / 无工具调用 /
    /// 达到 MAX_REACT_ITERATIONS。任何子步骤抛 `Aborted` 都原样往外抛，
    /// 由 `run` 转成 `TurnEnd{Aborted}`。
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
        context.push(ChatMessage {
            role: ChatRole::User,
            content: user_msg.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let mut turn_usage = Usage::default();
        let tool_defs = self.tool_registry.list_definitions().await;

        for _ in 0..MAX_REACT_ITERATIONS {
            context_manager.prune(context);

            let msg = self
                .stream_llm_message(turn_id, context, &tool_defs, event_tx, event_log, cmd_rx)
                .await?;

            turn_usage.input_tokens += msg.msg_usage.input_tokens;
            turn_usage.output_tokens += msg.msg_usage.output_tokens;

            let core_tcs: Vec<CoreToolCallInfo> = msg
                .tool_calls
                .iter()
                .map(|tc| CoreToolCallInfo {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments_json.clone().unwrap_or(serde_json::Value::Null),
                })
                .collect();
            context.push(ChatMessage {
                role: ChatRole::Assistant,
                content: msg.accumulated_text.clone(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: if core_tcs.is_empty() {
                    None
                } else {
                    Some(core_tcs)
                },
            });

            if msg.msg_stop == MessageStopReason::EndTurn || msg.tool_calls.is_empty() {
                let _ = event_log.maybe_snapshot(context);
                return Ok((TurnStopReason::EndTurn, turn_usage));
            }

            for tc in &msg.tool_calls {
                self.run_one_tool(
                    turn_id,
                    msg.message_id,
                    tc,
                    context,
                    event_tx,
                    event_log,
                    cmd_rx,
                )
                .await?;
            }

            let _ = event_log.maybe_snapshot(context);
        }

        Ok((TurnStopReason::MaxIterations, turn_usage))
    }

    /// 流式跑一轮 LLM 调用：发 MessageStart，把 provider 的 delta 转发出去，
    /// 到 Finish 后发 MessageEnd 并返回累积内容 + 工具调用。流式中途
    /// Abort 直接返回 `Err(Aborted)`，**不发** MessageEnd（客户端用
    /// TurnEnd{Aborted} 当边界）。
    #[allow(clippy::too_many_arguments)]
    async fn stream_llm_message(
        &self,
        turn_id: Uuid,
        context: &[ChatMessage],
        tool_defs: &[crate::tool::ToolDefinition],
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
        cmd_rx: &mut mpsc::Receiver<SessionCmd>,
    ) -> Result<StreamedMessage, AgentError> {
        let session_id = self.session_id;

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
            .chat_stream(&self.config.model, context, tool_defs, &self.config)
            .await
            .map_err(AgentError::Provider)?;

        let message_id = Uuid::new_v4();
        let message_start = AgentEvent::MessageStart {
            session_id,
            turn_id,
            message_id,
        };
        let _ = event_tx.send(message_start.clone()).await.ok();
        let _ = event_log.append(message_start);

        let mut accumulated_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut msg_stop = MessageStopReason::EndTurn;
        let mut msg_usage = Usage::default();
        let mut aborted = false;

        loop {
            tokio::select! {
                biased;
                Some(cmd) = cmd_rx.recv() => match cmd {
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
                },
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
            return Err(AgentError::Aborted);
        }

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
            tool_calls: tool_calls_info,
            stop_reason: msg_stop.clone(),
            usage: msg_usage.clone(),
        };
        let _ = event_tx.send(message_end.clone()).await.ok();
        let _ = event_log.append(message_end);

        Ok(StreamedMessage {
            message_id,
            accumulated_text,
            tool_calls,
            msg_stop,
            msg_usage,
        })
    }

    /// 跑一个工具：发 ToolStart → 可能等用户 confirm → 跑工具（Abort 可打断）
    /// → 发 ToolEnd + 把 Tool 消息塞进 context。confirm 等待或工具执行中
    /// 收到 Abort，统一走 `emit_aborted_tool_end` 后返回 `Err(Aborted)`。
    #[allow(clippy::too_many_arguments)]
    async fn run_one_tool(
        &self,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tc: &PendingToolCall,
        context: &mut Vec<ChatMessage>,
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
        cmd_rx: &mut mpsc::Receiver<SessionCmd>,
    ) -> Result<(), AgentError> {
        let session_id = self.session_id;
        let args = tc.arguments_json.clone().unwrap_or(serde_json::Value::Null);

        let tool_start = AgentEvent::ToolStart {
            session_id,
            turn_id,
            parent_message_id,
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            arguments: args.clone(),
        };
        let _ = event_tx.send(tool_start.clone()).await.ok();
        let _ = event_log.append(tool_start);

        let tool_call_outcome = {
            let mut emit = make_emit(session_id, event_tx);
            self.hooks
                .run(
                    HookEvent::ToolCall {
                        session_id,
                        turn_id,
                        parent_message_id,
                        tool_call_id: &tc.id,
                        tool_name: &tc.name,
                        arguments: &args,
                    },
                    &self.working_dir,
                    &mut emit,
                )
                .await
        };
        if let HookResult::Block { reason, .. } = tool_call_outcome {
            let result = parrot_protocol::types::ToolOutput {
                content: format!("blocked: {reason}"),
                is_error: true,
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
            return Ok(());
        }

        let tool_ctx = ToolContext {
            working_dir: self.working_dir.clone(),
            max_file_size_bytes: 10 * 1024 * 1024,
        };

        let needs_confirm = self
            .confirm_config
            .require_confirmation
            .iter()
            .any(|pat| tc.name.starts_with(pat.as_str()))
            && self.confirm_config.router.is_some();

        let decision = if needs_confirm {
            self.await_confirmation(
                turn_id, &tc.id, &tc.name, &args, context, event_tx, event_log, cmd_rx,
            )
            .await?
        } else {
            ConfirmDecision::Approve
        };

        let result = if matches!(decision, ConfirmDecision::Approve) {
            {
                let mut emit = make_emit(session_id, event_tx);
                self.hooks
                    .run(
                        HookEvent::ToolExecutionStart {
                            session_id,
                            turn_id,
                            tool_call_id: &tc.id,
                            tool_name: &tc.name,
                            arguments: &args,
                        },
                        &self.working_dir,
                        &mut emit,
                    )
                    .await;
            }
            match race_with_abort(cmd_rx, self.execute_tool(&tc.name, args.clone(), &tool_ctx))
                .await
            {
                Abortable::Completed(r) => {
                    r.unwrap_or_else(|e| parrot_protocol::types::ToolOutput {
                        content: format!("Error: {}", e),
                        is_error: true,
                    })
                }
                Abortable::Aborted => {
                    emit_aborted_tool_end(
                        session_id,
                        turn_id,
                        tc.id.clone(),
                        tc.name.clone(),
                        event_tx,
                        event_log,
                        context,
                    )
                    .await;
                    return Err(AgentError::Aborted);
                }
            }
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

        let tool_result_decision = {
            let mut emit = make_emit(session_id, event_tx);
            self.hooks
                .run(
                    HookEvent::ToolResult {
                        session_id,
                        turn_id,
                        tool_call_id: &tc.id,
                        tool_name: &tc.name,
                        input: &args,
                        result: &result,
                    },
                    &self.working_dir,
                    &mut emit,
                )
                .await
        };
        let result = match tool_result_decision {
            HookResult::Replace { content, is_error, .. } => {
                parrot_protocol::types::ToolOutput { content, is_error }
            }
            _ => result,
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

        Ok(())
    }

    /// 等用户对工具调用给出 ConfirmDecision：注册 one-shot、发
    /// ToolConfirmRequired，在 timeout 内和 Abort 赛跑。Abort 时不等了，
    /// 撤注册后调用方（run_one_tool）发 aborted ToolEnd。
    #[allow(clippy::too_many_arguments)]
    async fn await_confirmation(
        &self,
        turn_id: Uuid,
        tool_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        context: &mut Vec<ChatMessage>,
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
        cmd_rx: &mut mpsc::Receiver<SessionCmd>,
    ) -> Result<ConfirmDecision, AgentError> {
        let session_id = self.session_id;
        let router = self.confirm_config.router.as_ref().unwrap().clone();
        let (ctx_tx, ctx_rx) = oneshot::channel::<ConfirmDecision>();

        router
            .register(self.session_id, tool_id.to_string(), ctx_tx)
            .await;

        let _ = event_tx
            .send(AgentEvent::ToolConfirmRequired {
                session_id,
                turn_id,
                tool_call_id: tool_id.to_string(),
                tool_name: tool_name.to_string(),
                arguments: args.clone(),
            })
            .await
            .ok();

        let confirm_fut = async {
            match tokio::time::timeout(self.confirm_config.timeout, ctx_rx).await {
                Ok(Ok(d)) => d,
                Ok(Err(_)) => ConfirmDecision::Timeout,
                Err(_) => ConfirmDecision::Timeout,
            }
        };

        let decision = match race_with_abort(cmd_rx, confirm_fut).await {
            Abortable::Completed(d) => d,
            Abortable::Aborted => {
                router.unregister(&self.session_id, tool_id).await;
                emit_aborted_tool_end(
                    session_id,
                    turn_id,
                    tool_id.to_string(),
                    tool_name.to_string(),
                    event_tx,
                    event_log,
                    context,
                )
                .await;
                return Err(AgentError::Aborted);
            }
        };

        router.unregister(&self.session_id, tool_id).await;
        Ok(decision)
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

/// 一轮流式 LLM 调用的结果，handle_turn 拿它 push assistant 消息进
/// context，并根据是否含工具调用决定要不要继续下一轮 ReAct。
struct StreamedMessage {
    message_id: Uuid,
    accumulated_text: String,
    tool_calls: Vec<PendingToolCall>,
    msg_stop: MessageStopReason,
    msg_usage: Usage,
}

/// 把 future 和 `cmd_rx` 上的 Abort 命令赛跑。三种结果：
/// - Abort 先到 → `Aborted`（调用方自己清理）。
/// - 操作中途来了条 Chat → warn 后照常 await 操作完成，**之后不再受理
///   Abort**（沿用重构前的语义，避免一条噪音 Chat 改变取消语义）。
/// - 操作先完成 → `Completed(value)`。
enum Abortable<T> {
    Completed(T),
    Aborted,
}

async fn race_with_abort<F, R>(cmd_rx: &mut mpsc::Receiver<SessionCmd>, future: F) -> Abortable<R>
where
    F: Future<Output = R>,
{
    let mut fut = Box::pin(future);
    tokio::select! {
        biased;
        Some(cmd) = cmd_rx.recv() => match cmd {
            SessionCmd::Abort => Abortable::Aborted,
            SessionCmd::Chat { .. } => {
                tracing::warn!("Chat command received during operation; ignoring");
                Abortable::Completed(fut.as_mut().await)
            }
        },
        r = &mut fut => Abortable::Completed(r),
    }
}

/// Build a non-async emit closure backed by `tokio::mpsc::Sender::try_send`.
/// Drives fire-and-forget + noop + inject_messages + replace_result +
/// timeout + error `HookFired` emissions from inside the registry dispatch
/// helpers. The returned closure owns a cloned `Sender` and a copied
/// `session_id` (`'static`), so it does NOT borrow `self` and is safe to use
/// alongside other `&self` method calls. If the channel is full the event is
/// dropped — the buffer is 64 and losing a `HookFired{noop}` is acceptable
/// observability telemetry.
fn make_emit(
    session_id: Uuid,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> impl FnMut(&str, &str, &str, Option<String>) + Send {
    let tx = event_tx.clone();
    move |hook_id: &str, event_kind: &str, result_kind: &str, summary: Option<String>| {
        let _ = tx.try_send(AgentEvent::HookFired {
            session_id,
            hook_id: hook_id.to_string(),
            event_kind: event_kind.to_string(),
            result_kind: result_kind.to_string(),
            summary,
        });
    }
}

/// 发"aborted before execution" ToolEnd 并把 Tool 消息塞进 context。
/// confirm 等待与工具执行两条中断路径共用这份清理。
#[allow(clippy::too_many_arguments)]
async fn emit_aborted_tool_end(
    session_id: Uuid,
    turn_id: Uuid,
    tool_call_id: String,
    tool_name: String,
    event_tx: &mpsc::Sender<AgentEvent>,
    event_log: &mut EventLog,
    context: &mut Vec<ChatMessage>,
) {
    let result = parrot_protocol::types::ToolOutput {
        content: "aborted before execution".to_string(),
        is_error: true,
    };
    let tool_end = AgentEvent::ToolEnd {
        session_id,
        turn_id,
        tool_call_id: tool_call_id.clone(),
        result: result.clone(),
    };
    let _ = event_tx.send(tool_end.clone()).await.ok();
    let _ = event_log.append(tool_end);
    context.push(ChatMessage {
        role: ChatRole::Tool,
        content: result.content,
        tool_call_id: Some(tool_call_id),
        tool_name: Some(tool_name),
        tool_calls: None,
    });
}

/// RAII guard that emits `AgentEnd` on drop unless already fired. Ensures
/// `AgentEnd` is always sent even on panic or task abort.
struct AgentEndGuard {
    session_id: Uuid,
    event_tx: mpsc::Sender<AgentEvent>,
    fired: bool,
    total_usage: Arc<Mutex<Usage>>,
    reason: Arc<Mutex<AgentEndReason>>,
    hooks: Arc<HookRegistry>,
    working_dir: std::path::PathBuf,
}

impl AgentEndGuard {
    /// 显式触发 AgentEnd（正常退出路径调用，避免 Drop 不可控）。
    /// 先落盘再发送：resume 时靠读 events.log 区分"会话已结束"和"daemon 崩溃"。
    async fn fire_and_drop(mut self, _tx: &mpsc::Sender<AgentEvent>, event_log: &mut EventLog) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        let mut emit = make_emit(self.session_id, &self.event_tx);
        self.hooks
            .run(
                HookEvent::AgentEnd { session_id: self.session_id },
                &self.working_dir,
                &mut emit,
            )
            .await;
        let event = AgentEvent::AgentEnd {
            session_id: self.session_id,
            reason,
            total_usage: usage,
        };
        if let Err(e) = event_log.append(event.clone()) {
            tracing::warn!(error = ?e, "failed to persist AgentEnd");
        }
        let _ = self.event_tx.send(event).await.ok();
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
