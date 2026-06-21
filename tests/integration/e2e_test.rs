use parrot_config::AppConfig;
use parrot_protocol::{ClientMessage, ServerMessage};

#[test]
fn test_config_loads() {
    // Use default config instead of loading from file (which requires ANTHROPIC_API_KEY)
    let config = AppConfig::default_config();
    assert_eq!(config.daemon.port, 9876);
    assert!(!config.tools.shell_allowed);
    assert!(!config.tools.file_write_allowed);
    assert!(config.tools.web_allowed);
}

#[test]
fn test_protocol_roundtrip() {
    let msg = ClientMessage::Hello {
        token: "test-token".into(),
        client_version: "1.0".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn test_server_message_roundtrip() {
    let msg = ServerMessage::Finished {
        session_id: uuid::Uuid::new_v4(),
        stop_reason: parrot_protocol::types::StopReason::EndTurn,
        usage: parrot_protocol::types::Usage { input_tokens: 100, output_tokens: 50 },
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}