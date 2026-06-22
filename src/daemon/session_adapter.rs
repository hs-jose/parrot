use parrot_core::event_log::StreamEvent;
use parrot_protocol::ServerMessage;
use uuid::Uuid;

pub struct SessionAdapter;

impl SessionAdapter {
    pub fn stream_event_to_server(session_id: Uuid, event: StreamEvent) -> ServerMessage {
        match event {
            StreamEvent::TextDelta { delta } => ServerMessage::TextDelta { session_id, delta },
            StreamEvent::ToolCallStart { id, name } => ServerMessage::ToolCallStart {
                session_id,
                tool_id: id,
                tool_name: name,
            },
            StreamEvent::ToolCallDelta { id, args_delta } => ServerMessage::ToolCallDelta {
                session_id,
                tool_id: id,
                args_delta,
            },
            StreamEvent::ToolCallEnd { id, arguments } => ServerMessage::ToolCallEnd {
                session_id,
                tool_id: id,
                arguments,
            },
            StreamEvent::ToolResult { id, result } => ServerMessage::ToolResult {
                session_id,
                tool_id: id,
                result: parrot_protocol::types::ToolOutput {
                    content: result.content,
                    is_error: result.is_error,
                },
            },
            StreamEvent::Finish { stop_reason, usage } => ServerMessage::Finished {
                session_id,
                stop_reason,
                usage,
            },
            StreamEvent::ToolCallConfirmationRequired {
                tool_id,
                tool_name,
                arguments,
            } => ServerMessage::ToolCallConfirmationRequired {
                session_id,
                tool_id,
                tool_name,
                arguments,
            },
        }
    }
}
