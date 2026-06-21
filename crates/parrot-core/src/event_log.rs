use serde::{Deserialize, Serialize};
use crate::tool::ToolOutput;
use parrot_protocol::types::{StopReason, Usage};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StreamEvent {
    TextDelta { delta: String },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, args_delta: String },
    ToolCallEnd { id: String, arguments: serde_json::Value },
    ToolResult { id: String, result: ToolOutput },
    Finish { stop_reason: StopReason, usage: Usage },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum EventLogEntry {
    SessionCreated { model: String, provider: String },
    UserMessage { content: String },
    AssistantText { content: String },
    ToolCall { tool_id: String, tool_name: String, arguments: serde_json::Value },
    ToolResult { tool_id: String, output: ToolOutput },
    Finish { stop_reason: StopReason, usage: Usage },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogEntryWithMeta {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub entry: EventLogEntry,
}

pub struct EventLog {
    dir: std::path::PathBuf,
    current_seq: u64,
}

impl EventLog {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self { dir, current_seq: 0 }
    }

    pub fn append(&mut self, entry: EventLogEntry) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("events.log");
        let meta = EventLogEntryWithMeta {
            seq: self.current_seq,
            ts: chrono::Utc::now(),
            entry,
        };
        let mut line = serde_json::to_string(&meta).map_err(|e| {
            std::io::Error::other(e)
        })?;
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
}