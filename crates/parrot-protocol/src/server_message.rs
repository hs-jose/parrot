use crate::agent_event::{AgentEvent, PersistedAgentEvent};
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
    SessionResumed {
        session_id: SessionId,
    },
    SessionList {
        sessions: Vec<SessionMeta>,
    },
    ModelList {
        models: Vec<ModelInfo>,
    },
    /// Receipt ack for `ClientMessage::Model`: the request was received. The
    /// engine applies the switch at the next turn boundary; a request that
    /// arrives while a turn is in flight is ignored. This ack therefore does
    /// not mean the active model has changed.
    ModelSet {
        session_id: SessionId,
        model: String,
    },
    ToolList {
        session_id: SessionId,
        tools: Vec<ToolDefinitionWire>,
    },
    History {
        session_id: SessionId,
        events: Vec<PersistedAgentEvent>,
    },
    ShellResult {
        session_id: SessionId,
        output: String,
        exit_code: i32,
    },

    /// MCP server 状态变更（daemon 主动推送，broadcast 转发）。
    McpNotice {
        id: String,
        state: McpServerState,
        detail: String,
        tool_count: u32,
    },
    /// `ClientMessage::ListMcpServers` 的应答。
    McpServers {
        entries: Vec<McpServerStatusWire>,
    },

    AgentEvent {
        event: AgentEvent,
    },

    Error {
        session_id: Option<SessionId>,
        code: ErrorCode,
        message: String,
    },
}
