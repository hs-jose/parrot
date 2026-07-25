use parrot_protocol::agent_event::{
    AgentEndReason, AgentEvent, IntegrityIssue, IntegrityIssueKind, MessageDeltaPayload,
    MessageStopReason, PersistedAgentEvent, ToolCallInfo, ToolPartial, TurnStopReason,
};
use parrot_protocol::types::{
    ConfirmDecision, ModelInfo, SessionMeta, ToolDefinitionWire, ToolOutput, Usage,
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
    assert!(
        json.contains(r#""type":"ModelList""#),
        "expected ModelList tag in: {json}"
    );
}

// ---------------------------------------------------------------------------
// AgentEvent roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn agent_event_roundtrip_all_variants() {
    let sid = uuid::Uuid::new_v4();
    let turn_id = uuid::Uuid::new_v4();
    let msg_id = uuid::Uuid::new_v4();

    let variants = vec![
        AgentEvent::AgentStart {
            session_id: sid,
            model: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            system_prompt_hash: "a3f9e1b2c4d5f6a7".into(),
            resumed_from_seq: None,
        },
        AgentEvent::AgentEnd {
            session_id: sid,
            reason: AgentEndReason::ClientClose,
            total_usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
            },
        },
        AgentEvent::TurnStart {
            session_id: sid,
            turn_id,
            user_message: "hello".into(),
        },
        AgentEvent::TurnEnd {
            session_id: sid,
            turn_id,
            stop_reason: TurnStopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        },
        AgentEvent::MessageStart {
            session_id: sid,
            turn_id,
            message_id: msg_id,
        },
        AgentEvent::MessageDelta {
            session_id: sid,
            message_id: msg_id,
            payload: MessageDeltaPayload::TextDelta { delta: "hi".into() },
        },
        AgentEvent::MessageDelta {
            session_id: sid,
            message_id: msg_id,
            payload: MessageDeltaPayload::ToolCallStart {
                tool_call_id: "tc_1".into(),
                tool_name: "echo".into(),
            },
        },
        AgentEvent::MessageDelta {
            session_id: sid,
            message_id: msg_id,
            payload: MessageDeltaPayload::ToolCallArgsDelta {
                tool_call_id: "tc_1".into(),
                args_delta: r#"{"msg":"hi"}"#.into(),
            },
        },
        AgentEvent::MessageEnd {
            session_id: sid,
            turn_id,
            message_id: msg_id,
            final_content: "I'll echo.".into(),
            tool_calls: vec![ToolCallInfo {
                tool_call_id: "tc_1".into(),
                tool_name: "echo".into(),
                arguments: serde_json::json!({"msg": "hi"}),
            }],
            stop_reason: MessageStopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        },
        AgentEvent::ToolStart {
            session_id: sid,
            turn_id,
            parent_message_id: msg_id,
            tool_call_id: "tc_1".into(),
            tool_name: "echo".into(),
            arguments: serde_json::json!({"msg": "hi"}),
        },
        AgentEvent::ToolUpdate {
            session_id: sid,
            tool_call_id: "tc_1".into(),
            partial: ToolPartial {
                kind: "stdout_line".into(),
                content: serde_json::json!("hello"),
            },
        },
        AgentEvent::ToolEnd {
            session_id: sid,
            turn_id,
            tool_call_id: "tc_1".into(),
            result: ToolOutput {
                content: "echo: hi".into(),
                is_error: false,
            },
        },
        AgentEvent::ToolConfirmRequired {
            session_id: sid,
            turn_id,
            tool_call_id: "tc_1".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        },
        AgentEvent::ReplayIntegrityWarning {
            session_id: sid,
            issue: IntegrityIssue {
                kind: IntegrityIssueKind::PartialTurn,
                dropped_event_count: 3,
                first_dropped_seq: 42,
                last_dropped_seq: 44,
                dangling_turn_ids: vec![uuid::Uuid::new_v4()],
                dangling_message_ids: vec![uuid::Uuid::new_v4()],
                dangling_tool_call_ids: vec!["tc_1".into()],
            },
        },
    ];

    for ev in &variants {
        let json = serde_json::to_string(ev).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, &decoded, "roundtrip failed for: {json}");
    }
}

