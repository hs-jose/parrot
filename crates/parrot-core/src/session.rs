use crate::compaction::ContextLimits;
use crate::confirm::ConfirmRouter;
use crate::engine::ReActEngine;
use crate::hooks::HookRegistry;
use crate::provider::ProviderRegistry;
use crate::tool::ToolRegistry;
use crate::types::{ChatMessage, GenerateConfig};
use parrot_protocol::agent_event::{AgentEndReason, AgentEvent};
use parrot_protocol::types::SessionConfig as ProtocolSessionConfig;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub enum SessionCmd {
    Chat { message: String },
    Abort,
}

pub struct SessionHandle {
    pub id: Uuid,
    pub cmd_tx: mpsc::Sender<SessionCmd>,
    event_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub join_handle: tokio::task::JoinHandle<()>,
    pub end_reason: Arc<Mutex<AgentEndReason>>,
}

/// What the engine should inject as the system prompt when the client didn't
/// supply one in `SessionConfig`. Daemon supplies this so `parrot-core` stays
/// policy-free (the engine only emits whatever prompt it's handed).
pub fn default_system_prompt() -> String {
    "You are Parrot, a helpful coding assistant. Use the available tools when \
     they help you answer the user's request. Reason step by step about which \
     tool to call and with what arguments, then act. When you have enough \
     information, give a concise final answer."
        .to_string()
}

/// Phase 1.5: tool-call confirmation configuration. Lives in core so the
/// engine can read it without a boundary crossing each tool call. Populated
/// by the daemon from `config.tools.sandbox.require_confirmation` plus the
/// shared `ConfirmRouter` handle. Cloned cheaply into each engine instance.
#[derive(Clone)]
pub struct ConfirmConfig {
    /// Tool-name prefixes that require client confirmation before execution.
    /// Simple `starts_with` match (see `2026-06-21-parrot-phase-1.5.md` §4.3).
    pub require_confirmation: Vec<String>,
    /// How long to wait for `ClientMessage::ConfirmToolCall` before treating
    /// it as `ConfirmDecision::Timeout`. Default 60s; tests use a small value.
    pub timeout: Duration,
    /// The daemon-side router that bridges client responses to waiting
    /// session tasks. `None` disables confirmation entirely (engine never
    /// blocks on a confirm). Set by the daemon when constructing the engine.
    pub router: Option<Arc<ConfirmRouter>>,
}

impl Default for ConfirmConfig {
    fn default() -> Self {
        Self {
            require_confirmation: Vec::new(),
            timeout: Duration::from_secs(60),
            router: None,
        }
    }
}

pub struct SessionManager {
    sessions: HashMap<Uuid, SessionHandle>,
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    default_config: GenerateConfig,
    data_dir: std::path::PathBuf,
    working_dir: std::path::PathBuf,
    confirm_config: ConfirmConfig,
    hooks: Option<Arc<HookRegistry>>,
    /// Context limits (`[session]` from parrot.toml), inherited by
    /// every engine this manager spawns.
    context_limits: ContextLimits,
}

