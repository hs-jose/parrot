use parrot_protocol::agent_event::{AgentEvent, MessageDeltaPayload, ToolCallInfo};
use parrot_protocol::types::{ConfirmDecision, ToolOutput};
use parrot_protocol::{ServerMessage, SessionId};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) enum ChatEntry {
    User(String),
    Assistant {
        text: String,
        completed: bool,
        #[allow(dead_code)]
        tool_calls: Vec<ToolCallInfo>,
    },
    Tool {
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        result: Option<ToolOutput>,
    },
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
    pub quit: bool,
    pub scroll_offset: u16,
    cur_assistant_text: HashMap<Uuid, String>,
    cur_assistant_tools: HashMap<Uuid, Vec<ToolCallInfo>>,
    cur_assistant_completed: HashMap<Uuid, bool>,
}

impl App {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            entries: Vec::new(),
            mode: Mode::Normal,
            pending_confirmation: None,
            ended: false,
            quit: false,
            scroll_offset: 0,
            cur_assistant_text: HashMap::new(),
            cur_assistant_tools: HashMap::new(),
            cur_assistant_completed: HashMap::new(),
        }
    }

    pub fn push_user_input(&mut self, text: String) {
        self.entries.push(ChatEntry::User(text));
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    #[allow(dead_code)]
    pub fn quit(&mut self) {
        self.quit = true;
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
            ServerMessage::Error { message, .. } => {
                self.entries.push(ChatEntry::Error(message));
            }
            // TUI 默认不处理其他 ServerMessage（HelloAck 已在握手期完成；SessionList/History 等由列表页消费）
            _ => {}
        }
        self.quit || self.ended
    }

    pub fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart { .. } => {}
            AgentEvent::AgentEnd { .. } => {
                self.ended = true;
            }
            AgentEvent::TurnStart { user_message, .. } => {
                self.entries.push(ChatEntry::User(user_message));
            }
            AgentEvent::TurnEnd { .. } => {
                self.flush_open_assistant();
                self.scroll_offset = 0;
            }
            AgentEvent::MessageStart { message_id, .. } => {
                self.flush_open_assistant();
                self.cur_assistant_text.insert(message_id, String::new());
                self.cur_assistant_tools.insert(message_id, Vec::new());
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
                tool_calls,
                ..
            } => {
                self.cur_assistant_text.insert(message_id, final_content);
                self.cur_assistant_tools.insert(message_id, tool_calls);
                self.cur_assistant_completed.insert(message_id, true);
                self.flush_open_assistant();
            }
            AgentEvent::ToolStart {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.entries.push(ChatEntry::Tool {
                    tool_call_id,
                    tool_name,
                    arguments,
                    result: None,
                });
            }
            AgentEvent::ToolUpdate { .. } => {}
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                ..
            } => {
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
        }
    }

    /// 把当前 (任一) 已完成的 assistant message 从内部 map 落地成 entries。
    /// 单 turn 内不知何时 TurnEnd，所以保守地：任一已 completed 的 message 立即落地一条；
    /// 处理 TurnEnd 时把全部 uncompleted 也清空（丢弃空文本，多余没 Problem）。
    fn flush_open_assistant(&mut self) {
        let mut still_open: Vec<Uuid> = Vec::new();
        let mut to_emit: Vec<(Uuid, String, Vec<ToolCallInfo>)> = Vec::new();
        for (mid, done) in &self.cur_assistant_completed {
            if *done {
                if let Some(text) = self.cur_assistant_text.remove(mid) {
                    let tools = self.cur_assistant_tools.remove(mid).unwrap_or_default();
                    to_emit.push((*mid, text, tools));
                }
            } else {
                still_open.push(*mid);
            }
        }
        for (_mid, text, _tools) in to_emit {
            self.entries.push(ChatEntry::Assistant {
                text,
                completed: true,
                tool_calls: Vec::new(), // 工具单独以 ChatEntry::Tool 形式由 ToolStart 流入
            });
        }
        // 清掉 completed marker 防止重复 flush
        for mid in still_open {
            // keep open message maps intact
            let _ = mid;
        }
        self.cur_assistant_completed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_protocol::agent_event::{
        AgentEndReason, IntegrityIssue, IntegrityIssueKind, MessageStopReason, TurnStopReason,
    };
    use parrot_protocol::types::{ToolOutput, Usage};
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
        assert!(!app.quit);
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
        assert!(matches!(app.entries[0], ChatEntry::User(ref s) if s == "hi"));
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
        // flush happens on TurnEnd; 模拟 TurnEnd 触发
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
    fn quit_flag_via_quit_method() {
        let mut app = App::new(sid());
        app.quit();
        assert!(app.quit);
    }
}
