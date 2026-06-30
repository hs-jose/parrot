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

    /// Set the starting seq counter — used after a resume replay so new
    /// appends continue from where the replay left off.
    pub fn with_start_seq(mut self, seq: u64) -> Self {
        self.current_seq = seq;
        self
    }

    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }

    /// Append a persistent `AgentEvent` to `events.log`. Non-persistent
    /// events (`MessageDelta`, `ToolUpdate`) are filtered out by the caller
    /// — only `is_persistent()` events reach this method.
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

    /// Resume-specific replay: reads `events.log`, truncates partial turns,
    /// writes discarded events to `corrupted.log`, rewrites `events.log` with
    /// the truncated content, and returns the clean stream plus an optional
    /// `IntegrityIssue` (caller emits `ReplayIntegrityWarning`).
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

    pub fn write_snapshot(&self, snapshot: &str) -> std::io::Result<()> {
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

/// Truncate the event stream to "after the last complete TurnEnd".
/// Returns `(kept, dropped, issue)`. If `issue` is `None`, no truncation
/// was needed.
pub fn truncate_to_last_complete_turn(
    events: Vec<PersistedAgentEvent>,
) -> (
    Vec<PersistedAgentEvent>,
    Vec<PersistedAgentEvent>,
    Option<IntegrityIssue>,
) {
    // Severe corruption: AgentEnd followed by more events.
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

    // Partial turn: find the last complete TurnEnd. Anything after it is the
    // "tail". Within the tail, lifecycle boundary markers (AgentStart /
    // AgentEnd / ReplayIntegrityWarning) are NOT turn content — a lone
    // trailing AgentEnd after a closed turn, or a resume's leading
    // AgentStart with no turn started yet, are clean and must be kept. The
    // partial turn begins at the first `TurnStart` in the tail; if there is
    // none, the tail is clean and we keep everything.
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
        // Tail contains only lifecycle markers (or is empty) — clean prefix.
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

/// Rebuild a `Vec<ChatMessage>` from a replayed event log. Only
/// `TurnStart` (user message), `MessageEnd` (assistant message), and
/// `ToolEnd` (tool result) produce context entries. Start/Delta/AgentStart/
/// AgentEnd/TurnEnd are skipped.
pub fn rebuild_context(events: &[PersistedAgentEvent]) -> Vec<ChatMessage> {
    let mut ctx = Vec::new();
    for ev in events {
        match &ev.event {
            AgentEvent::TurnStart { user_message, .. } => {
                ctx.push(ChatMessage {
                    role: ChatRole::User,
                    content: user_message.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                });
            }
            AgentEvent::MessageEnd {
                final_content,
                tool_calls,
                ..
            } => {
                let core_tcs: Vec<CoreToolCallInfo> = tool_calls
                    .iter()
                    .map(|tc| CoreToolCallInfo {
                        id: tc.tool_call_id.clone(),
                        name: tc.tool_name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect();
                ctx.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: final_content.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: if core_tcs.is_empty() {
                        None
                    } else {
                        Some(core_tcs)
                    },
                });
            }
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                ..
            } => {
                ctx.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: result.content.clone(),
                    tool_call_id: Some(tool_call_id.clone()),
                    tool_name: None,
                    tool_calls: None,
                });
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

    /// Regression: a lone trailing `AgentEnd` after a closed turn is a clean
    /// shutdown marker, not a partial turn. Must be kept, no integrity issue.
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

    /// Regression: a resume's leading `AgentStart{resumed_from_seq}` that
    /// arrives before any new turn is a lifecycle marker, not a partial turn.
    /// Must be kept (matches the user-reported bug where `corrupted.log`
    /// showed a lone `AgentStart` being dropped as `PartialTurn`).
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
}
