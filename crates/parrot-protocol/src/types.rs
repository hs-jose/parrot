use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type SessionId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ErrorCode {
    AuthFailed,
    SessionNotFound,
    ProviderError,
    ToolError,
    InvalidRequest,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionConfig {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Wire-format model descriptor, returned by `ListModels` and embedded in
/// `ServerMessage::ModelList`. Mirrors `parrot_core::types::ModelInfo` (the
/// canonical trait return type) field-for-field; daemon converts at the
/// boundary. Kept duplicated so `parrot-protocol` stays free of `parrot-core`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// One event in a session's append-only event log. Persisted to
/// `{data_dir}/sessions/{id}/events.log` as one JSON line per entry (wrapped
/// in `EventLogEntryWithMeta`), and replayed both for `GetHistory` responses
/// and for session recovery.
///
/// Tagged `#[serde(tag = "type")]` so each serialized line carries its variant
/// name at the top level — `events.log` is human-greppable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum EventLogEntry {
    SessionCreated {
        model: String,
        provider: String,
    },
    UserMessage {
        content: String,
    },
    AssistantText {
        content: String,
    },
    ToolCall {
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_id: String,
        output: ToolOutput,
    },
    Finish {
        stop_reason: StopReason,
        usage: Usage,
    },
}


/// An `EventLogEntry` plus the metadata required to reconstruct ordering and
/// timing on replay. This is the unit that's both:
///   - persisted to `events.log` (one JSON line per entry), and
///   - sent over the wire in `ServerMessage::History`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventLogEntryWithMeta {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub entry: EventLogEntry,
}

/// Client's decision on a `ToolCallConfirmationRequired` request. See
/// `2026-06-21-parrot-phase-1.5.md` §4 for the full flow.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// Client user approved the tool call.
    Approve,
    /// Client user rejected the tool call.
    Reject,
    /// Client did not respond within the daemon's confirmation timeout.
    /// Daemon-side timeout also produces this; clients should only send
    /// `Approve` or `Reject` (sending `Timeout` from a client is allowed but
    /// redundant).
    Timeout,
}

/// Session metadata returned in `ServerMessage::SessionList`. Mirrors the
/// daemon's `SessionMeta` (in `src/daemon/session_store.rs`) minus the
/// `system_prompt` field — that's sensitive and clients don't need it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub model: String,
    pub provider: String,
    pub title: Option<String>,
    pub total_tokens: u64,
}

/// Tool definition in wire format, returned in `ServerMessage::ToolList`.
/// Mirrors `parrot_core::tool::ToolDefinition` field-for-field; daemon
/// converts at the boundary. Independent definition so `parrot-protocol`
/// stays free of `parrot-core`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinitionWire {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
