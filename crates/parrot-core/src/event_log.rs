use crate::tool::ToolOutput;
use parrot_protocol::types::{StopReason, Usage};
use serde::{Deserialize, Serialize};

// `EventLogEntry` and `EventLogEntryWithMeta` are defined in `parrot-protocol`
// because they are pure serde data used both on the wire (`ServerMessage::History`)
// and on disk (`events.log`). Re-export here so existing callers
// (`crate::engine`, `crate::session`) can keep using `crate::event_log::*`.
pub use parrot_protocol::types::{EventLogEntry, EventLogEntryWithMeta};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StreamEvent {
    TextDelta {
        delta: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        args_delta: String,
    },
    ToolCallEnd {
        id: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        id: String,
        result: ToolOutput,
    },
    Finish {
        stop_reason: StopReason,
        usage: Usage,
    },
    /// Phase 1.5: a tool call matched `require_confirmation` and the engine
    /// is now blocking on `ConfirmRouter` for the client's decision. The
    /// daemon's session adapter converts this to
    /// `ServerMessage::ToolCallConfirmationRequired`. The engine does NOT
    /// emit a `ToolResult` until a decision arrives (or the timeout fires).
    ToolCallConfirmationRequired {
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
}

pub struct EventLog {
    dir: std::path::PathBuf,
    current_seq: u64,
}

impl EventLog {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            current_seq: 0,
        }
    }

    /// Current sequence number (number of entries appended so far).
    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }

    pub fn append(&mut self, entry: EventLogEntry) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("events.log");
        let meta = EventLogEntryWithMeta {
            seq: self.current_seq,
            ts: chrono::Utc::now(),
            entry,
        };
        let mut line = serde_json::to_string(&meta).map_err(std::io::Error::other)?;
        line.push('\n');
        use std::io::Write;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(line.as_bytes())?;
        self.current_seq += 1;
        Ok(())
    }

    pub fn replay(&self) -> std::io::Result<Vec<EventLogEntryWithMeta>> {
        let path = self.dir.join("events.log");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut entries = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<EventLogEntryWithMeta>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!("Failed to parse event log line: {}", e);
                }
            }
        }
        Ok(entries)
    }

    pub fn write_snapshot(&self, snapshot: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("snapshot.json");
        std::fs::write(path, snapshot)
    }

    /// Write a snapshot of the current context if `current_seq` has crossed a
    /// multiple of `SNAPSHOT_INTERVAL`. Called by the engine after each
    /// `Finish` event so a crash never loses more than ~`SNAPSHOT_INTERVAL`
    /// events of replay work. Single-file overwrite for MVP (rotation to
    /// keep the last 3 is a follow-up per design doc §7).
    pub fn maybe_snapshot(&self, context: &[crate::types::ChatMessage]) -> std::io::Result<()> {
        const SNAPSHOT_INTERVAL: u64 = 100;
        if self.current_seq == 0 || !self.current_seq.is_multiple_of(SNAPSHOT_INTERVAL) {
            return Ok(());
        }
        let json = serde_json::to_string_pretty(context).map_err(std::io::Error::other)?;
        self.write_snapshot(&json)?;
        tracing::info!(seq = self.current_seq, "wrote session snapshot");
        Ok(())
    }
}
