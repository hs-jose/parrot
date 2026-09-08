//! 会话元数据持久化：每个会话一个 `meta.json` + 全局 `index.json`。
//!
//! 放在 daemon（而非 `parrot-core`）是因为它是纯 IO，设计的"core = 零 IO"
//! 规则规定文件 IO 归属 daemon。引擎经 `EventLog` 追加 `events.log`
//! （这是既有的 core IO 面，留待后续重构）；meta/index 文件是 daemon 的
//! 职责，在会话创建和每轮 `Finished` 事件时同步更新。
//!
//! 文件布局见设计文档 §7，生命周期触发点见 §4.4。

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
    pub total_tokens: u64,
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
    /// 会话首次创建时间。Phase 1.5 补充：线上 `SessionMeta` 暴露
    /// `created_at`，而 `ListSessions` 只读 `index.json`（不读每个会话的
    /// `meta.json`），所以索引必须带上 `created_at` 才能免去 N 次额外
    /// 文件读取。老版本 index.json（无此字段写入）通过
    /// `#[serde(default)]` 容忍——读取路径在 `created_at` 等于默认纪元
    /// 时回退用 `updated_at`。
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub total_tokens: u64,
}

/// 管理 `{data_dir}/sessions/` 下的每个会话 `meta.json` 与全局
/// `index.json`。所有写入都走"写临时文件再 rename"的原子模式，崩溃
/// 不会留下半截文件——设计文档 §7 明确要求这一点。
pub struct SessionStore {
    /// 会话根目录路径，如 `{data_dir}/parrot/sessions`。
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// 会话根目录路径（`{data_dir}/parrot/sessions/`）。暴露给 daemon
    /// 派生单会话路径（如处理 `GetHistory` 时的 `EventLog::replay`）。
    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions_dir.clone()
    }

    /// 确保会话目录存在。daemon 启动时调用一次。
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.sessions_dir)
    }

    /// 为新建会话写入初始 `meta.json` 并在 `index.json` 里加条目。
    /// 对 meta 文件幂等（同 id 二次调用会覆盖——用于补齐缺失字段）。
    pub fn init_session(
        &self,
        id: Uuid,
        model: &str,
        provider: &str,
    ) -> std::io::Result<SessionMeta> {
        self.ensure_dir()?;
        let session_dir = self.sessions_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;

        let meta = fresh_meta(id, model, provider, Utc::now());
        self.write_meta_atomic(&meta)?;
        self.upsert_index_entry(&meta)?;
        Ok(meta)
    }

    /// 读取某会话的 `meta.json`。文件不存在时返回 `Ok(None)`
    /// （比如会话创建于 meta.json 支持加入之前，或 id 无效）。
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

    /// 对当前 meta（缺失时用空默认值）应用 `f` 并持久化结果，同时更新
    /// 索引条目。relay 任务在 `Finished` 事件上用它更新
    /// `updated_at` / `total_tokens`。
    pub fn update_meta(&self, id: Uuid, f: impl FnOnce(&mut SessionMeta)) -> std::io::Result<()> {
        let mut meta = self
            .read_meta(id)?
            .unwrap_or_else(|| fresh_meta(id, "", "", Utc::now()));
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
        write_json_atomic(&self.meta_path(meta.id), meta)
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
            // 条目已存在（更新而非创建）时保留原 `created_at`；
            // 否则用 meta 的 `created_at` 播种。
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
        // 最新在前，`ListSessions` 输出更友好。
        index
            .sessions
            .sort_by_key(|b| std::cmp::Reverse(b.updated_at));

        write_json_atomic(&self.sessions_dir.join("index.json"), &index)
    }
}

/// 新建会话的 `SessionMeta` 初始值。`update_meta` 在 meta 缺失时的
/// 兜底也复用它（model/provider 传空串）。
fn fresh_meta(id: Uuid, model: &str, provider: &str, now: DateTime<Utc>) -> SessionMeta {
    SessionMeta {
        id,
        created_at: now,
        updated_at: now,
        model: model.to_string(),
        provider: provider.to_string(),
        title: None,
        total_tokens: 0,
    }
}

/// 原子写 JSON：先写临时文件再 rename。Windows 上 rename 会覆盖已存在的
/// 目标（Rust 1.51+ 起），Unix 上 rename 由 POSIX 保证原子——两端皆是
/// 标准的原子写模式。
fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
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
            .init_session(id, "claude-sonnet-4-6", "anthropic")
            .unwrap();
        assert_eq!(meta.id, id);
        assert_eq!(meta.model, "claude-sonnet-4-6");
        assert_eq!(meta.provider, "anthropic");
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
            .init_session(id, "claude-sonnet-4-6", "anthropic")
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
            .init_session(id1, "claude-sonnet-4-6", "anthropic")
            .unwrap();
        store
            .init_session(id2, "claude-haiku-3-5", "anthropic")
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
