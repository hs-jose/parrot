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

/// 客户端未在 `SessionConfig` 里带 system prompt 时，引擎注入的默认提示词。
/// 由 daemon 提供以此保证 `parrot-core` 不带策略（引擎只转发拿到的 prompt）。
pub fn default_system_prompt() -> String {
    "You are Parrot, a helpful coding assistant. Use the available tools when \
     they help you answer the user's request. Reason step by step about which \
     tool to call and with what arguments, then act. When you have enough \
     information, give a concise final answer."
        .to_string()
}

/// Phase 1.5：工具调用确认配置。放在 core 里让引擎读取时不必跨边界。
/// 由 daemon 根据 `config.tools.sandbox.require_confirmation` 与共享的
/// `ConfirmRouter` 句柄填充，再以低开销 clone 进每个引擎实例。
#[derive(Clone)]
pub struct ConfirmConfig {
    /// 需要客户端确认后才执行的工具名前缀。简单 `starts_with` 匹配
    /// （见 `2026-06-21-parrot-phase-1.5.md` §4.3）。
    pub require_confirmation: Vec<String>,
    /// 等待 `ClientMessage::ConfirmToolCall` 的时长，超时按
    /// `ConfirmDecision::Timeout` 处理。默认 60s；测试用小值。
    pub timeout: Duration,
    /// daemon 侧路由器，把客户端响应桥接到等待中的会话任务。
    /// `None` 表示完全禁用确认（引擎不会因确认阻塞）。
    /// 由 daemon 构造引擎时设置。
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
    /// 上下文限额（parrot.toml 的 `[session]`），由本管理器 spawn 的
    /// 每个引擎继承。
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

    /// 设置确认配置。daemon 在启动时调用一次，位于 `new()` 之后、
    /// 创建任何会话之前。每个引擎实例拿到它的 clone。
    pub fn with_confirm_config(mut self, config: ConfirmConfig) -> Self {
        self.confirm_config = config;
        self
    }

    pub fn with_hooks(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hooks = Some(registry);
        self
    }

