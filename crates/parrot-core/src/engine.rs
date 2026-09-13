use crate::compaction::{
    apply_summary, build_summary_request, plan_compaction, wrap_summary, ContextLimits,
};
use crate::context::ContextManager;
use crate::error::{AgentError, ProviderError};
use crate::event_log::EventLog;
use crate::hooks::{HookEvent, HookExecution, HookRegistry, HookResult};
use crate::provider::{ProviderRegistry, ProviderStreamEvent};
use crate::session::{ConfirmConfig, SessionCmd};
use crate::tool::{SharedFilesRead, ToolContext, ToolRegistry};
use crate::tool_output::truncate_tool_content;
use crate::types::{
    ChatMessage, ChatRole, GenerateConfig, ModelInfo, ToolCallInfo as CoreToolCallInfo,
};
use parrot_protocol::agent_event::{
    AgentEndReason, AgentEvent, IntegrityIssue, MessageDeltaPayload, MessageStopReason,
    ToolCallInfo, TurnStopReason,
};
use parrot_protocol::types::{ConfirmDecision, Usage};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// 实际生效的上下文裁剪预算：`[session] max_history_tokens` 配置值，
/// 当 provider 上报模型 context window 时再取二者较小值。配置是主旋钮，
/// 模型窗口只能收紧。
fn context_budget(max_history_tokens: u32, model_window: Option<u32>) -> u32 {
    max_history_tokens.min(model_window.unwrap_or(u32::MAX))
}

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
    /// SHA-256(system_prompt) 前 16 字节的 hex。无 prompt 时为空串。
    system_prompt_hash: String,
    /// 引擎由 resume 创建、回放到 seq `n` 时为 `Some(n)`。
    /// 随 `AgentStart` 发给客户端以便观察。
    resumed_from_seq: Option<u64>,
    /// resume 检测到损坏时由 `with_pending_integrity_warning` 设置。
    /// 在 `AgentStart` 之后发一次 `ReplayIntegrityWarning` 即清空。
    pending_integrity_warning: Option<IntegrityIssue>,
    /// resume 回放结束后 EventLog 所在的 seq。resume 后追加新事件时
    /// 用来保持 `current_seq` 连续。
    resume_seq_offset: u64,
    /// 上下文限额（`[session]` + 压缩，由 daemon 接线）。
    context_limits: ContextLimits,
    hooks: Arc<HookRegistry>,
    /// 与 `SessionManager`（经 `with_end_reason`）共享，未来的
    /// `shutdown_all` 可在触发干净退出路径前把原因改成 `DaemonShutdown`。
    /// 默认 `ClientDisconnect`；`run()` 的 `None` 分支在通道正常关闭时重置。
    end_reason: Arc<Mutex<AgentEndReason>>,
    /// 本会话已成功 file_read 过的文件集合（`ToolContext.files_read` 的
    /// 数据源）。resume 创建新 engine → 集合为空，模型必须重读。
    files_read: SharedFilesRead,
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
        let system_prompt_hash = system_prompt
            .as_deref()
            .map(system_prompt_hash)
            .unwrap_or_default();
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
            context_limits: ContextLimits::default(),
            hooks: Arc::new(HookRegistry::empty()),
            end_reason: Arc::new(Mutex::new(AgentEndReason::ClientDisconnect)),
            files_read: Arc::new(Mutex::new(HashSet::new())),
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

    /// 标记引擎来自 resume：`resumed_from_seq` 是回放结束后 EventLog 的
    /// seq（下一个追加的事件编号即此值）。
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

    /// 覆盖上下文限额。daemon 把 parrot.toml 的 `[session]` 值传进来。
    pub fn with_context_limits(mut self, limits: ContextLimits) -> Self {
        self.context_limits = limits;
        self
    }

    /// 把引擎的 `end_reason` 槽位与调用方（`SessionManager`）共享。
    /// `AgentEndGuard` 发 `AgentEnd` 时读的是同一个 `Arc`，因此外部
    /// （如设置 `DaemonShutdown`）在干净退出路径之前的修改能决定发出的
    /// 原因。未设置时默认 `ClientDisconnect`。
    pub fn with_end_reason(mut self, reason: Arc<Mutex<AgentEndReason>>) -> Self {
        self.end_reason = reason;
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
            context.insert(0, ChatMessage::new(ChatRole::System, prompt.clone()));
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
        emit_and_persist(&event_tx, &mut event_log, agent_start).await;

        let (_result, execs) = self
            .hooks
            .run(
                HookEvent::AgentStart {
                    session_id,
                    model: &self.config.model,
                    provider: &provider_id,
                },
                &self.working_dir,
            )
            .await;
        emit_hook_executions(&execs, session_id, &event_tx);

        // 若 resume 时检测到事件日志损坏，发一次 ReplayIntegrityWarning 即清空，
        // 落盘后下次 resume 不会再重复报。
        if let Some(issue) = self.pending_integrity_warning.take() {
            let warning = AgentEvent::ReplayIntegrityWarning {
                session_id,
                issue: issue.clone(),
            };
            emit_and_persist(&event_tx, &mut event_log, warning).await;
            tracing::warn!(session_id = %session_id, kind = ?issue.kind, "replay integrity warning emitted");
        }

        // 预算 = min([session] max_history_tokens, 模型 context_window)。
        // 配置值是主旋钮，模型窗口只能进一步收紧。
        let model_window = self.resolve_model_context_window().await;
        let mut budget = context_budget(self.context_limits.max_history_tokens, model_window);
        let mut context_manager =
            ContextManager::new(budget, self.context_limits.keep_recent_turns);

        // RAII 守卫：确保 AgentEnd 即使 panic 也发。临界区都是 clone 取值后
        // 立刻丢锁，不在 await 间持锁，所以用 std::sync::Mutex 是安全的。
        let total_usage: Arc<Mutex<Usage>> = Arc::new(Mutex::new(Usage::default()));
        let event_tx_for_guard = event_tx.clone();
        let guard = AgentEndGuard {
            session_id,
            event_tx: event_tx_for_guard,
            fired: false,
            total_usage: Arc::clone(&total_usage),
            reason: Arc::clone(&self.end_reason),
            hooks: Arc::clone(&self.hooks),
            working_dir: self.working_dir.clone(),
        };

        loop {
            match cmd_rx.recv().await {
                Some(SessionCmd::Chat { message }) => {
                    let turn_id = Uuid::new_v4();
                    let (turn_start_outcome, execs) = self
                        .hooks
                        .run(
                            HookEvent::TurnStart {
                                session_id,
                                turn_id,
                                user_message: &message,
                            },
                            &self.working_dir,
                        )
                        .await;
                    emit_hook_executions(&execs, session_id, &event_tx);
                    match turn_start_outcome {
                        HookResult::Block { reason, .. } => {
                            let turn_end = AgentEvent::TurnEnd {
                                session_id,
                                turn_id,
                                stop_reason: TurnStopReason::BlockedHook(reason.clone()),
                                usage: Usage::default(),
                            };
                            emit_and_persist(&event_tx, &mut event_log, turn_end).await;
                            continue;
                        }
                        HookResult::Inject { messages } => context.extend(messages),
                        _ => {}
                    }
                    // 压缩检查在 TurnStart 事件之前(spec §3.1):避免
                    // CompactionSummary 落入半成品 turn 被 resume 截断丢弃。
                    self.maybe_compact(&mut context, turn_id, budget, &event_tx, &mut event_log)
                        .await;
                    let turn_start = AgentEvent::TurnStart {
                        session_id,
                        turn_id,
                        user_message: message.clone(),
                    };
                    emit_and_persist(&event_tx, &mut event_log, turn_start).await;

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
                    emit_and_persist(&event_tx, &mut event_log, turn_end).await;
                }
                Some(SessionCmd::Abort) => {
                    // Top-level abort with no active turn — ignore.
                }
                Some(SessionCmd::SetModel { model }) => {
                    tracing::info!(
                        session_id = %session_id,
                        old_model = %self.config.model,
                        new_model = %model,
                        "switching model at turn boundary"
                    );
                    self.config.model = model;
                    // 模型与其上下文大小配置一体：预算按新模型 window 同步重算。
                    let model_window = self.resolve_model_context_window().await;
                    budget = context_budget(self.context_limits.max_history_tokens, model_window);
                    context_manager =
                        ContextManager::new(budget, self.context_limits.keep_recent_turns);
                }
                None => {
                    let mut reason = self.end_reason.lock().unwrap();
                    if !matches!(*reason, AgentEndReason::DaemonShutdown) {
                        *reason = AgentEndReason::ClientDisconnect;
                    }
                    drop(reason);
                    break;
                }
            }
        }

        guard.fire_and_drop(&mut event_log).await;
    }

    async fn resolve_provider_id(&self) -> String {
        self.provider_registry
            .resolve(&self.config.model)
            .await
            .map(|p| p.provider_id().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// 模型的 context window（由其 provider 上报），未知时为 None。
    /// `context_window == 0` 表示远端/配置均未知，视为未收录，
    /// 保持配置预算不变（spec §3.5）。
    async fn resolve_model_context_window(&self) -> Option<u32> {
        let provider = self.provider_registry.resolve(&self.config.model).await?;
        let models = provider.list_models().await.ok()?;
        model_context_window(&models, &self.config.model)
    }

    /// Pi 式上下文压缩(spec §3.1):超阈值时把切点前历史交给当前模型
    /// 生成结构化摘要,近期消息原样保留。失败 fail-open,prune 兜底。
    async fn maybe_compact(
        &self,
        context: &mut Vec<ChatMessage>,
        turn_id: Uuid,
        budget: u32,
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
    ) {
        let session_id = self.session_id;
        let cfg = &self.context_limits.compaction;
        let Some(plan) = plan_compaction(context, budget, cfg) else {
            return;
        };

        let region: Vec<ChatMessage> = context[plan.summarize_start..plan.cut_index].to_vec();
        let request = build_summary_request(&region);

        let Some(provider) = self.provider_registry.resolve(&self.config.model).await else {
            tracing::warn!(session_id = %session_id, "compaction skipped: provider not found");
            return;
        };
        let _ = event_tx
            .send(AgentEvent::CompactionStart {
                session_id,
                turn_id,
            })
            .await
            .ok();
        let mut summary_config = self.config.clone();
        summary_config.max_tokens = Some(cfg.summary_max_tokens);
        let summary = match provider
            .chat(&self.config.model, &request, &[], &summary_config)
            .await
        {
            Ok(m) => m.content,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "compaction summary call failed; falling back to prune"
                );
                return;
            }
        };
        if summary.trim().is_empty() {
            tracing::warn!(session_id = %session_id, "compaction summary empty; falling back to prune");
            return;
        }

        let (dropped, kept_count) = apply_summary(context, &plan, &summary);

        let event = AgentEvent::CompactionSummary {
            session_id,
            turn_id,
            summary: wrap_summary(&summary),
            dropped_message_count: dropped,
            kept_message_count: kept_count,
        };
        emit_and_persist(event_tx, event_log, event).await;
    }

    /// ReAct 主循环：跑一轮 LLM，把 assistant 消息塞进 context；
    /// 若有工具调用就依次跑，然后下一轮；直到 EndTurn / 无工具调用。
    /// 任何子步骤抛 `Aborted` 都原样往外抛，
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
        context.push(ChatMessage::new(ChatRole::User, user_msg));

        let mut turn_usage = Usage::default();
        let tool_defs = self.tool_registry.list_definitions().await;

        {
            context_manager.prune(context);
            let session_id = self.session_id;
            let (outcome, execs) = self
                .hooks
                .run(
                    HookEvent::ContextReady {
                        session_id,
                        turn_id,
                        context,
                    },
                    &self.working_dir,
                )
                .await;
            emit_hook_executions(&execs, session_id, event_tx);
            match outcome {
                HookResult::Block { reason, .. } => {
                    return Ok((TurnStopReason::BlockedHook(reason), Usage::default()));
                }
                HookResult::ReplaceContext { messages, .. } => {
                    *context = messages;
                    context_manager.prune(context);
                }
                HookResult::Inject { messages } => {
                    context.extend(messages);
                    context_manager.prune(context);
                }
                HookResult::Continue | HookResult::Replace { .. } => {}
            }
        }

        loop {
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
                tool_calls: (!core_tcs.is_empty()).then_some(core_tcs),
                ..ChatMessage::new(ChatRole::Assistant, msg.accumulated_text.clone())
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
        emit_and_persist(event_tx, event_log, message_start).await;

        let mut accumulated_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut msg_stop = MessageStopReason::EndTurn;
        let mut msg_usage = Usage::default();
        let mut aborted = false;
        let mut finished = false;

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
                    SessionCmd::SetModel { .. } => {
                        tracing::warn!(
                            "SetModel command received while a turn is in flight; ignoring"
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
                            finished = true;
                            break;
                        }
                    }
                }
            }
        }

        if aborted {
            return Err(AgentError::Aborted);
        }

        if !finished {
            return Err(AgentError::Provider(ProviderError::StreamError(
                "stream ended without Finish".into(),
            )));
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
        emit_and_persist(event_tx, event_log, message_end).await;

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
        emit_and_persist(event_tx, event_log, tool_start).await;

        let (tool_call_outcome, execs) = self
            .hooks
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
            )
            .await;
        emit_hook_executions(&execs, session_id, event_tx);
        if let HookResult::Block { reason, .. } = tool_call_outcome {
            let result = parrot_protocol::types::ToolOutput {
                content: truncate_tool_content(&format!("blocked: {reason}")),
                is_error: true,
            };
            finish_tool_call(
                session_id, turn_id, &tc.id, &tc.name, result, event_tx, event_log, context,
            )
            .await;
            return Ok(());
        }

        let tool_ctx = ToolContext::new(self.working_dir.clone(), 10 * 1024 * 1024)
            .with_files_read(Arc::clone(&self.files_read));

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
            let (_, execs) = self
                .hooks
                .run(
                    HookEvent::ToolExecutionStart {
                        session_id,
                        turn_id,
                        tool_call_id: &tc.id,
                        tool_name: &tc.name,
                        arguments: &args,
                    },
                    &self.working_dir,
                )
                .await;
            emit_hook_executions(&execs, session_id, event_tx);
            match race_with_abort(
                cmd_rx,
                self.tool_registry
                    .execute(&tc.name, args.clone(), &tool_ctx),
            )
            .await
            {
                Abortable::Completed(r) => {
                    r.unwrap_or_else(|e| parrot_protocol::types::ToolOutput {
                        content: truncate_tool_content(&format!("Error: {}", e)),
                        is_error: true,
                    })
                }
                Abortable::Aborted => {
                    finish_tool_call(
                        session_id,
                        turn_id,
                        &tc.id,
                        &tc.name,
                        parrot_protocol::types::ToolOutput {
                            content: "aborted before execution".to_string(),
                            is_error: true,
                        },
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

        let (tool_result_decision, execs) = self
            .hooks
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
            )
            .await;
        emit_hook_executions(&execs, session_id, event_tx);
        // 用户 hook Replace 可注入无界内容,截断必须在它之后收口
        // (TruncateHook 在管道外再兜一次)。
        let result = match tool_result_decision {
            HookResult::Replace {
                content, is_error, ..
            } => parrot_protocol::types::ToolOutput {
                content: truncate_tool_content(&content),
                is_error,
            },
            _ => result,
        };

        finish_tool_call(
            session_id, turn_id, &tc.id, &tc.name, result, event_tx, event_log, context,
        )
        .await;

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
                finish_tool_call(
                    session_id,
                    turn_id,
                    tool_id,
                    tool_name,
                    parrot_protocol::types::ToolOutput {
                        content: "aborted before execution".to_string(),
                        is_error: true,
                    },
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
/// - 操作中途来了条 Chat 或 SetModel → warn 后照常 await 操作完成，**之后
///   不再受理 Abort**（沿用重构前的语义，避免一条噪音命令改变取消语义）。
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
            SessionCmd::SetModel { .. } => {
                tracing::warn!("SetModel command received during operation; ignoring");
                Abortable::Completed(fut.as_mut().await)
            }
        },
        r = &mut fut => Abortable::Completed(r),
    }
}

