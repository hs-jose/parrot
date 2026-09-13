use crate::tui::slash::SlashPopup;
use chrono::Local;
use parrot_protocol::agent_event::{
    AgentEndReason, AgentEvent, MessageDeltaPayload, PersistedAgentEvent, TurnStopReason,
};
use parrot_protocol::types::{ConfirmDecision, ToolOutput, Usage};
use parrot_protocol::{ServerMessage, SessionId};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

/// 客户端本地时间戳，格式 `HH:MM:SS`（精确到秒）。协议事件本身不携带
/// 发生时间，因此在事件到达时打戳；replay 场景下历史消息会统一显示为
/// "当前"时间，这是为了避免给 `apply_event` 签名注入时间参数而做的取舍。
fn stamp() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

#[derive(Debug, Clone)]
pub(crate) enum ChatEntry {
    User {
        text: String,
        time: String,
    },
    Assistant {
        text: String,
        time: String,
    },
    Tool {
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        result: Option<ToolOutput>,
        /// 是否展开显示完整参数与结果（纯 UI 状态，不持久化；replay 默认紧凑）。
        expanded: bool,
    },
    Shell {
        command: String,
        output: Option<String>,
        exit_code: Option<i32>,
    },
    Info(String),
    Error(String),
    Warning(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Normal,
    ConfirmPending,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingConfirmation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

pub(crate) struct App {
    pub session_id: SessionId,
    pub entries: Vec<ChatEntry>,
    pub mode: Mode,
    pub pending_confirmation: Option<PendingConfirmation>,
    pub ended: bool,
    pub scroll_offset: u16,
    /// 对话区可视高度（上次 draw 时记录），用于半页滚动。
    pub view_height: u16,
    cur_assistant_text: HashMap<Uuid, String>,
    cur_assistant_completed: HashMap<Uuid, bool>,
    current_message_id: Option<Uuid>,
    /// 模型名（来自 `AgentStart`），用于状态栏展示。
    pub model: String,
    /// Provider 名（来自 `AgentStart`）。
    pub provider: String,
    /// 累计 token 用量（来自 `TurnEnd` / `AgentEnd` 的权威值）。
    pub total_usage: Usage,
    /// 当前是否处于一轮对话中（`TurnStart` → `TurnEnd`）。
    turn_active: bool,
    /// 已发出 `Chat` 但尚未收到 `TurnStart`（daemon 侧 hook/压缩窗口）。
    /// 与 `turn_active` 共同构成「轮次进行中」判定，避免预 TurnStart 窗口
    /// 切换模型被引擎丢弃、状态栏却显示切换成功。
    pub pending_turn: bool,
    /// 正在执行、尚未 `ToolEnd` 的工具数量。
    tools_in_flight: u32,
    /// 上下文压缩摘要调用进行中（`CompactionStart` → `CompactionSummary`/`TurnStart`）。
    pub compacting: bool,
    /// 当前选中的工具条目（tool_call_id）。Tab 循环选中、Enter 展开。
    pub selected_tool: Option<String>,
    /// 斜杠命令补全弹窗（存活 = Some）。由 sync_slash_popup 按输入文本校正。
    pub slash_popup: Option<SlashPopup>,
    /// Esc 关闭弹窗时的 query；同 query 不复活（防下一帧重新弹出），见设计 §2。
    pub slash_dismissed_query: Option<String>,
}

impl App {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            entries: Vec::new(),
            mode: Mode::Normal,
            pending_confirmation: None,
            ended: false,
            scroll_offset: 0,
            view_height: 0,
            cur_assistant_text: HashMap::new(),
            cur_assistant_completed: HashMap::new(),
            current_message_id: None,
            model: String::new(),
            provider: String::new(),
            total_usage: Usage::default(),
            turn_active: false,
            pending_turn: false,
            tools_in_flight: 0,
            compacting: false,
            selected_tool: None,
            slash_popup: None,
            slash_dismissed_query: None,
        }
    }

    /// `Thinking...`：一轮进行中、还没有流式文本、也没有工具在跑。
    pub fn is_thinking(&self) -> bool {
        self.turn_active
            && self.tools_in_flight == 0
            && self.streaming_text().is_none_or(|s| s.is_empty())
    }

