use parrot_protocol::types::{
    ConfirmDecision, EventLogEntry, EventLogEntryWithMeta, ModelInfo, SessionMeta, StopReason,
    ToolDefinitionWire, ToolOutput, Usage,
};
use parrot_protocol::*;

#[test]
fn client_message_hello_roundtrip() {
    let msg = ClientMessage::Hello {
        token: "test-token".into(),
        client_version: "0.1.0".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn client_chat_roundtrip() {
    let msg = ClientMessage::Chat {
        session_id: uuid::Uuid::new_v4(),
        message: "read the file".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn server_text_delta_roundtrip() {
    let msg = ServerMessage::TextDelta {
        session_id: uuid::Uuid::new_v4(),
        delta: "hello".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn server_error_roundtrip() {
    let msg = ServerMessage::Error {
        session_id: None,
        code: ErrorCode::AuthFailed,
        message: "invalid token".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn tool_call_end_roundtrip() {
    let msg = ServerMessage::ToolCallEnd {
        session_id: uuid::Uuid::new_v4(),
        tool_id: "tc_1".into(),
        arguments: serde_json::json!({"path": "/tmp/test.rs"}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn serde_tag_format() {
    let msg = ClientMessage::Hello {
        token: "abc".into(),
        client_version: "1.0".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains(r#""type":"Hello""#),
        "Expected tagged enum, got: {json}"
    );
}

#[test]
fn tool_result_roundtrip() {
    let msg = ServerMessage::ToolResult {
        session_id: uuid::Uuid::new_v4(),
        tool_id: "tc_1".into(),
        result: ToolOutput {
            content: "file contents here".into(),
            is_error: false,
        },
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn model_list_roundtrip() {
    let msg = ServerMessage::ModelList {
        models: vec![
            ModelInfo {
                id: "claude-sonnet-4-6".into(),
                name: "Claude Sonnet 4.6".into(),
                provider: "anthropic".into(),
                context_window: 200_000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-haiku-3-5".into(),
                name: "Claude Haiku 3.5".into(),
                provider: "anthropic".into(),
                context_window: 200_000,
                max_output_tokens: 8192,
            },
        ],
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
    // Verify the tagged enum format
    assert!(
        json.contains(r#""type":"ModelList""#),
        "expected ModelList tag in: {json}"
    );
}

#[test]
fn history_roundtrip() {
    let session_id = uuid::Uuid::new_v4();
    let entries = vec![
        EventLogEntryWithMeta {
            seq: 0,
            ts: "2026-06-21T10:00:00Z".parse().unwrap(),
            entry: EventLogEntry::SessionCreated {
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
            },
        },
        EventLogEntryWithMeta {
            seq: 1,
            ts: "2026-06-21T10:00:01Z".parse().unwrap(),
            entry: EventLogEntry::UserMessage {
                content: "hello".into(),
            },
        },
        EventLogEntryWithMeta {
            seq: 2,
            ts: "2026-06-21T10:00:05Z".parse().unwrap(),
            entry: EventLogEntry::Finish {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            },
        },
    ];
    let msg = ServerMessage::History {
        session_id,
        entries,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
    // Verify the tagged enum format and that entries retain their variant tags
    assert!(
        json.contains(r#""type":"History""#),
        "expected History tag in: {json}"
    );
    assert!(
        json.contains(r#""type":"SessionCreated""#),
        "expected SessionCreated tag in: {json}"
    );
    assert!(
        json.contains(r#""type":"UserMessage""#),
        "expected UserMessage tag in: {json}"
    );
    assert!(
        json.contains(r#""type":"Finish""#),
        "expected Finish tag in: {json}"
    );
}

#[test]
fn event_log_entry_tagged_format() {
    // Verify that EventLogEntry serializes with the "type" tag at top level,
    // matching the events.log on-disk format.
    let entry = EventLogEntry::ToolCall {
        tool_id: "tc_1".into(),
        tool_name: "file_read".into(),
        arguments: serde_json::json!({"path": "src/lib.rs"}),
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(
        json.contains(r#""type":"ToolCall""#),
        "expected ToolCall tag in: {json}"
    );
    assert!(
        json.contains(r#""tool_id":"tc_1""#),
        "expected tool_id field in: {json}"
    );

    let decoded: EventLogEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, decoded);
}

// ---------------------------------------------------------------------------
// Phase 1.5 protocol additions
// ---------------------------------------------------------------------------

#[test]
fn list_sessions_roundtrip() {
    let msg = ClientMessage::ListSessions;
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"ListSessions""#));
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn resume_session_roundtrip() {
    let id = uuid::Uuid::new_v4();
    let msg = ClientMessage::ResumeSession { session_id: id };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"ResumeSession""#));
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn confirm_tool_call_roundtrip() {
    let id = uuid::Uuid::new_v4();
    let msg = ClientMessage::ConfirmToolCall {
        session_id: id,
        tool_id: "tc_1".into(),
        decision: ConfirmDecision::Approve,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"ConfirmToolCall""#));
    assert!(json.contains(r#""decision":"Approve""#));
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);

    // Reject variant
    let msg = ClientMessage::ConfirmToolCall {
        session_id: id,
        tool_id: "tc_2".into(),
        decision: ConfirmDecision::Reject,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""decision":"Reject""#));
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn session_list_roundtrip() {
    let now = "2026-06-21T10:00:00Z".parse().unwrap();
    let meta = SessionMeta {
        id: uuid::Uuid::new_v4(),
        created_at: now,
        updated_at: now,
        model: "claude-sonnet-4-6".into(),
        provider: "anthropic".into(),
        title: Some("重构 utils.rs".into()),
        total_tokens: 15000,
    };
    let msg = ServerMessage::SessionList {
        sessions: vec![meta],
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"SessionList""#));
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn session_resumed_roundtrip() {
    let id = uuid::Uuid::new_v4();
    let msg = ServerMessage::SessionResumed { session_id: id };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"SessionResumed""#));
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn tool_call_confirmation_required_roundtrip() {
    let id = uuid::Uuid::new_v4();
    let msg = ServerMessage::ToolCallConfirmationRequired {
        session_id: id,
        tool_id: "tc_7".into(),
        tool_name: "shell_exec".into(),
        arguments: serde_json::json!({"command": "rm -rf /tmp/old"}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"ToolCallConfirmationRequired""#));
    assert!(json.contains(r#""tool_name":"shell_exec""#));
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn tool_list_roundtrip() {
    let id = uuid::Uuid::new_v4();
    let tools = vec![ToolDefinitionWire {
        name: "file_read".into(),
        description: "Read a file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    }];
    let msg = ServerMessage::ToolList {
        session_id: id,
        tools,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains(r#""type":"ToolList""#));
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}
