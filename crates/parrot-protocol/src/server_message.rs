use crate::types::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    HelloAck {
        server_version: String,
    },
    SessionCreated {
        session_id: SessionId,
    },
    TextDelta {
        session_id: SessionId,
        delta: String,
    },
    ToolCallStart {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
    },
    ToolCallDelta {
        session_id: SessionId,
        tool_id: String,
        args_delta: String,
    },
    ToolCallEnd {
        session_id: SessionId,
        tool_id: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        session_id: SessionId,
        tool_id: String,
        result: ToolOutput,
    },
    Finished {
        session_id: SessionId,
        stop_reason: StopReason,
        usage: Usage,
    },
    Error {
        session_id: Option<SessionId>,
        code: ErrorCode,
        message: String,
    },
    /// Response to `ClientMessage::ListModels`: aggregated models across all
    /// registered providers.
    ModelList {
        models: Vec<ModelInfo>,
    },
    /// Response to `ClientMessage::GetHistory`: replayed event log for a session.
    History {
        session_id: SessionId,
        entries: Vec<EventLogEntryWithMeta>,
    },
    /// Response to `ClientMessage::ListSessions`: all sessions known to the
    /// daemon (read from `index.json`).
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    /// Response to `ClientMessage::ResumeSession`: session was successfully
    /// resumed — the client can now send `Chat` commands to it.
    SessionResumed {
        session_id: SessionId,
    },
    /// Daemon-initiated: a tool call matched `require_confirmation` and needs
    /// the client user's approval before executing. The daemon waits up to
    /// its configured confirm timeout for `ClientMessage::ConfirmToolCall`.
    /// If no response arrives, the daemon treats it as `ConfirmDecision::Timeout`
    /// and emits a `ToolResult{is_error: true}`.
    ToolCallConfirmationRequired {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// Response to `ClientMessage::ListTools`: the daemon's registered tool
    /// definitions. Replaces the previous `TextDelta`+`Finished` hack used
    /// to ferry tool schemas back to the client.
    ToolList {
        session_id: SessionId,
        tools: Vec<ToolDefinitionWire>,
    },
}
