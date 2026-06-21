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
    assert!(json.contains(r#""type":"Hello""#), "Expected tagged enum, got: {json}");
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