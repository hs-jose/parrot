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

    AgentEvent {
        event: AgentEvent,
    },

    Error {
        session_id: Option<SessionId>,
        code: ErrorCode,
        message: String,
    },
}