/// 把 `HookRegistry::run` 产生的 per-hook 遥测记录
/// （`Vec<HookExecution>`）转成 `AgentEvent::HookFired` 发到线上。
/// 非阻塞：`try_send` 在通道满时静默丢弃（缓冲 64；丢一条
/// `HookFired{noop}` 属可接受的观测性损耗）。
fn emit_hook_executions(
    executions: &[HookExecution],
    session_id: Uuid,
    event_tx: &mpsc::Sender<AgentEvent>,
) {
    for ex in executions {
        let _ = event_tx.try_send(AgentEvent::HookFired {
            session_id,
            hook_id: ex.hook_id.clone(),
            event_kind: ex.event_kind.clone(),
            result_kind: ex.result_kind.clone(),
            summary: ex.summary.clone(),
        });
    }
}

/// 发送事件到线上并落盘。先发 clone 再 append 原件；落盘失败只 warn
/// （磁盘故障不应中断在线流）。
async fn emit_and_persist(
    event_tx: &mpsc::Sender<AgentEvent>,
    event_log: &mut EventLog,
    event: AgentEvent,
) {
    let _ = event_tx.send(event.clone()).await.ok();
    if let Err(e) = event_log.append(event) {
        tracing::warn!(error = ?e, "event persistence failed");
    }
}

