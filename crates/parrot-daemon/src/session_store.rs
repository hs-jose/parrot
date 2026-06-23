//! Session metadata persistence: `meta.json` per session + global `index.json`.
//!
//! Lives in the daemon (not `parrot-core`) because it's pure IO and the
//! design's "core = zero IO" rule says file IO belongs to the daemon. The
//! engine appends to `events.log` via `EventLog` (a pre-existing core IO
//! surface that's slated for later refactor); meta/index files are the
//! daemon's responsibility and are updated synchronously on session creation
//! and on each turn's `Finished` event.
//!
//! See design doc §7 for the file layout and §4.4 for the lifecycle triggers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub provider: String,
    pub title: Option<String>,
    /// Persisted so `ResumeSession` (Phase 1.5) can reconstruct the exact
    /// system prompt the session was started with, even across daemon
    /// restarts. `None` means "use daemon default".
    pub system_prompt: Option<String>,
    pub total_tokens: u64,
    pub last_snapshot_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionIndex {
    pub version: u32,
    pub sessions: Vec<SessionIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub id: Uuid,
    pub title: Option<String>,
    pub model: String,
    pub provider: String,
    /// When the session was first created. Phase 1.5 addition: the wire
    /// `SessionMeta` exposes `created_at`, and `ListSessions` reads only
    /// `index.json` (not each session's `meta.json`), so the index must
    /// carry `created_at` to populate the wire response without N extra
    /// file reads. Older index.json files (written before this field) are
    /// tolerated via `#[serde(default)]` — `updated_at` is used as a
    /// fallback (the read path falls back to `updated_at` when `created_at`
    /// equals the default epoch).
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub total_tokens: u64,
}

/// Manages `meta.json` per session and the global `index.json` under
/// `{data_dir}/sessions/`. All writes use the atomic
/// "write-to-tmp-then-rename" pattern so a crash never leaves a half-written
/// file — the design doc §7 explicitly calls this out.
pub struct SessionStore {
    /// Path to the sessions root, e.g. `{data_dir}/parrot/sessions`.
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Path to the sessions root directory (`{data_dir}/parrot/sessions/`).
    /// Exposed so the daemon can derive per-session paths (e.g. for
    /// `EventLog::replay` when handling `GetHistory`).
    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions_dir.clone()
    }

    /// Ensure the sessions directory exists. Call once at daemon startup.
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.sessions_dir)
    }

    /// Write the initial `meta.json` for a freshly-created session and add
    /// an entry to `index.json`. Idempotent on the meta file (a second call
    /// with the same id overwrites the meta — used to fix up missing fields).
    pub fn init_session(
        &self,
        id: Uuid,
        model: &str,
        provider: &str,
        system_prompt: Option<&str>,
    ) -> std::io::Result<SessionMeta> {
        self.ensure_dir()?;
        let session_dir = self.sessions_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;

        let now = Utc::now();
        let meta = SessionMeta {
            id,
            created_at: now,
            updated_at: now,
            model: model.to_string(),
            provider: provider.to_string(),
            title: None,
            system_prompt: system_prompt.map(str::to_string),
            total_tokens: 0,
            last_snapshot_seq: 0,
        };
        self.write_meta_atomic(&meta)?;
        self.upsert_index_entry(&meta)?;
        Ok(meta)
    }

    /// Read the `meta.json` for a session. Returns `Ok(None)` if the file
    /// doesn't exist (e.g. session was created before meta.json support was
    /// added, or the id is bogus).
    pub fn read_meta(&self, id: Uuid) -> std::io::Result<Option<SessionMeta>> {
        let path = self.meta_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<SessionMeta>(&content) {
            Ok(meta) => Ok(Some(meta)),
            Err(e) => Err(std::io::Error::other(format!("meta.json parse error: {e}"))),
        }
    }

    /// Apply `f` to the current meta (or an empty default if missing) and
    /// persist the result. Also updates the index entry. Used by the relay
    /// task on `Finished` events to bump `updated_at` / `total_tokens`.
    pub fn update_meta(&self, id: Uuid, f: impl FnOnce(&mut SessionMeta)) -> std::io::Result<()> {
        let mut meta = self.read_meta(id)?.unwrap_or_else(|| SessionMeta {
            id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: String::new(),
            provider: String::new(),
            title: None,
            system_prompt: None,
            total_tokens: 0,
            last_snapshot_seq: 0,
        });
        f(&mut meta);
        self.write_meta_atomic(&meta)?;
        self.upsert_index_entry(&meta)?;
        Ok(())
    }

    pub fn read_index(&self) -> std::io::Result<SessionIndex> {
        let path = self.sessions_dir.join("index.json");
        if !path.exists() {
            return Ok(SessionIndex::default());
        }
        let content = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<SessionIndex>(&content) {
            Ok(idx) => Ok(idx),
            Err(e) => {
                tracing::warn!("index.json parse error: {}; rebuilding empty", e);
                Ok(SessionIndex::default())
            }
        }
    }

    fn meta_path(&self, id: Uuid) -> PathBuf {
        self.sessions_dir.join(id.to_string()).join("meta.json")
    }

    fn write_meta_atomic(&self, meta: &SessionMeta) -> std::io::Result<()> {
        let path = self.meta_path(meta.id);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(meta).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        atomic_rename(&tmp, &path)
    }

    fn upsert_index_entry(&self, meta: &SessionMeta) -> std::io::Result<()> {
        let mut index = self.read_index()?;
        if index.version == 0 {
            index.version = 1;
        }
        let entry = SessionIndexEntry {
            id: meta.id,
            title: meta.title.clone(),
            model: meta.model.clone(),
            provider: meta.provider.clone(),
            // Preserve the original `created_at` if this entry already
            // exists (we're updating, not creating). Otherwise seed it from
            // the meta's `created_at`.
            created_at: Some(
                index
                    .sessions
                    .iter()
                    .find(|e| e.id == meta.id)
                    .and_then(|e| e.created_at)
                    .unwrap_or(meta.created_at),
            ),
            updated_at: meta.updated_at,
            total_tokens: meta.total_tokens,
        };
        if let Some(existing) = index.sessions.iter_mut().find(|e| e.id == meta.id) {
            *existing = entry;
        } else {
            index.sessions.push(entry);
        }
        // Keep newest-first for friendlier `ListSessions` output.
        index
            .sessions
            .sort_by_key(|b| std::cmp::Reverse(b.updated_at));

        let path = self.sessions_dir.join("index.json");
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(&index).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        atomic_rename(&tmp, &path)
    }
}

