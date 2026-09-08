use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
}

impl ChatMessage {
    /// 构造不带工具元数据的消息。工具字段需要时用结构体更新语法补上，
    /// 例如 `ChatMessage { tool_call_id: Some(id), ..ChatMessage::new(ChatRole::Tool, text) }`。
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl From<&parrot_protocol::agent_event::ToolCallInfo> for ToolCallInfo {
    fn from(tc: &parrot_protocol::agent_event::ToolCallInfo) -> Self {
        Self {
            id: tc.tool_call_id.clone(),
            name: tc.tool_name.clone(),
            arguments: tc.arguments.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerateConfig {
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
}

impl Default for GenerateConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
            temperature: None,
            max_tokens: Some(8192),
            stop_sequences: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}
