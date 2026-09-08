use crate::types::{ChatMessage, ChatRole, ToolCallInfo as CoreToolCallInfo};
use parrot_protocol::agent_event::{
    AgentEvent, IntegrityIssue, IntegrityIssueKind, PersistedAgentEvent,
};
use std::io::Write;

pub struct EventLog {
    dir: std::path::PathBuf,
    current_seq: u64,
}

impl EventLog {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            current_seq: 0,
        }
    }

    /// 设置 seq 计数起点——resume 回放后调用，让新追加的事件从
    /// 回放停止的位置继续编号。
    pub fn with_start_seq(mut self, seq: u64) -> Self {
        self.current_seq = seq;
        self
    }

    /// 把持久化 `AgentEvent` 追加到 `events.log`。非持久化事件
    /// （`MessageDelta`、`ToolUpdate`）由调用方过滤——只有
    /// `is_persistent()` 的事件会到达此方法。
    pub fn append(&mut self, event: AgentEvent) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("events.log");
        let meta = PersistedAgentEvent {
            seq: self.current_seq,
            ts: chrono::Utc::now(),
            event,
        };
        let mut line = serde_json::to_string(&meta).map_err(std::io::Error::other)?;
        line.push('\n');
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(line.as_bytes())?;
        self.current_seq += 1;
        Ok(())
    }

    pub fn replay(&self) -> std::io::Result<Vec<PersistedAgentEvent>> {
        let path = self.dir.join("events.log");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut entries = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PersistedAgentEvent>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!("Failed to parse event log line: {}", e);
                }
            }
        }
        Ok(entries)
    }

    /// resume 专用回放：读 `events.log`，截掉半成品 turn，把丢弃的
    /// 事件写入 `corrupted.log`，用截断后的内容重写 `events.log`，
    /// 返回干净流和可选 `IntegrityIssue`（调用方据此发
    /// `ReplayIntegrityWarning`）。
    pub fn replay_for_resume(
        &mut self,
    ) -> std::io::Result<(Vec<PersistedAgentEvent>, Option<IntegrityIssue>)> {
        let raw = self.replay()?;
        let (clean, dropped, issue) = truncate_to_last_complete_turn(raw);

        if let Some(ref issue) = issue {
            if let Err(e) = self.append_corrupted_block(issue, &dropped) {
                tracing::error!(error = ?e, "failed to write corrupted.log; dropped events lost");
            }
            if let Err(e) = self.rewrite_truncated(&clean) {
                tracing::error!(error = ?e, "failed to rewrite events.log after truncation");
                return Err(e);
            }
            tracing::warn!(
                kind = ?issue.kind,
                dropped = issue.dropped_event_count,
                first_seq = issue.first_dropped_seq,
                last_seq = issue.last_dropped_seq,
                "events.log integrity issue — truncated to last complete turn"
            );
            self.current_seq = clean.len() as u64;
        }

        Ok((clean, issue))
    }

    fn append_corrupted_block(
        &self,
        issue: &IntegrityIssue,
        dropped: &[PersistedAgentEvent],
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("corrupted.log");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let header = serde_json::json!({
            "_corrupted_block": true,
            "detected_at": chrono::Utc::now(),
            "issue": issue,
        });
        writeln!(file, "{}", header)?;
        for ev in dropped {
            writeln!(
                file,
                "{}",
                serde_json::to_string(ev).map_err(std::io::Error::other)?
            )?;
        }
        Ok(())
    }

    fn rewrite_truncated(&self, clean: &[PersistedAgentEvent]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("events.log");
        let tmp_path = self.dir.join("events.log.tmp");
        {
            let file = std::fs::File::create(&tmp_path)?;
            let mut writer = std::io::BufWriter::new(file);
            for ev in clean {
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(ev).map_err(std::io::Error::other)?
                )?;
            }
            writer.flush()?;
        }
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    fn write_snapshot(&self, snapshot: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("snapshot.json");
        std::fs::write(path, snapshot)
    }

    pub fn maybe_snapshot(&self, context: &[ChatMessage]) -> std::io::Result<()> {
        const SNAPSHOT_INTERVAL: u64 = 100;
        if self.current_seq == 0 || !self.current_seq.is_multiple_of(SNAPSHOT_INTERVAL) {
            return Ok(());
        }
        let json = serde_json::to_string_pretty(context).map_err(std::io::Error::other)?;
        self.write_snapshot(&json)?;
        tracing::info!(seq = self.current_seq, "wrote session snapshot");
        Ok(())
    }
}

