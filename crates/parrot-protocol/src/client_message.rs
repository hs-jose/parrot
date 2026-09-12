use crate::types::{ConfirmDecision, SessionConfig, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Hello {
        token: String,
        client_version: String,
    },
    CreateSession {
        config: Option<SessionConfig>,
    },
    Chat {
        session_id: SessionId,
        message: String,
    },
    Abort {
        session_id: SessionId,
    },
    /// Switch the active model of a running session at runtime. Response:
    /// `ServerMessage::ModelSet` or `Error{SessionNotFound}`.
    Model {
        session_id: SessionId,
        model: String,
    },
    ListModels,
    ListTools {
        session_id: SessionId,
    },
    GetHistory {
        session_id: SessionId,
    },
    /// List all sessions known to the daemon (reads `index.json`).
    /// Response: `ServerMessage::SessionList`.
    ListSessions,
    /// Resume a previously-persisted session: daemon replays the event log,
    /// rebuilds the in-memory context, and spawns a new engine task. Response:
    /// `ServerMessage::SessionResumed` or `Error{SessionNotFound}`.
    ResumeSession {
        session_id: SessionId,
    },
    /// Response to `ServerMessage::ToolCallConfirmationRequired`. The daemon
    /// routes this to the waiting session task via `ConfirmRouter`.
    ConfirmToolCall {
        session_id: SessionId,
        tool_id: String,
        decision: ConfirmDecision,
    },
    /// Execute a shell command on the daemon (the `!cmd` feature). Runs
    /// outside the engine and never enters the session context. Response:
    /// `ServerMessage::ShellResult`.
    Shell {
        session_id: SessionId,
        command: String,
    },
    /// 查询 MCP server 状态。Response: `ServerMessage::McpServers`。
    ListMcpServers,
}