fn atomic_rename(src: &Path, dst: &Path) -> std::io::Result<()> {
    // On Windows, `std::fs::rename` overwrites the destination if it exists
    // (since 1.51+). On Unix, `rename` is atomic by POSIX. Either way this
    // is the canonical "atomic write" pattern.
    std::fs::rename(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_read_meta_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf());
        let id = Uuid::new_v4();
        let meta = store
            .init_session(id, "claude-sonnet-4-6", "anthropic", Some("be helpful"))
            .unwrap();
        assert_eq!(meta.id, id);
        assert_eq!(meta.model, "claude-sonnet-4-6");
        assert_eq!(meta.provider, "anthropic");
        assert_eq!(meta.system_prompt.as_deref(), Some("be helpful"));
        assert_eq!(meta.total_tokens, 0);

        let read_back = store.read_meta(id).unwrap().unwrap();
        assert_eq!(read_back, meta);
    }

    #[test]
    fn update_meta_bumps_fields_and_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf());
        let id = Uuid::new_v4();
        store
            .init_session(id, "claude-sonnet-4-6", "anthropic", None)
            .unwrap();

        store
            .update_meta(id, |m| {
                m.total_tokens += 1500;
                m.updated_at = Utc::now();
            })
            .unwrap();

        let after = store.read_meta(id).unwrap().unwrap();
        assert_eq!(after.total_tokens, 1500);
    }

    #[test]
    fn index_json_tracks_all_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf());
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        store
            .init_session(id1, "claude-sonnet-4-6", "anthropic", None)
            .unwrap();
        store
            .init_session(id2, "claude-haiku-3-5", "anthropic", None)
            .unwrap();

        let index = store.read_index().unwrap();
        assert_eq!(index.version, 1);
        assert_eq!(index.sessions.len(), 2);
        assert!(index.sessions.iter().any(|e| e.id == id1));
        assert!(index.sessions.iter().any(|e| e.id == id2));
    }

    #[test]
    fn read_meta_missing_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf());
        let meta = store.read_meta(Uuid::new_v4()).unwrap();
        assert!(meta.is_none());
    }

    #[test]
    fn read_index_missing_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf());
        let index = store.read_index().unwrap();
        assert_eq!(index.version, 0);
        assert!(index.sessions.is_empty());
    }
}
