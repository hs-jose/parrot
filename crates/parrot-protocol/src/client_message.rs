use serde::{Deserialize, Serialize};
use crate::types::{SessionConfig, SessionId};

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
    ListModels,
    ListTools {
        session_id: SessionId,
    },
    GetHistory {
        session_id: SessionId,
    },
}