/// 发 ToolEnd 并把 Tool 消息塞进 context。hook 拦截、正常完成、
/// 执行前中断三条路径共用这份收尾。
#[allow(clippy::too_many_arguments)]
async fn finish_tool_call(
    session_id: Uuid,
    turn_id: Uuid,
    tool_call_id: &str,
    tool_name: &str,
    result: parrot_protocol::types::ToolOutput,
    event_tx: &mpsc::Sender<AgentEvent>,
    event_log: &mut EventLog,
    context: &mut Vec<ChatMessage>,
) {
    let tool_end = AgentEvent::ToolEnd {
        session_id,
        turn_id,
        tool_call_id: tool_call_id.to_string(),
        result: result.clone(),
    };
    emit_and_persist(event_tx, event_log, tool_end).await;
    context.push(ChatMessage {
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        ..ChatMessage::new(ChatRole::Tool, result.content)
    });
}

/// RAII 守卫：drop 时发 `AgentEnd`（除非已触发）。保证 panic 或任务
/// abort 时 `AgentEnd` 也一定发出。
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
    async fn fire_and_drop(mut self, event_log: &mut EventLog) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        let (_, execs) = self
            .hooks
            .run(
                HookEvent::AgentEnd {
                    session_id: self.session_id,
                },
                &self.working_dir,
            )
            .await;
        emit_hook_executions(&execs, self.session_id, &self.event_tx);
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