    /// `Working...`：有工具正在执行。
    pub fn is_working(&self) -> bool {
        self.turn_active && self.tools_in_flight > 0
    }

    pub fn is_turn_active(&self) -> bool {
        self.turn_active
    }

    /// 把"距底部滚动偏移"限制在可视滚动范围内。由 `draw_entries` 调用——
    /// 只有它知道真实范围。避免 PgUp 冲过头后要按很多次 PgDn 才能回底部。
    pub fn clamp_scroll(&mut self, max: u16) {
        self.scroll_offset = self.scroll_offset.min(max);
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = u16::MAX;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Tab / Shift+Tab：在工具条目间循环移动选中。
    pub fn select_next_tool(&mut self, forward: bool) {
        let ids: Vec<&str> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                ChatEntry::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            self.selected_tool = None;
            return;
        }
        let next = match self
            .selected_tool
            .as_deref()
            .and_then(|sel| ids.iter().position(|id| *id == sel))
        {
            Some(i) => {
                if forward {
                    (i + 1) % ids.len()
                } else {
                    (i + ids.len() - 1) % ids.len()
                }
            }
            None => {
                if forward {
                    0
                } else {
                    ids.len() - 1
                }
            }
        };
        self.selected_tool = Some(ids[next].to_string());
    }

    /// 展开/收起选中的工具条目。无选中时返回 false（调用方据此让 Enter
    /// 走发送消息的原路径）。
    pub fn toggle_selected_tool(&mut self) -> bool {
        let Some(sel) = self.selected_tool.clone() else {
            return false;
        };
        for e in self.entries.iter_mut() {
            if let ChatEntry::Tool {
                tool_call_id,
                expanded,
                ..
            } = e
            {
                if *tool_call_id == sel {
                    *expanded = !*expanded;
                    return true;
                }
            }
        }
        false
    }

    /// 单击 Esc 清除选中（双击 Esc 的中断/退出语义不变）。
    pub fn clear_tool_selection(&mut self) {
        self.selected_tool = None;
    }

    pub fn confirm_decision(
        &mut self,
        decision: ConfirmDecision,
    ) -> Option<(String, ConfirmDecision)> {
        if let Mode::ConfirmPending = self.mode {
            if let Some(p) = self.pending_confirmation.take() {
                self.mode = Mode::Normal;
                return Some((p.tool_call_id, decision));
            }
        }
        None
    }

