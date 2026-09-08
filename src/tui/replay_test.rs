//! TUI replay tests: load a session event log from a JSON fixture and
//! replay it through the `App` state machine, verifying that entries,
//! modes, and error display work correctly without needing a daemon.
//!
//! Fixtures are `Vec<PersistedAgentEvent>` JSON files under
//! `tests/tui_replay/`. They can be created from real sessions via:
//!   parrot sessions export <session_id> > tests/tui_replay/my_fixture.json

use parrot_protocol::agent_event::PersistedAgentEvent;
use parrot_protocol::SessionId;
use std::path::PathBuf;

use crate::tui::app::{App, ChatEntry};

fn load_fixture(name: &str) -> Vec<PersistedAgentEvent> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "tui_replay",
        &format!("{name}.json"),
    ]
    .iter()
    .collect();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {name} at {path:?}: {e}"));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("failed to parse fixture {name}: {e}"))
}

fn replay(app: &mut App, events: &[PersistedAgentEvent]) {
    for ev in events {
        app.apply_event(ev.event.clone());
    }
}

#[test]
fn replay_multi_turn_session() {
    let events = load_fixture("multi_turn_session");
    let sid: SessionId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
    let mut app = App::new(sid);
    replay(&mut app, &events);

    // 8 entries: User, Assistant, User, Assistant, Tool, Assistant, User, Error
    assert_eq!(
        app.entries.len(),
        8,
        "expected 8 entries, got {:?}",
        app.entries
    );

    // Turn 1: user "hello" → assistant "Hi there!\n..."
    assert!(matches!(&app.entries[0], ChatEntry::User { ref text, .. } if text == "hello"));
    assert!(
        matches!(&app.entries[1], ChatEntry::Assistant { ref text, .. } if text.contains("Hi there!"))
    );

    // Turn 2: user → assistant → tool → assistant
    assert!(
        matches!(&app.entries[2], ChatEntry::User { ref text, .. } if text.contains("list files"))
    );
    assert!(
        matches!(&app.entries[3], ChatEntry::Assistant { ref text, .. } if text.contains("Let me check"))
    );
    match &app.entries[4] {
        ChatEntry::Tool {
            tool_name, result, ..
        } => {
            assert_eq!(tool_name, "file_list");
            assert!(result.is_some(), "tool result should be backfilled");
            assert!(!result.as_ref().unwrap().is_error);
            assert!(result.as_ref().unwrap().content.contains("file1.txt"));
        }
        other => panic!("expected Tool entry, got {:?}", other),
    }
    assert!(
        matches!(&app.entries[5], ChatEntry::Assistant { ref text, .. } if text.contains("Found 3 files"))
    );

    // Turn 3: user → error (no assistant message, TurnEnd has Error)
    assert!(
        matches!(&app.entries[6], ChatEntry::User { ref text, .. } if text == "delete everything")
    );
    assert!(matches!(&app.entries[7], ChatEntry::Error(s) if s.contains("invalid api key")));

    // AgentEnd{ClientClose} → ended=true
    assert!(app.ended);
}

#[test]
fn replay_multi_line_assistant_preserved() {
    let events = load_fixture("multi_turn_session");
    let sid: SessionId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
    let mut app = App::new(sid);
    replay(&mut app, &events);

    // The first assistant response has a newline; verify it's preserved.
    match &app.entries[1] {
        ChatEntry::Assistant { text, .. } => {
            assert!(
                text.contains('\n'),
                "multi-line text should preserve newlines"
            );
            assert_eq!(text, "Hi there!\nHow can I help you today?");
        }
        other => panic!("expected Assistant, got {:?}", other),
    }
}

#[test]
fn replay_no_streaming_text_after_flush() {
    let events = load_fixture("multi_turn_session");
    let sid: SessionId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
    let mut app = App::new(sid);
    replay(&mut app, &events);

    // After replaying all events, no in-progress streaming text should remain.
    assert!(app.streaming_text().is_none() || app.streaming_text().unwrap().is_empty());
}

#[test]
fn replay_tool_entries_default_compact() {
    let events = load_fixture("multi_turn_session");
    let sid: SessionId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
    let mut app = App::new(sid);
    replay(&mut app, &events);

    match &app.entries[4] {
        ChatEntry::Tool { expanded, .. } => {
            assert!(!expanded, "History replay 重建的工具条目应默认紧凑");
        }
        other => panic!("expected Tool entry, got {:?}", other),
    }
}