/// 计算 system prompt 的 SHA-256（前 16 字节 hex）。
fn system_prompt_hash(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let full = hasher.finalize();
    hex::encode(&full[..16])
}

fn model_context_window(models: &[ModelInfo], model: &str) -> Option<u32> {
    models
        .iter()
        .find(|m| m.id == model && m.context_window > 0)
        .map(|m| m.context_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_budget_takes_min_of_config_and_model_window() {
        assert_eq!(context_budget(100_000, None), 100_000);
        assert_eq!(context_budget(100_000, Some(200_000)), 100_000);
        assert_eq!(context_budget(2_000_000, Some(200_000)), 200_000);
        assert_eq!(context_budget(50, Some(200_000)), 50);
    }
}

#[cfg(test)]
mod context_window_tests {
    use super::*;

    fn model(id: &str, cw: u32) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            provider: "p".into(),
            context_window: cw,
            max_output_tokens: 0,
        }
    }

    #[test]
    fn known_window_returned() {
        let models = vec![model("m1", 200_000)];
        assert_eq!(model_context_window(&models, "m1"), Some(200_000));
    }

    #[test]
    fn zero_window_treated_as_unknown() {
        let models = vec![model("m1", 0)];
        assert_eq!(model_context_window(&models, "m1"), None);
    }

    #[test]
    fn unknown_model_returns_none() {
        let models = vec![model("m1", 200_000)];
        assert_eq!(model_context_window(&models, "m2"), None);
    }
}