/// 把事件流截断到"最后一个完整 TurnEnd 之后"。返回 `(保留, 丢弃, issue)`。
/// `issue` 为 `None` 表示无需截断。
pub fn truncate_to_last_complete_turn(
    events: Vec<PersistedAgentEvent>,
) -> (
    Vec<PersistedAgentEvent>,
    Vec<PersistedAgentEvent>,
    Option<IntegrityIssue>,
) {
    // 严重损坏：AgentEnd 之后还有事件。
    let agent_end_idx = events
        .iter()
        .enumerate()
        .find_map(|(i, ev)| matches!(&ev.event, AgentEvent::AgentEnd { .. }).then_some(i));
    if let Some(end_idx) = agent_end_idx {
        if end_idx + 1 < events.len() {
            let (keep, drop) = events.split_at(end_idx + 1);
            let issue = IntegrityIssue {
                kind: IntegrityIssueKind::EventsAfterAgentEnd,
                dropped_event_count: drop.len() as u32,
                first_dropped_seq: drop.first().map(|e| e.seq).unwrap_or(0),
                last_dropped_seq: drop.last().map(|e| e.seq).unwrap_or(0),
                dangling_turn_ids: Vec::new(),
                dangling_message_ids: Vec::new(),
                dangling_tool_call_ids: Vec::new(),
            };
            return (keep.to_vec(), drop.to_vec(), Some(issue));
        }
    }

    // 半成品 turn：找最后一个完整 TurnEnd，其后都是"尾巴"。尾巴里的
    // 生命周期边界标记（AgentStart / AgentEnd / ReplayIntegrityWarning）
    // 不算 turn 内容——闭合 turn 之后的孤立 AgentEnd，或 resume 时还没
    // 开始 turn 的开头 AgentStart，都是干净的，必须保留。半成品 turn
    // 从尾巴里第一个 `TurnStart` 开始；找不到则尾巴干净，全部保留。
    let last_turn_end_idx = events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, ev)| matches!(&ev.event, AgentEvent::TurnEnd { .. }).then_some(i));

    let tail_start = last_turn_end_idx.map(|i| i + 1).unwrap_or(0);

    let partial_start = events[tail_start..]
        .iter()
        .enumerate()
        .map(|(i, _)| tail_start + i)
        .find(|i| matches!(&events[*i].event, AgentEvent::TurnStart { .. }));

    let Some(partial_start) = partial_start else {
        // 尾巴里只有生命周期标记（或为空）——干净前缀。
        return (events, Vec::new(), None);
    };

    let (keep, drop) = events.split_at(partial_start);
    let drop = drop.to_vec();

    let mut issue = IntegrityIssue {
        kind: IntegrityIssueKind::PartialTurn,
        dropped_event_count: drop.len() as u32,
        first_dropped_seq: drop.first().map(|e| e.seq).unwrap_or(0),
        last_dropped_seq: drop.last().map(|e| e.seq).unwrap_or(0),
        dangling_turn_ids: Vec::new(),
        dangling_message_ids: Vec::new(),
        dangling_tool_call_ids: Vec::new(),
    };

    for ev in &drop {
        match &ev.event {
            AgentEvent::TurnStart { turn_id, .. } => issue.dangling_turn_ids.push(*turn_id),
            AgentEvent::MessageStart { message_id, .. } => {
                issue.dangling_message_ids.push(*message_id)
            }
            AgentEvent::ToolStart { tool_call_id, .. } => {
                issue.dangling_tool_call_ids.push(tool_call_id.clone())
            }
            _ => {}
        }
    }

    (keep.to_vec(), drop, Some(issue))
}