    pub fn apply_server_message(&mut self, msg: ServerMessage) -> bool {
        match msg {
            ServerMessage::AgentEvent { event } => {
                self.apply_event(event);
            }
            ServerMessage::Error {
                message,
                session_id,
                ..
            } => {
                // Chat 被 daemon 拒绝时不会有任何 turn 事件，不清 pending_turn
                // 会永久卡住 /model 拦截；错误路径下放行（宁冒提前解锁的窄窗口）。
                // 广播（无 session_id）或匹配本会话的错误才清；其他会话的
                // 错误与本会话的 pending 状态无关。
                if session_id.is_none() || session_id == Some(self.session_id) {
                    self.pending_turn = false;
                }
                self.entries.push(ChatEntry::Error(message));
            }
            ServerMessage::ShellResult {
                output, exit_code, ..
            } => {
                for e in self.entries.iter_mut().rev() {
                    if let ChatEntry::Shell {
                        output: o,
                        exit_code: c,
                        ..
                    } = e
                    {
                        if o.is_none() {
                            *o = Some(output);
                            *c = Some(exit_code);
                            break;
                        }
                    }
                }
            }
            ServerMessage::ModelSet { model, .. } => {
                self.model = model.clone();
                self.entries
                    .push(ChatEntry::Info(format!("模型已切换为 {model}")));
            }
            ServerMessage::McpNotice {
                id,
                state,
                detail,
                tool_count,
            } => {
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                };
                self.entries.push(ChatEntry::Info(format!(
                    "MCP {id}: {state:?}（{tool_count} 个工具）{suffix}"
                )));
            }
            ServerMessage::McpServers { entries } => {
                if entries.is_empty() {
                    self.entries
                        .push(ChatEntry::Info("MCP: 未配置任何 server".into()));
                } else {
                    let lines: Vec<String> = entries
                        .iter()
                        .map(|e| {
                            let suffix = if e.detail.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", e.detail)
                            };
                            format!(
                                "· {} {:?}（{} 个工具）{}",
                                e.id, e.state, e.tool_count, suffix
                            )
                        })
                        .collect();
                    self.entries.push(ChatEntry::Info(lines.join("\n")));
                }
            }
            // TUI 默认不处理其他 ServerMessage（HelloAck 已在握手期完成；SessionList/History 等由列表页消费）
            _ => {}
        }
        self.ended
    }

    pub fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart {
                model, provider, ..
            } => {
                self.model = model;
                self.provider = provider;
                // 新的 AgentStart 意味着会话生命周期重新开始
                // （如 resume 之后）。清掉上一次的 AgentEnd 标记，
                // 让后续 turn 正常处理而不是退出 TUI。
                self.ended = false;
            }
            AgentEvent::AgentEnd {
                reason,
                total_usage,
                ..
            } => {
                if let AgentEndReason::FatalError(msg) = reason {
                    self.entries.push(ChatEntry::Error(msg));
                }
                // AgentEnd 的 total_usage 是整段会话权威合计，覆盖本地累加值。
                self.total_usage = total_usage;
                self.turn_active = false;
                self.pending_turn = false;
                self.tools_in_flight = 0;
                self.discard_incomplete_assistant();
                self.ended = true;
            }
            AgentEvent::TurnStart { user_message, .. } => {
                self.turn_active = true;
                self.pending_turn = false;
                self.tools_in_flight = 0;
                self.compacting = false;
                self.entries.push(ChatEntry::User {
                    text: user_message,
                    time: stamp(),
                });
            }
            AgentEvent::TurnEnd {
                stop_reason, usage, ..
            } => {
                self.flush_open_assistant();
                // 丢弃任何未完成的流式 assistant 状态。daemon 在 Abort 时
                // 会跳过 MessageEnd 只发 TurnEnd{Aborted}；若不清场，残留的
                // current_message_id 会让下一轮 streaming_text() 读到上一轮
                // 被中断的部分文本，表现为“新回答接续上一条”。
                self.discard_incomplete_assistant();
                self.scroll_offset = 0;
                // TurnEnd.usage 是本轮合计；累加进运行总量。
                self.total_usage.input_tokens = self
                    .total_usage
                    .input_tokens
                    .saturating_add(usage.input_tokens);
                self.total_usage.output_tokens = self
                    .total_usage
                    .output_tokens
                    .saturating_add(usage.output_tokens);
                self.turn_active = false;
                self.pending_turn = false;
                self.tools_in_flight = 0;
                if let TurnStopReason::Error(msg) = stop_reason {
                    self.entries.push(ChatEntry::Error(msg));
                }
            }
            AgentEvent::MessageStart { message_id, .. } => {
                self.flush_open_assistant();
                self.current_message_id = Some(message_id);
                self.cur_assistant_text.insert(message_id, String::new());
                self.cur_assistant_completed.insert(message_id, false);
            }
            AgentEvent::MessageDelta {
                message_id,
                payload,
                ..
            } => match payload {
                MessageDeltaPayload::TextDelta { delta } => {
                    if let Some(t) = self.cur_assistant_text.get_mut(&message_id) {
                        t.push_str(&delta);
                    }
                }
                MessageDeltaPayload::ToolCallStart { .. } => {}
                MessageDeltaPayload::ToolCallArgsDelta { .. } => {}
            },
            AgentEvent::MessageEnd {
                message_id,
                final_content,
                ..
            } => {
                self.cur_assistant_text.insert(message_id, final_content);
                self.cur_assistant_completed.insert(message_id, true);
                self.flush_open_assistant();
            }
            AgentEvent::ToolStart {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.tools_in_flight = self.tools_in_flight.saturating_add(1);
                self.entries.push(ChatEntry::Tool {
                    tool_call_id,
                    tool_name,
                    arguments,
                    result: None,
                    expanded: false,
                });
            }
            AgentEvent::ToolUpdate { .. } => {}
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                ..
            } => {
                self.tools_in_flight = self.tools_in_flight.saturating_sub(1);
                for e in self.entries.iter_mut() {
                    if let ChatEntry::Tool {
                        tool_call_id: id,
                        result: r,
                        ..
                    } = e
                    {
                        if id == &tool_call_id {
                            *r = Some(result);
                            break;
                        }
                    }
                }
            }
            AgentEvent::ToolConfirmRequired {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.pending_confirmation = Some(PendingConfirmation {
                    tool_call_id,
                    tool_name,
                    arguments,
                });
                self.mode = Mode::ConfirmPending;
            }
            AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                self.entries.push(ChatEntry::Warning(format!(
                    "{:?} ({} events dropped)",
                    issue.kind, issue.dropped_event_count
                )));
            }
            AgentEvent::HookFired { .. } => {}
            AgentEvent::CompactionStart { .. } => {
                self.compacting = true;
            }
            AgentEvent::CompactionSummary {
                dropped_message_count,
                ..
            } => {
                self.compacting = false;
                self.entries.push(ChatEntry::Warning(format!(
                    "上下文已压缩：{dropped_message_count} 条历史消息已生成结构化摘要"
                )));
            }
        }
    }

    /// 把当前 (任一) 已完成的 assistant message 从内部 map 落地成 entries。
    /// 只移除已 emit 的 message 的 map entries；未完成的保留在 map 里等后续 MessageEnd。
    fn flush_open_assistant(&mut self) {
        let mut completed_ids: Vec<Uuid> = Vec::new();
        for (mid, done) in &self.cur_assistant_completed {
            if *done {
                completed_ids.push(*mid);
            }
        }
        for mid in completed_ids {
            if let Some(text) = self.cur_assistant_text.remove(&mid) {
                self.cur_assistant_completed.remove(&mid);
                self.entries.push(ChatEntry::Assistant {
                    text,
                    time: stamp(),
                });
                if self.current_message_id == Some(mid) {
                    self.current_message_id = None;
                }
            }
        }
    }

    /// 清除所有未完成的流式 assistant 状态（Abort / TurnEnd 兜底）。
    /// 已落地为 entries 的内容不受影响；这里只丢弃内部 map 中那些
    /// 没有等到 MessageEnd 的残留，并重置 current_message_id。
    fn discard_incomplete_assistant(&mut self) {
        self.cur_assistant_text.clear();
        self.cur_assistant_completed.clear();
        self.current_message_id = None;
    }

    /// 把持久化事件（来自 GetHistory）灌进 app，像实时收到一样重建
    /// entries。resume 已有会话时使用，让 TUI 显示历史对话。
    pub fn load_history(&mut self, events: &[PersistedAgentEvent]) {
        for ev in events {
            self.apply_event(ev.event.clone());
        }
    }

    /// 返回进行中的 assistant 消息已累积文本（若有）。UI 在 MessageEnd
    /// 之前用它渲染流式文本。
    pub fn streaming_text(&self) -> Option<&str> {
        self.current_message_id
            .and_then(|mid| self.cur_assistant_text.get(&mid))
            .map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_protocol::agent_event::{
        AgentEndReason, IntegrityIssue, IntegrityIssueKind, MessageStopReason, TurnStopReason,
    };
    use parrot_protocol::types::{McpServerState, McpServerStatusWire, ToolOutput, Usage};
    use uuid::Uuid;

    fn sid() -> SessionId {
        SessionId::new_v4()
    }

    #[test]
    fn new_app_is_empty_normal_mode() {
        let app = App::new(sid());
        assert!(app.entries.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.ended);
    }

    #[test]
    fn turn_start_pushes_user_entry() {
        let mut app = App::new(sid());
        app.apply_event(AgentEvent::TurnStart {
            session_id: app.session_id,
            turn_id: Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(app.entries[0], ChatEntry::User { ref text, .. } if text == "hi"));
    }

    #[test]
    fn text_deltas_accumulate_into_assistant_entry_on_message_end() {
        let mid = Uuid::new_v4();
        let sid_v = sid();
        let mut app = App::new(sid_v);
        let evs = vec![
            AgentEvent::MessageStart {
                session_id: sid_v,
                turn_id: Uuid::new_v4(),
                message_id: mid,
            },
            AgentEvent::MessageDelta {
                session_id: sid_v,
                message_id: mid,
                payload: MessageDeltaPayload::TextDelta {
                    delta: "Hel".into(),
                },
            },
            AgentEvent::MessageDelta {
                session_id: sid_v,
                message_id: mid,
                payload: MessageDeltaPayload::TextDelta { delta: "lo".into() },
            },
            AgentEvent::MessageEnd {
                session_id: sid_v,
                turn_id: Uuid::new_v4(),
                message_id: mid,
                final_content: "Hello".into(),
                tool_calls: Vec::new(),
                stop_reason: MessageStopReason::EndTurn,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            },
        ];
        for ev in evs {
            app.apply_event(ev);
        }
        // flush 在 TurnEnd 时发生；模拟 TurnEnd 触发
        app.apply_event(AgentEvent::TurnEnd {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            stop_reason: TurnStopReason::EndTurn,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        let ass = app
            .entries
            .iter()
            .find_map(|e| match e {
                ChatEntry::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("expected one assistant entry");
        assert_eq!(ass, "Hello");
    }

    #[test]
    fn tool_end_backfills_result_into_matching_tool_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            parent_message_id: Uuid::new_v4(),
            tool_call_id: "tc1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "x"}),
        });
        app.apply_event(AgentEvent::ToolEnd {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc1".into(),
            result: ToolOutput {
                content: "ok".into(),
                is_error: false,
            },
        });
        match &app.entries[0] {
            ChatEntry::Tool { result, .. } => {
                assert!(result.is_some());
                assert_eq!(result.as_ref().unwrap().content, "ok");
            }
            other => panic!("expected Tool entry, got {:?}", other),
        }
    }

    #[test]
    fn tool_confirm_required_sets_confirm_pending() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolConfirmRequired {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc7".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({"command": "rm -rf /"}),
        });
        assert_eq!(app.mode, Mode::ConfirmPending);
        assert!(app.pending_confirmation.is_some());
        assert_eq!(
            app.pending_confirmation.as_ref().unwrap().tool_call_id,
            "tc7"
        );
    }

    #[test]
    fn confirm_decision_in_normal_mode_returns_none() {
        let mut app = App::new(sid());
        assert!(app.confirm_decision(ConfirmDecision::Approve).is_none());
    }

    #[test]
    fn confirm_decision_in_confirm_pending_emits_and_clears() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolConfirmRequired {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc7".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({}),
        });
        let out = app.confirm_decision(ConfirmDecision::Approve);
        assert_eq!(out, Some(("tc7".into(), ConfirmDecision::Approve)));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_confirmation.is_none());
    }

    #[test]
    fn agent_end_marks_ended() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::AgentEnd {
            session_id: sid_v,
            reason: AgentEndReason::ClientClose,
            total_usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        assert!(app.ended);
    }

    #[test]
    fn agent_start_after_agent_end_resets_ended_for_resumed_session() {
        let sid_v = sid();
        let mut app = App::new(sid_v);

        // 历史加载会回放上一次断连留下的旧 AgentEnd。
        app.apply_event(AgentEvent::AgentEnd {
            session_id: sid_v,
            reason: AgentEndReason::ClientClose,
            total_usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        assert!(app.ended);

        // resume 后的引擎发来新的 AgentStart。
        let should_quit = app.apply_server_message(ServerMessage::AgentEvent {
            event: AgentEvent::AgentStart {
                session_id: sid_v,
                model: "claude".into(),
                provider: "anthropic".into(),
                system_prompt_hash: "hash".into(),
                resumed_from_seq: Some(2),
            },
        });
        assert!(!should_quit);
        assert!(!app.ended);

        // 之后的新用户 turn 应正常处理。
        let should_quit = app.apply_server_message(ServerMessage::AgentEvent {
            event: AgentEvent::TurnStart {
                session_id: sid_v,
                turn_id: Uuid::new_v4(),
                user_message: "hi again".into(),
            },
        });
        assert!(!should_quit);
        assert!(matches!(
            app.entries.last(),
            Some(ChatEntry::User { text, .. }) if text == "hi again"
        ));
    }

    #[test]
    fn apply_server_message_error_pushes_error_entry() {
        let mut app = App::new(sid());
        let should_quit = app.apply_server_message(ServerMessage::Error {
            session_id: None,
            code: parrot_protocol::types::ErrorCode::InternalError,
            message: "boom".into(),
        });
        assert!(!should_quit);
        match &app.entries[0] {
            ChatEntry::Error(s) => assert_eq!(s, "boom"),
            other => panic!("expected Error entry, got {:?}", other),
        }
    }

    #[test]
    fn replay_integrity_warning_pushes_warning_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ReplayIntegrityWarning {
            session_id: sid_v,
            issue: IntegrityIssue {
                kind: IntegrityIssueKind::PartialTurn,
                dropped_event_count: 3,
                first_dropped_seq: 0,
                last_dropped_seq: 0,
                dangling_turn_ids: Vec::new(),
                dangling_message_ids: Vec::new(),
                dangling_tool_call_ids: Vec::new(),
            },
        });
        assert!(matches!(app.entries.last(), Some(ChatEntry::Warning(_))));
    }

    #[test]
    fn shell_result_fills_pending_shell_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.entries.push(ChatEntry::Shell {
            command: "echo hi".into(),
            output: None,
            exit_code: None,
        });
        app.apply_server_message(ServerMessage::ShellResult {
            session_id: sid_v,
            output: "hi\n".into(),
            exit_code: 0,
        });
        match &app.entries[0] {
            ChatEntry::Shell {
                output, exit_code, ..
            } => {
                assert_eq!(output.as_deref(), Some("hi\n"));
                assert_eq!(*exit_code, Some(0));
            }
            other => panic!("expected Shell entry, got {:?}", other),
        }
    }

    #[test]
    fn shell_result_skips_entries_without_pending_output() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.entries.push(ChatEntry::Info("note".into()));
        app.apply_server_message(ServerMessage::ShellResult {
            session_id: sid_v,
            output: "hi\n".into(),
            exit_code: 0,
        });
        assert_eq!(app.entries.len(), 1);
    }

    #[test]
    fn mcp_notice_renders_info_line() {
        let mut app = App::new(sid());
        app.apply_server_message(ServerMessage::McpNotice {
            id: "playwright".into(),
            state: McpServerState::Failed,
            detail: "spawn 失败: program not found".into(),
            tool_count: 0,
        });
        let last = app.entries.last().unwrap();
        match last {
            ChatEntry::Info(text) => {
                assert!(text.contains("MCP playwright"), "got: {text}");
                assert!(text.contains("spawn 失败"), "失败详情必须可见: {text}");
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn mcp_servers_reply_renders_status_table() {
        let mut app = App::new(sid());
        app.apply_server_message(ServerMessage::McpServers {
            entries: vec![McpServerStatusWire {
                id: "mock".into(),
                state: McpServerState::Connected,
                detail: String::new(),
                tool_count: 3,
            }],
        });
        match app.entries.last().unwrap() {
            ChatEntry::Info(text) => {
                assert!(text.contains("mock") && text.contains("3"), "got: {text}");
            }
            other => panic!("expected Info, got: {other:?}"),
        }
    }

    #[test]
    fn model_set_updates_model_and_pushes_info() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_server_message(ServerMessage::ModelSet {
            session_id: sid_v,
            model: "gpt-5".into(),
        });
        assert_eq!(app.model, "gpt-5");
        match app.entries.last() {
            Some(ChatEntry::Info(text)) => {
                assert!(text.contains("模型已切换为 gpt-5"), "{text}")
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn is_turn_active_reflects_turn_state() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        assert!(!app.is_turn_active());
        app.apply_event(AgentEvent::TurnStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert!(app.is_turn_active());
    }

    #[test]
    fn pending_turn_cleared_by_turn_events() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.pending_turn = true;
        app.apply_event(AgentEvent::TurnStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert!(!app.pending_turn, "TurnStart 应清除 pending_turn");

        app.pending_turn = true;
        app.apply_event(AgentEvent::TurnEnd {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            stop_reason: TurnStopReason::EndTurn,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        assert!(!app.pending_turn, "TurnEnd 应清除 pending_turn");

        app.pending_turn = true;
        app.apply_event(AgentEvent::AgentEnd {
            session_id: sid_v,
            reason: AgentEndReason::ClientClose,
            total_usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        assert!(!app.pending_turn, "AgentEnd 应清除 pending_turn");
    }

    #[test]
    fn pending_turn_cleared_by_server_error() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.pending_turn = true;
        app.apply_server_message(ServerMessage::Error {
            session_id: None,
            code: parrot_protocol::types::ErrorCode::InvalidRequest,
            message: "chat rejected".into(),
        });
        assert!(
            !app.pending_turn,
            "Error 应清除 pending_turn，避免 /model 永久被拦截"
        );

        // 匹配本会话的 Error 同样清除。
        app.pending_turn = true;
        app.apply_server_message(ServerMessage::Error {
            session_id: Some(sid_v),
            code: parrot_protocol::types::ErrorCode::InvalidRequest,
            message: "chat rejected".into(),
        });
        assert!(!app.pending_turn, "匹配会话的 Error 应清除 pending_turn");

        // 其他会话的 Error 不得误清本会话的 pending_turn。
        app.pending_turn = true;
        app.apply_server_message(ServerMessage::Error {
            session_id: Some(Uuid::new_v4()),
            code: parrot_protocol::types::ErrorCode::InvalidRequest,
            message: "other session error".into(),
        });
        assert!(app.pending_turn, "其他会话的 Error 不得清除 pending_turn");
    }

    #[test]
    fn clamp_scroll_caps_offset() {
        let mut app = App::new(sid());
        app.scroll_up(u16::MAX);
        app.clamp_scroll(40);
        assert_eq!(app.scroll_offset, 40);
    }

    #[test]
    fn scroll_to_top_then_clamp_is_bounded() {
        let mut app = App::new(sid());
        app.scroll_to_top();
        assert_eq!(app.scroll_offset, u16::MAX);
        app.clamp_scroll(12);
        assert_eq!(app.scroll_offset, 12);
    }

    #[test]
    fn scroll_to_bottom_zeroes_offset() {
        let mut app = App::new(sid());
        app.scroll_up(100);
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn compaction_start_and_summary_toggle_compacting() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::CompactionStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
        });
        assert!(app.compacting);
        app.apply_event(AgentEvent::CompactionSummary {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            summary: "[CONVERSATION SUMMARY]\n...".into(),
            dropped_message_count: 6,
            kept_message_count: 4,
        });
        assert!(!app.compacting);

        app.apply_event(AgentEvent::CompactionStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
        });
        assert!(app.compacting);
        app.apply_event(AgentEvent::TurnStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert!(
            !app.compacting,
            "TurnStart clears compacting (failure path)"
        );
    }

    fn tool_start_app() -> (App, uuid::Uuid) {
        let sid = uuid::Uuid::new_v4();
        let mut app = App::new(sid);
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid,
            turn_id: uuid::Uuid::new_v4(),
            parent_message_id: uuid::Uuid::new_v4(),
            tool_call_id: "tc_1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid,
            turn_id: uuid::Uuid::new_v4(),
            parent_message_id: uuid::Uuid::new_v4(),
            tool_call_id: "tc_2".into(),
            tool_name: "file_edit".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        (app, sid)
    }

    #[test]
    fn select_next_tool_cycles_forward_and_backward() {
        let (mut app, _) = tool_start_app();
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_1"));
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_2"));
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_1"), "正向应环绕");

        app.clear_tool_selection();
        app.select_next_tool(false);
        assert_eq!(
            app.selected_tool.as_deref(),
            Some("tc_2"),
            "无选中时反向取最后一个"
        );
    }

    #[test]
    fn toggle_selected_tool_toggles_expansion() {
        let (mut app, _) = tool_start_app();
        app.select_next_tool(true);
        assert!(app.toggle_selected_tool(), "首次切换应成功");
        assert!(matches!(
            &app.entries[0],
            ChatEntry::Tool { expanded: true, .. }
        ));
        assert!(app.toggle_selected_tool());
        assert!(matches!(
            &app.entries[0],
            ChatEntry::Tool {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn toggle_without_selection_is_noop() {
        let (mut app, _) = tool_start_app();
        assert!(!app.toggle_selected_tool());
    }

    #[test]
    fn compaction_summary_pushes_warning_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::CompactionSummary {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            summary: "[CONVERSATION SUMMARY]\n...".into(),
            dropped_message_count: 6,
            kept_message_count: 4,
        });
        match app.entries.last() {
            Some(ChatEntry::Warning(text)) => {
                assert!(text.contains("6"), "warning mentions dropped count: {text}");
            }
            other => panic!("expected Warning entry, got {:?}", other),
        }
    }
}
