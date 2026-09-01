use crate::types::{SessionId, ToolOutput, Usage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 统一的 Agent 事件流。
///
/// 三层生命周期，严格嵌套：
///
/// ```text
/// AgentStart
///   TurnStart
///     MessageStart
///       MessageDelta*    (高频，不持久化)
///     MessageEnd         (final_content 完整，供 replay 重建 context)
///     ToolStart          (parent_message_id 关联到上面的 message)
///       ToolUpdate*      (高频，不持久化，长任务进度)
///       ToolConfirmRequired? (可选，仅当工具命中 require_confirmation)
///     ToolEnd
///     // (ToolEnd 后 ReAct 可能进入下一轮 MessageStart...MessageEnd)
///   TurnEnd
/// AgentEnd
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AgentEvent {
    AgentStart {
        session_id: SessionId,
        model: String,
        provider: String,
        system_prompt_hash: String,
        resumed_from_seq: Option<u64>,
    },

    AgentEnd {
        session_id: SessionId,
        reason: AgentEndReason,
        total_usage: Usage,
    },

    TurnStart {
        session_id: SessionId,
        turn_id: Uuid,
        user_message: String,
    },

    TurnEnd {
        session_id: SessionId,
        turn_id: Uuid,
        stop_reason: TurnStopReason,
        usage: Usage,
    },

    MessageStart {
        session_id: SessionId,
        turn_id: Uuid,
        message_id: Uuid,
    },

    MessageDelta {
        session_id: SessionId,
        message_id: Uuid,
        payload: MessageDeltaPayload,
    },

    MessageEnd {
        session_id: SessionId,
        turn_id: Uuid,
        message_id: Uuid,
        final_content: String,
        tool_calls: Vec<ToolCallInfo>,
        stop_reason: MessageStopReason,
        usage: Usage,
    },

    ToolStart {
        session_id: SessionId,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    ToolUpdate {
        session_id: SessionId,
        tool_call_id: String,
        partial: ToolPartial,
    },

    ToolEnd {
        session_id: SessionId,
        turn_id: Uuid,
        tool_call_id: String,
        result: ToolOutput,
    },

    ToolConfirmRequired {
        session_id: SessionId,
        turn_id: Uuid,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    ReplayIntegrityWarning {
        session_id: SessionId,
        issue: IntegrityIssue,
    },

    HookFired {
        session_id: SessionId,
        hook_id: String,
        event_kind: String,
        result_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },

    /// Context compaction starting: a summary LLM call is in flight. UI-only
    /// notification (not persisted); `CompactionSummary` follows on success.
    CompactionStart {
        session_id: SessionId,
        turn_id: Uuid,
    },

    /// Context compaction applied: pre-cut-point history was replaced by a
    /// structured summary (marker-prefixed). Emitted BEFORE the triggering
    /// turn's `TurnStart`. Replayed by `rebuild_context` with message-count
    /// semantics: keep the last `kept_message_count` rebuilt messages, then
    /// push this summary as a User message.
    CompactionSummary {
        session_id: SessionId,
        turn_id: Uuid,
        summary: String,
        dropped_message_count: u32,
        kept_message_count: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum MessageDeltaPayload {
    TextDelta {
        delta: String,
    },
    ToolCallStart {
        tool_call_id: String,
        tool_name: String,
    },
    ToolCallArgsDelta {
        tool_call_id: String,
        args_delta: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolPartial {
    pub kind: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallInfo {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TurnStopReason {
    EndTurn,
    MaxTokens,
    MaxIterations,
    Aborted,
    Error(String),
    BlockedHook(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentEndReason {
    ClientClose,
    ClientDisconnect,
    DaemonShutdown,
    FatalError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntegrityIssue {
    pub kind: IntegrityIssueKind,
    pub dropped_event_count: u32,
    pub first_dropped_seq: u64,
    pub last_dropped_seq: u64,
    pub dangling_turn_ids: Vec<Uuid>,
    pub dangling_message_ids: Vec<Uuid>,
    pub dangling_tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IntegrityIssueKind {
    PartialTurn,
    EventsAfterAgentEnd,
}

/// An `AgentEvent` plus the metadata required to reconstruct ordering and
/// timing on replay. This is the unit that's both:
///   - persisted to `events.log` (one JSON line per entry), and
///   - sent over the wire in `ServerMessage::History`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedAgentEvent {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub event: AgentEvent,
}

impl AgentEvent {
    pub fn is_persistent(&self) -> bool {
        !matches!(
            self,
            Self::MessageDelta { .. }
                | Self::ToolUpdate { .. }
                | Self::HookFired { .. }
                | Self::CompactionStart { .. }
        )
    }

    pub fn session_id(&self) -> SessionId {
        match self {
            Self::AgentStart { session_id, .. }
            | Self::AgentEnd { session_id, .. }
            | Self::TurnStart { session_id, .. }
            | Self::TurnEnd { session_id, .. }
            | Self::MessageStart { session_id, .. }
            | Self::MessageDelta { session_id, .. }
            | Self::MessageEnd { session_id, .. }
            | Self::ToolStart { session_id, .. }
            | Self::ToolUpdate { session_id, .. }
            | Self::ToolEnd { session_id, .. }
            | Self::ToolConfirmRequired { session_id, .. }
            | Self::ReplayIntegrityWarning { session_id, .. }
            | Self::HookFired { session_id, .. }
            | Self::CompactionStart { session_id, .. }
            | Self::CompactionSummary { session_id, .. } => *session_id,
        }
    }
}