/// 从回放的事件日志重建 `Vec<ChatMessage>`。只有 `TurnStart`（用户消息）、
/// `MessageEnd`（助手消息）和 `ToolEnd`（工具结果）会产生上下文条目。
/// Start/Delta/AgentStart/AgentEnd/TurnEnd 一律跳过。
pub fn rebuild_context(events: &[PersistedAgentEvent]) -> Vec<ChatMessage> {
    let mut ctx = Vec::new();
    for ev in events {
        match &ev.event {
            AgentEvent::TurnStart { user_message, .. } => {
                ctx.push(ChatMessage::new(ChatRole::User, user_message.clone()));
            }
            AgentEvent::MessageEnd {
                final_content,
                tool_calls,
                ..
            } => {
                let core_tcs: Vec<CoreToolCallInfo> =
                    tool_calls.iter().map(CoreToolCallInfo::from).collect();
                ctx.push(ChatMessage {
                    tool_calls: (!core_tcs.is_empty()).then_some(core_tcs),
                    ..ChatMessage::new(ChatRole::Assistant, final_content.clone())
                });
            }
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                ..
            } => {
                ctx.push(ChatMessage {
                    tool_call_id: Some(tool_call_id.clone()),
                    ..ChatMessage::new(ChatRole::Tool, result.content.clone())
                });
            }
            AgentEvent::CompactionSummary {
                summary,
                kept_message_count,
                ..
            } => {
                // 消息计数语义(spec §4):当前已重建列表 == 压缩前上下文
                // (不含 system)。保留末尾 kept_message_count 条,摘要以
                // User 角色插在最前。
                let kept = (*kept_message_count as usize).min(ctx.len());
                let kept_msgs: Vec<ChatMessage> = ctx.split_off(ctx.len() - kept);
                ctx.clear(); // 丢弃被摘要的前缀
                ctx.push(ChatMessage::new(ChatRole::User, summary.clone()));
                ctx.extend(kept_msgs);
            }
            _ => {}
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_protocol::agent_event::{MessageStopReason, TurnStopReason};
    use parrot_protocol::types::Usage;
    use uuid::Uuid;

    fn make_persisted(seq: u64, event: AgentEvent) -> PersistedAgentEvent {
        PersistedAgentEvent {
            seq,
            ts: chrono::Utc::now(),
            event,
        }
    }

    #[test]
    fn truncate_empty_is_noop() {
        let (keep, drop, issue) = truncate_to_last_complete_turn(Vec::new());
        assert!(keep.is_empty());
        assert!(drop.is_empty());
        assert!(issue.is_none());
    }

    #[test]
    fn truncate_complete_log_is_noop() {
        let sid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: tid,
                    user_message: "hi".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: tid,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events.clone());
        assert_eq!(keep.len(), 2);
        assert!(drop.is_empty());
        assert!(issue.is_none());
    }

    #[test]
    fn truncate_partial_turn_drops_tail() {
        let sid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: tid,
                    user_message: "first".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: tid,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    user_message: "partial".into(),
                },
            ),
            make_persisted(
                3,
                AgentEvent::MessageStart {
                    session_id: sid,
                    turn_id: tid,
                    message_id: mid,
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events);
        assert_eq!(keep.len(), 2);
        assert_eq!(drop.len(), 2);
        let issue = issue.expect("expected integrity issue");
        assert_eq!(issue.dropped_event_count, 2);
        assert_eq!(issue.dangling_message_ids.len(), 1);
        assert_eq!(issue.dangling_turn_ids.len(), 1);
    }

    #[test]
    fn truncate_events_after_agent_end_is_severe() {
        let sid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::AgentStart {
                    session_id: sid,
                    model: "m".into(),
                    provider: "p".into(),
                    system_prompt_hash: "h".into(),
                    resumed_from_seq: None,
                },
            ),
            make_persisted(
                1,
                AgentEvent::AgentEnd {
                    session_id: sid,
                    reason: parrot_protocol::agent_event::AgentEndReason::ClientClose,
                    total_usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    user_message: "after".into(),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events);
        assert_eq!(keep.len(), 2);
        assert_eq!(drop.len(), 1);
        let issue = issue.expect("expected integrity issue");
        assert_eq!(issue.kind, IntegrityIssueKind::EventsAfterAgentEnd);
    }

    /// 回归：闭合 turn 之后的孤立 `AgentEnd` 是干净的停机标记，
    /// 不是半成品 turn。必须保留，不产生完整性问题。
    #[test]
    fn truncate_lone_trailing_agent_end_is_kept() {
        let sid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: tid,
                    user_message: "hi".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: tid,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::AgentEnd {
                    session_id: sid,
                    reason: parrot_protocol::agent_event::AgentEndReason::ClientClose,
                    total_usage: Usage::default(),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events.clone());
        assert_eq!(keep.len(), 3, "AgentEnd must be kept");
        assert!(drop.is_empty());
        assert!(issue.is_none());
    }

    /// 回归：resume 时先于任何新 turn 到达的开头
    /// `AgentStart{resumed_from_seq}` 是生命周期标记，不是半成品 turn。
    /// 必须保留（对应用户报告的 bug：`corrupted.log` 里出现了被当作
    /// `PartialTurn` 丢弃的孤立 `AgentStart`）。
    #[test]
    fn truncate_lone_trailing_resume_agent_start_is_kept() {
        let sid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: tid,
                    user_message: "hi".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: tid,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::AgentStart {
                    session_id: sid,
                    model: "m".into(),
                    provider: "p".into(),
                    system_prompt_hash: "h".into(),
                    resumed_from_seq: Some(2),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events.clone());
        assert_eq!(keep.len(), 3, "resume AgentStart must be kept");
        assert!(drop.is_empty());
        assert!(issue.is_none());
    }

    #[test]
    fn rebuild_context_produces_expected_messages() {
        let sid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: tid,
                    user_message: "hi".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::MessageEnd {
                    session_id: sid,
                    turn_id: tid,
                    message_id: mid,
                    final_content: "hello".into(),
                    tool_calls: vec![],
                    stop_reason: MessageStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
        ];
        let ctx = rebuild_context(&events);
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].role, ChatRole::User);
        assert_eq!(ctx[0].content, "hi");
        assert_eq!(ctx[1].role, ChatRole::Assistant);
        assert_eq!(ctx[1].content, "hello");
    }

    #[test]
    fn rebuild_applies_compaction_summary() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let m1 = Uuid::new_v4();
        let m2 = Uuid::new_v4();
        // Turn 1（将被摘要）、Turn 2（保留），然后压缩。
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "old question".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::MessageEnd {
                    session_id: sid,
                    turn_id: t1,
                    message_id: m1,
                    final_content: "old answer".into(),
                    tool_calls: vec![],
                    stop_reason: MessageStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t2,
                    user_message: "new question".into(),
                },
            ),
            make_persisted(
                3,
                AgentEvent::MessageEnd {
                    session_id: sid,
                    turn_id: t2,
                    message_id: m2,
                    final_content: "new answer".into(),
                    tool_calls: vec![],
                    stop_reason: MessageStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                4,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: t2,
                    summary: "[CONVERSATION SUMMARY]\n## 目标\n...".into(),
                    dropped_message_count: 2,
                    kept_message_count: 2,
                },
            ),
        ];
        let ctx = rebuild_context(&events);
        assert_eq!(ctx.len(), 3, "summary + kept 2 messages");
        assert_eq!(ctx[0].role, ChatRole::User);
        assert!(ctx[0].content.starts_with("[CONVERSATION SUMMARY]"));
        assert_eq!(ctx[1].content, "new question");
        assert_eq!(ctx[2].content, "new answer");
        assert!(
            !ctx.iter().any(|m| m.content == "old question"),
            "summarized messages must be dropped"
        );
    }

    #[test]
    fn rebuild_compaction_keeps_all_when_count_exceeds() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "q".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: t1,
                    summary: "[CONVERSATION SUMMARY]\n...".into(),
                    dropped_message_count: 0,
                    kept_message_count: 99, // more than rebuilt
                },
            ),
        ];
        let ctx = rebuild_context(&events);
        assert_eq!(ctx.len(), 2);
        assert!(ctx[0].content.starts_with("[CONVERSATION SUMMARY]"));
        assert_eq!(ctx[1].content, "q");
    }

    #[test]
    fn truncate_keeps_compaction_summary_before_partial_turn() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "done turn".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: t1,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    summary: "[CONVERSATION SUMMARY]\n...".into(),
                    dropped_message_count: 1,
                    kept_message_count: 1,
                },
            ),
            make_persisted(
                3,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    user_message: "partial turn".into(),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events);
        assert!(issue.is_some(), "partial turn must be detected");
        assert_eq!(keep.len(), 3, "CompactionSummary must survive truncation");
        assert!(
            matches!(
                keep.last().map(|e| &e.event),
                Some(AgentEvent::CompactionSummary { .. })
            ),
            "last kept event is the CompactionSummary"
        );
        assert_eq!(drop.len(), 1);
    }
}