impl SessionManager {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        provider_registry: Arc<ProviderRegistry>,
        default_config: GenerateConfig,
        data_dir: std::path::PathBuf,
        working_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            sessions: HashMap::new(),
            tool_registry,
            provider_registry,
            default_config,
            data_dir,
            working_dir,
            confirm_config: ConfirmConfig::default(),
            hooks: None,
            context_limits: ContextLimits::default(),
        }
    }

    /// Set the confirmation config. Daemon calls this once at startup, after
    /// `new()`, before any session is created. Each engine instance gets a
    /// clone of this config.
    pub fn with_confirm_config(mut self, config: ConfirmConfig) -> Self {
        self.confirm_config = config;
        self
    }

    pub fn with_hooks(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hooks = Some(registry);
        self
    }

    /// Set context limits (`[session]` from parrot.toml). Every engine
    /// spawned afterwards inherits them.
    pub fn with_context_limits(mut self, limits: ContextLimits) -> Self {
        self.context_limits = limits;
        self
    }

    pub async fn create_session(
        &mut self,
        config: Option<ProtocolSessionConfig>,
    ) -> Result<Uuid, crate::error::AgentError> {
        let id = Uuid::new_v4();

        let (gen_config, system_prompt) = match config {
            Some(c) => {
                let gen = GenerateConfig {
                    model: c.model.unwrap_or_else(|| self.default_config.model.clone()),
                    temperature: self.default_config.temperature,
                    max_tokens: self.default_config.max_tokens,
                    stop_sequences: self.default_config.stop_sequences.clone(),
                };
                // If the client supplied a system_prompt use it; otherwise fall
                // back to the daemon default (kept here in core's `session`
                // module purely so the engine doesn't need to know the
                // default — the daemon is free to override before constructing
                // the engine if it wants a different default).
                let prompt = Some(c.system_prompt.unwrap_or_else(default_system_prompt));
                (gen, prompt)
            }
            None => {
                let prompt = Some(default_system_prompt());
                (self.default_config.clone(), prompt)
            }
        };

        self.spawn_session(id, gen_config, system_prompt, Vec::new())
            .await?;
        Ok(id)
    }

    /// Phase 1.5: create a session with a pre-built context. Used by
    /// `ResumeSession` to reconstruct the in-memory state from a replayed
    /// event log. The `replayed_context` should NOT include the system
    /// prompt — the engine injects `system_prompt` at the head itself
    /// (matching `create_session`'s behavior), so callers should pass only
    /// user/assistant/tool messages.
    pub async fn create_session_with_context(
        &mut self,
        id: Uuid,
        config: GenerateConfig,
        system_prompt: Option<String>,
        replayed_context: Vec<ChatMessage>,
    ) -> Result<Uuid, crate::error::AgentError> {
        self.spawn_session(id, config, system_prompt, replayed_context)
            .await?;
        Ok(id)
    }

    /// Resume variant: spawn an engine with the replayed context plus
    /// resume-specific metadata (`resumed_from_seq`, optional integrity
    /// warning). The daemon's `resume_session` calls this after running
    /// `EventLog::replay_for_resume` and `rebuild_context`.
    pub async fn create_resumed_session(
        &mut self,
        id: Uuid,
        config: GenerateConfig,
        system_prompt: Option<String>,
        replayed_context: Vec<ChatMessage>,
        resumed_from_seq: u64,
        integrity_warning: Option<parrot_protocol::agent_event::IntegrityIssue>,
    ) -> Result<Uuid, crate::error::AgentError> {
        let session_dir = self.data_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(64);

        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));

        let mut engine = ReActEngine::new(
            id,
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.provider_registry),
            config,
            system_prompt,
            session_dir,
            self.working_dir.clone(),
        )
        .with_confirm_config(self.confirm_config.clone())
        .with_initial_context(replayed_context)
        .with_resumed_from(resumed_from_seq)
        .with_context_limits(self.context_limits.clone())
        .with_end_reason(Arc::clone(&end_reason));

        if let Some(issue) = integrity_warning {
            engine = engine.with_pending_integrity_warning(issue);
        }

        let engine = if let Some(h) = self.hooks.clone() {
            engine.with_hooks(h)
        } else {
            engine
        };

        let join_handle = tokio::spawn(async move {
            engine.run(cmd_rx, event_tx).await;
        });

        self.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx,
                event_rx: Some(event_rx),
                join_handle,
                end_reason,
            },
        );

        Ok(id)
    }

    /// Shared inner: build the engine, spawn its task, register the handle.
    /// `initial_context` is fed to the engine's run loop so it can resume
    /// mid-conversation (empty for fresh sessions).
    async fn spawn_session(
        &mut self,
        id: Uuid,
        gen_config: GenerateConfig,
        system_prompt: Option<String>,
        initial_context: Vec<ChatMessage>,
    ) -> Result<(), crate::error::AgentError> {
        let session_dir = self.data_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(64);

        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));

        let engine = ReActEngine::new(
            id,
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.provider_registry),
            gen_config,
            system_prompt,
            session_dir,
            self.working_dir.clone(),
        )
        .with_confirm_config(self.confirm_config.clone())
        .with_initial_context(initial_context)
        .with_context_limits(self.context_limits.clone())
        .with_end_reason(Arc::clone(&end_reason));
        let engine = if let Some(h) = self.hooks.clone() {
            engine.with_hooks(h)
        } else {
            engine
        };

        let join_handle = tokio::spawn(async move {
            engine.run(cmd_rx, event_tx).await;
        });

        self.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx,
                event_rx: Some(event_rx),
                join_handle,
                end_reason,
            },
        );

        Ok(())
    }

    pub fn get_handle(&self, id: &Uuid) -> Option<&SessionHandle> {
        self.sessions.get(id)
    }

    pub fn get_handle_mut(&mut self, id: &Uuid) -> Option<&mut SessionHandle> {
        self.sessions.get_mut(id)
    }

    /// Takes the event receiver for a session. Returns None if already taken or session not found.
    pub fn take_event_receiver(&mut self, id: &Uuid) -> Option<mpsc::Receiver<AgentEvent>> {
        self.sessions.get_mut(id)?.event_rx.take()
    }

    /// Get tool definitions from the tool registry
    pub async fn list_tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.tool_registry.list_definitions().await
    }

    /// Enumerate all known session ids (used by the daemon to implement
    /// `ListSessions` in Phase 1.5 — kept here so the session manager is the
    /// single source of truth for the in-memory session set).
    pub fn session_ids(&self) -> Vec<Uuid> {
        self.sessions.keys().copied().collect()
    }

    /// Whether a session with the given id is currently live in this manager.
    /// `ResumeSession` uses this to short-circuit: if the session is already
    /// in memory (e.g. resumed by another connection), just acknowledge.
    pub fn contains(&self, id: &Uuid) -> bool {
        self.sessions.contains_key(id)
    }

    /// Check if a session is healthy: engine still running (cmd_tx open) and
    /// event receiver still available (not taken by a relay task). Used by
    /// the daemon's `resume_session` to decide whether to short-circuit or
    /// clean up and re-resume from disk.
    pub fn is_healthy(&self, id: &Uuid) -> bool {
        match self.sessions.get(id) {
            Some(h) => !h.cmd_tx.is_closed() && h.event_rx.is_some(),
            None => false,
        }
    }

    /// Remove a session from the manager and abort its engine task. Used by
    /// the daemon's `resume_session` to clean up stale handles (engine exited
    /// or event receiver already taken) before re-spawning from disk.
    pub fn remove(&mut self, id: &Uuid) -> bool {
        if let Some(handle) = self.sessions.remove(id) {
            handle.join_handle.abort();
            true
        } else {
            false
        }
    }
}
