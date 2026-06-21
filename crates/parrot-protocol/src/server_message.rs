use serde::{Deserialize, Serialize};
use crate::types::*;

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
}