    /// 设置上下文限额（parrot.toml 的 `[session]`）。之后 spawn 的
    /// 每个引擎都会继承。
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
                // 客户端带 system_prompt 就用；否则回退到 daemon 默认值。
                // 默认值放在 core 的 session 模块纯粹是为了让引擎不必知道
                // 默认内容——daemon 构造引擎前想换默认随时可以覆盖。
                let prompt = Some(c.system_prompt.unwrap_or_else(default_system_prompt));
                (gen, prompt)
            }
            None => {
                let prompt = Some(default_system_prompt());
                (self.default_config.clone(), prompt)
            }
        };

        self.spawn_session(id, gen_config, system_prompt, Vec::new(), None)
            .await?;
        Ok(id)
    }

    /// Resume 变体：用回放出的上下文加 resume 专属元数据（`resumed_from_seq`、
    /// 可选完整性告警）spawn 引擎。daemon 的 `resume_session` 在跑完
    /// `EventLog::replay_for_resume` 与 `rebuild_context` 后调用这里。
    pub async fn create_resumed_session(
        &mut self,
        id: Uuid,
        config: GenerateConfig,
        system_prompt: Option<String>,
        replayed_context: Vec<ChatMessage>,
        resumed_from_seq: u64,
        integrity_warning: Option<parrot_protocol::agent_event::IntegrityIssue>,
    ) -> Result<Uuid, crate::error::AgentError> {
        self.spawn_session(
            id,
            config,
            system_prompt,
            replayed_context,
            Some((resumed_from_seq, integrity_warning)),
        )
        .await?;
        Ok(id)
    }

    /// 内部共享：构造引擎、spawn 其任务、注册句柄。
    /// `initial_context` 喂给引擎的 run 循环，让它能从会话中段续跑
    /// （全新会话传空）。`resume` 携带 resume 回放元数据（seq 起点 +
    /// 可选完整性告警），全新会话传 `None`。
    async fn spawn_session(
        &mut self,
        id: Uuid,
        gen_config: GenerateConfig,
        system_prompt: Option<String>,
        initial_context: Vec<ChatMessage>,
        resume: Option<(u64, Option<parrot_protocol::agent_event::IntegrityIssue>)>,
    ) -> Result<(), crate::error::AgentError> {
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
            gen_config,
            system_prompt,
            session_dir,
            self.working_dir.clone(),
        )
        .with_confirm_config(self.confirm_config.clone())
        .with_initial_context(initial_context)
        .with_context_limits(self.context_limits.clone())
        .with_end_reason(Arc::clone(&end_reason));

        if let Some((resumed_from_seq, integrity_warning)) = resume {
            engine = engine.with_resumed_from(resumed_from_seq);
            if let Some(issue) = integrity_warning {
                engine = engine.with_pending_integrity_warning(issue);
            }
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

        Ok(())
    }

    pub fn get_handle(&self, id: &Uuid) -> Option<&SessionHandle> {
        self.sessions.get(id)
    }

    /// 取走某会话的事件接收端。已被取走或会话不存在时返回 None。
    pub fn take_event_receiver(&mut self, id: &Uuid) -> Option<mpsc::Receiver<AgentEvent>> {
        self.sessions.get_mut(id)?.event_rx.take()
    }

    /// 取工具注册表里的工具定义列表
    pub async fn list_tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.tool_registry.list_definitions().await
    }

    /// 某会话是否仍在本管理器内活跃。`ResumeSession` 用它短路：
    /// 会话已在内存（比如被另一条连接 resume 过）时直接确认即可。
    pub fn contains(&self, id: &Uuid) -> bool {
        self.sessions.contains_key(id)
    }

    /// 协作式优雅停机：把所有活跃会话的 `end_reason` 置为
    /// `DaemonShutdown`，发送 `Abort` 打断在途工作，丢掉命令发送端
    /// （顺带取走句柄）让空闲引擎走 `None` 分支退出，然后在 `deadline`
    /// 内逐个 await `JoinHandle`。超时未退出的会话被强制 abort
    /// （其 `AgentEndGuard` 的 Drop 会尽力补发 `AgentEnd`）。
    /// `DaemonShutdown` 原因得以保留，因为引擎的 `None` 分支不会
    /// 覆盖已设置的 `DaemonShutdown`。
    pub async fn shutdown_all(&mut self, deadline: Duration) {
        for handle in self.sessions.values() {
            *handle.end_reason.lock().unwrap() = AgentEndReason::DaemonShutdown;
            let _ = handle.cmd_tx.send(SessionCmd::Abort).await;
        }
        // 第二次 Abort：防止第一次恰好在 turn 中途被消费、引擎此刻
        // 正在等下一条命令的情况。
        for handle in self.sessions.values() {
            let _ = handle.cmd_tx.send(SessionCmd::Abort).await;
        }
        let handles: Vec<(Uuid, tokio::task::JoinHandle<()>)> = std::mem::take(&mut self.sessions)
            .into_values()
            .map(|h| (h.id, h.join_handle))
            .collect();
        for (id, mut join) in handles {
            match tokio::time::timeout(deadline, &mut join).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(session_id = %id, "shutdown deadline exceeded, force-aborting");
                    join.abort();
                }
            }
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_all_aborts_active_session_within_deadline() {
        let mut mgr = SessionManager::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(ProviderRegistry::new()),
            GenerateConfig::default(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("."),
        );
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (evt_tx, _evt_rx) = mpsc::channel::<AgentEvent>(64);
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));
        let id = Uuid::new_v4();
        let join = tokio::spawn(async move {
            let mut aborted = false;
            while let Some(cmd) = cmd_rx.recv().await {
                if let SessionCmd::Abort = cmd {
                    aborted = true;
                    break;
                }
            }
            let _ = (aborted, evt_tx);
        });
        mgr.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx: cmd_tx.clone(),
                event_rx: None,
                join_handle: join,
                end_reason: Arc::clone(&end_reason),
            },
        );

        mgr.shutdown_all(Duration::from_millis(500)).await;

        assert_eq!(*end_reason.lock().unwrap(), AgentEndReason::DaemonShutdown);
        assert!(cmd_tx.is_closed());
    }

    #[tokio::test]
    async fn shutdown_all_force_aborts_unresponsive_session() {
        let mut mgr = SessionManager::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(ProviderRegistry::new()),
            GenerateConfig::default(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("."),
        );
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (_evt_tx, _evt_rx) = mpsc::channel::<AgentEvent>(64);
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));
        let id = Uuid::new_v4();
        let join = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        mgr.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx,
                event_rx: None,
                join_handle: join,
                end_reason,
            },
        );

        let start = std::time::Instant::now();
        mgr.shutdown_all(Duration::from_millis(100)).await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "must not hang"
        );
    }
}