#[test]
fn is_persistent_filter() {
    let sid = uuid::Uuid::new_v4();
    let msg_id = uuid::Uuid::new_v4();

    let persistent = AgentEvent::AgentStart {
        session_id: sid,
        model: "m".into(),
        provider: "p".into(),
        system_prompt_hash: "h".into(),
        resumed_from_seq: None,
    };
    assert!(persistent.is_persistent());

    let delta = AgentEvent::MessageDelta {
        session_id: sid,
        message_id: msg_id,
        payload: MessageDeltaPayload::TextDelta { delta: "x".into() },
    };
    assert!(!delta.is_persistent());

    let tool_update = AgentEvent::ToolUpdate {
        session_id: sid,
        tool_call_id: "tc".into(),
        partial: ToolPartial {
            kind: "k".into(),
            content: serde_json::json!(1),
        },
    };
    assert!(!tool_update.is_persistent());

    let turn_end = AgentEvent::TurnEnd {
        session_id: sid,
        turn_id: uuid::Uuid::new_v4(),
        stop_reason: TurnStopReason::EndTurn,
        usage: Usage::default(),
    };
    assert!(turn_end.is_persistent());
}

#[test]
fn agent_event_session_id_accessor() {
    let sid = uuid::Uuid::new_v4();
    let ev = AgentEvent::AgentStart {
        session_id: sid,
        model: "m".into(),
        provider: "p".into(),
        system_prompt_hash: "h".into(),
        resumed_from_seq: None,
    };
    assert_eq!(ev.session_id(), sid);
}

#[test]
fn server_message_agent_event_envelope_roundtrip() {
    let sid = uuid::Uuid::new_v4();
    let inner = AgentEvent::TurnStart {
        session_id: sid,
        turn_id: uuid::Uuid::new_v4(),
        user_message: "hi".into(),
    };
    let msg = ServerMessage::AgentEvent { event: inner };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains(r#""type":"AgentEvent""#),
        "expected AgentEvent tag in: {json}"
    );
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn history_roundtrip() {
    let session_id = uuid::Uuid::new_v4();
    let events = vec![
        PersistedAgentEvent {
            seq: 0,
            ts: "2026-06-22T10:00:00Z".parse().unwrap(),
            event: AgentEvent::AgentStart {
                session_id,
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                system_prompt_hash: "a3f9".into(),
                resumed_from_seq: None,
            },
        },
        PersistedAgentEvent {
            seq: 1,
            ts: "2026-06-22T10:00:01Z".parse().unwrap(),
            event: AgentEvent::TurnStart {
                session_id,
                turn_id: uuid::Uuid::new_v4(),
                user_message: "hello".into(),
            },
        },
    ];
    let msg = ServerMessage::History { session_id, events };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
    assert!(
        json.contains(r#""type":"History""#),
        "expected History tag in: {json}"
    );
    assert!(
        json.contains(r#""type":"AgentStart""#),
        "expected AgentStart tag in: {json}"
    );
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

#[test]
fn hook_fired_roundtrip() {
    let sid = uuid::Uuid::new_v4();
    let event = AgentEvent::HookFired {
        session_id: sid,
        hook_id: "dangerous_command_blocker".into(),
        event_kind: "tool_call".into(),
        result_kind: "block".into(),
        summary: Some("dangerous command: rm -rf /".into()),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(
        json.contains(r#""type":"HookFired""#),
        "expected HookFired tag in: {json}"
    );
    let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, decoded);
}

#[test]
fn hook_fired_no_summary_roundtrip() {
    let sid = uuid::Uuid::new_v4();
    let event = AgentEvent::HookFired {
        session_id: sid,
        hook_id: "redact_secrets".into(),
        event_kind: "tool_result".into(),
        result_kind: "replace_result".into(),
        summary: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, decoded);
    assert!(
        !json.contains(r#""summary""#),
        "None summary must be skipped"
    );
}

#[test]
fn turn_stop_reason_blocked_hook_roundtrip() {
    let reason = TurnStopReason::BlockedHook("policy".into());
    let json = serde_json::to_string(&reason).unwrap();
    assert!(
        json.contains(r#""BlockedHook""#),
        "expected BlockedHook tag in: {json}"
    );
    let decoded: TurnStopReason = serde_json::from_str(&json).unwrap();
    assert_eq!(reason, decoded);
}

#[test]
fn hook_action_replace_context_roundtrip() {
    use parrot_core::hooks::HookAction;
    use parrot_core::types::{ChatMessage, ChatRole};

    let action = HookAction::ReplaceContext {
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: "system prompt".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
        ],
    };
    let json = serde_json::to_string(&action).unwrap();
    assert!(
        json.contains(r#""kind":"replace_context""#),
        "expected replace_context tag in: {json}"
    );
    let decoded: HookAction = serde_json::from_str(&json).unwrap();
    assert_eq!(action, decoded);
}
