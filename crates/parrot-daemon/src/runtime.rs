use crate::auth::Auth;
use crate::session_store::SessionStore;
use parrot_config::AppConfig;
use parrot_core::confirm::ConfirmRouter;
use parrot_core::event_log::{rebuild_context, EventLog};
use parrot_core::provider::ProviderRegistry;
use parrot_core::session::{ConfirmConfig, SessionCmd, SessionManager};
use parrot_core::tool::ToolRegistry;
use parrot_core::types::GenerateConfig;
use parrot_hooks::build_registry;
use parrot_protocol::agent_event::{AgentEvent, PersistedAgentEvent};
use parrot_protocol::types::{
    ErrorCode, ModelInfo as ProtocolModelInfo, SessionMeta as ProtocolSessionMeta,
    ToolDefinitionWire,
};
use parrot_protocol::{ClientMessage, ServerMessage};
use parrot_tools::shell_exec::run_shell_command;
use parrot_transport::{accept_connection, TransportServer, WsTransportServer};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    let auth = Auth::new(&token_path).await?;
    let auth = Arc::new(auth);

    let tool_registry = Arc::new(ToolRegistry::new());
    let provider_registry = Arc::new(ProviderRegistry::new());

    parrot_tools::register_all(&tool_registry, &config).await;
    parrot_providers::register_all(&provider_registry, &config).await;

    run_with(config, auth, provider_registry, tool_registry).await
}

pub async fn run_with(
    config: AppConfig,
    auth: Arc<Auth>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_confirm_timeout(
        config,
        auth,
        provider_registry,
        tool_registry,
        Duration::from_secs(60),
    )
    .await
}

pub async fn run_with_confirm_timeout(
    config: AppConfig,
    auth: Arc<Auth>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    confirm_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let default_config = config
        .providers
        .first()
        .map(|p| GenerateConfig {
            model: p.default_model.clone(),
            temperature: None,
            max_tokens: Some(8192),
            stop_sequences: None,
        })
        .unwrap_or_default();

    let sessions_dir = std::path::PathBuf::from(&config.session.data_dir).join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;

    let session_store = Arc::new(SessionStore::new(sessions_dir.clone()));

    let working_dir =
        Arc::new(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));

    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    info!(
        "Parrot daemon starting on {}:{}; token file at {:?}",
        config.daemon.host, config.daemon.port, token_path
    );

    let confirm_router = Arc::new(ConfirmRouter::new());
    let confirm_config = ConfirmConfig {
        require_confirmation: config.tools.sandbox.require_confirmation.clone(),
        timeout: confirm_timeout,
        router: Some(Arc::clone(&confirm_router)),
    };

    let hook_registry = build_registry(&config.hooks);

    let session_manager = Arc::new(RwLock::new(
        SessionManager::new(
            Arc::clone(&tool_registry),
            Arc::clone(&provider_registry),
            default_config.clone(),
            sessions_dir,
            (*working_dir).clone(),
        )
        .with_confirm_config(confirm_config)
        .with_hooks(hook_registry)
        .with_context_limits(parrot_core::compaction::ContextLimits {
            max_history_tokens: config.session.max_history_tokens,
            keep_recent_turns: config.session.keep_recent_turns,
            compaction: parrot_core::compaction::CompactionConfig {
                enabled: config.session.compaction,
                threshold: config.session.compaction_threshold,
                keep_recent_tokens: config.session.keep_recent_tokens,
                summary_max_tokens: config.session.summary_max_tokens,
            },
        }),
    ));
    // 包一层 Arc，让每个 spawn 出的 handler 共享 daemon 的当前默认配置，
    // 避免每条连接各自 clone。
    let default_config = Arc::new(default_config);

    let ws_server = WsTransportServer::new(&config.daemon.host, config.daemon.port);
    let listener = ws_server.bind().await?;

    loop {
        let sig = async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = unix_terminate_signal() => {},
            }
        };
        tokio::select! {
            biased;
            _ = sig => break,
            res = accept_connection(&listener) => match res {
                Ok(client_conn) => {
                    let auth = Arc::clone(&auth);
                    let session_manager = Arc::clone(&session_manager);
                    let session_store = Arc::clone(&session_store);
                    let provider_registry = Arc::clone(&provider_registry);
                    let confirm_router = Arc::clone(&confirm_router);
                    let default_config = Arc::clone(&default_config);
                    let working_dir = Arc::clone(&working_dir);

                    tokio::spawn(async move {
                        handle_connection(
                            client_conn,
                            auth,
                            session_manager,
                            session_store,
                            provider_registry,
                            confirm_router,
                            default_config,
                            working_dir,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            },
        }
    }

    info!("Shutdown signal received, draining sessions...");
    session_manager
        .write()
        .await
        .shutdown_all(Duration::from_secs(3))
        .await;
    info!("All sessions drained, exiting");
    Ok(())
}

async fn unix_terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}

/// 向客户端发一条 `Error` 消息（发送失败忽略——连接可能已断）。
async fn send_error(
    sender: &tokio::sync::mpsc::Sender<ServerMessage>,
    session_id: Option<uuid::Uuid>,
    code: ErrorCode,
    message: impl Into<String>,
) {
    let _ = sender
        .send(ServerMessage::Error {
            session_id,
            code,
            message: message.into(),
        })
        .await;
}

/// 把会话命令投递给活跃会话；会话不存在时回一条 `SessionNotFound`。
/// Chat / Abort 两个消息臂共用此逻辑。
async fn send_session_cmd(
    mgr: &SessionManager,
    sender: &tokio::sync::mpsc::Sender<ServerMessage>,
    session_id: uuid::Uuid,
    cmd: SessionCmd,
    label: &str,
) {
    match mgr.get_handle(&session_id) {
        Some(handle) => {
            if let Err(e) = handle.cmd_tx.send(cmd).await {
                error!(
                    "Failed to send {} command to session {}: {}",
                    label, session_id, e
                );
            }
        }
        None => {
            send_error(
                sender,
                Some(session_id),
                ErrorCode::SessionNotFound,
                "Session not found",
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    mut client: parrot_transport::ClientConnection,
    auth: Arc<Auth>,
    session_manager: Arc<RwLock<SessionManager>>,
    session_store: Arc<SessionStore>,
    provider_registry: Arc<ProviderRegistry>,
    confirm_router: Arc<ConfirmRouter>,
    default_config: Arc<GenerateConfig>,
    working_dir: Arc<std::path::PathBuf>,
) {
    let client_id = client.id;
    info!("Handling connection from client {}", client_id);

    let mut authenticated = false;

    while let Some(msg) = client.receiver.recv().await {
        match msg {
            ClientMessage::Hello {
                token,
                client_version,
            } => {
                if auth.validate(&token) {
                    authenticated = true;
                    info!(
                        "Client {} authenticated (version: {})",
                        client_id, client_version
                    );
                    let _ = client
                        .sender
                        .send(ServerMessage::HelloAck {
                            server_version: env!("CARGO_PKG_VERSION").to_string(),
                        })
                        .await;
                } else {
                    warn!("Client {} auth failed", client_id);
                    send_error(
                        &client.sender,
                        None,
                        ErrorCode::AuthFailed,
                        "Authentication failed",
                    )
                    .await;
                    return;
                }
            }
            _ if !authenticated => {
                send_error(
                    &client.sender,
                    None,
                    ErrorCode::AuthFailed,
                    "Not authenticated",
                )
                .await;
                return;
            }
            ClientMessage::CreateSession { config } => {
                let proto_config = config.map(|c| parrot_protocol::types::SessionConfig {
                    model: c.model,
                    provider: c.provider,
                    system_prompt: c.system_prompt,
                });

                // meta 记录的模型：客户端显式指定优先，否则用 daemon 默认。
                let model_for_meta = proto_config
                    .as_ref()
                    .and_then(|c| c.model.clone())
                    .unwrap_or_else(|| default_config.model.clone());

                let mut mgr = session_manager.write().await;
                match mgr.create_session(proto_config).await {
                    Ok(session_id) => {
                        info!("Created session {} for client {}", session_id, client_id);

                        let provider_id = provider_registry
                            .resolve(&model_for_meta)
                            .await
                            .map(|p| p.provider_id().to_string())
                            .unwrap_or_else(|| "unknown".to_string());

                        if let Err(e) =
                            session_store.init_session(session_id, &model_for_meta, &provider_id)
                        {
                            warn!(
                                "Failed to write initial meta.json for {}: {}",
                                session_id, e
                            );
                        }

                        let event_rx = mgr
                            .take_event_receiver(&session_id)
                            .expect("session just created but receiver missing");
                        drop(mgr);

                        let sender = client.sender.clone();
                        let store = Arc::clone(&session_store);
                        tokio::spawn(async move {
                            relay_session_events(session_id, event_rx, sender, store).await;
                        });

                        let _ = client
                            .sender
                            .send(ServerMessage::SessionCreated { session_id })
                            .await;
                    }
                    Err(e) => {
                        error!("Failed to create session for client {}: {}", client_id, e);
                        send_error(
                            &client.sender,
                            None,
                            ErrorCode::InternalError,
                            format!("Failed to create session: {}", e),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::Chat {
                session_id,
                message,
            } => {
                let mgr = session_manager.read().await;
                send_session_cmd(
                    &mgr,
                    &client.sender,
                    session_id,
                    SessionCmd::Chat { message },
                    "chat",
                )
                .await;
            }
            ClientMessage::Abort { session_id } => {
                let mgr = session_manager.read().await;
                send_session_cmd(&mgr, &client.sender, session_id, SessionCmd::Abort, "abort")
                    .await;
            }
            ClientMessage::Shell {
                session_id,
                command,
            } => match run_shell_command(&command, &working_dir).await {
                Ok(r) => {
                    let _ = client
                        .sender
                        .send(ServerMessage::ShellResult {
                            session_id,
                            output: r.output,
                            exit_code: r.exit_code,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = client
                        .sender
                        .send(ServerMessage::ShellResult {
                            session_id,
                            output: e,
                            exit_code: -1,
                        })
                        .await;
                }
            },
            ClientMessage::ListModels => {
                let models = collect_models(&provider_registry).await;
                let _ = client
                    .sender
                    .send(ServerMessage::ModelList { models })
                    .await;
            }
            ClientMessage::ListTools { session_id } => {
                let mgr = session_manager.read().await;
                let defs = mgr.list_tool_definitions().await;
                drop(mgr);

                let tools: Vec<ToolDefinitionWire> = defs
                    .into_iter()
                    .map(|d| ToolDefinitionWire {
                        name: d.name,
                        description: d.description,
                        input_schema: d.input_schema,
                    })
                    .collect();
                let _ = client
                    .sender
                    .send(ServerMessage::ToolList { session_id, tools })
                    .await;
            }
            ClientMessage::GetHistory { session_id } => {
                match read_history(&session_manager, &session_store, session_id).await {
                    Ok(events) => {
                        let _ = client
                            .sender
                            .send(ServerMessage::History { session_id, events })
                            .await;
                    }
                    Err(e) => {
                        send_error(
                            &client.sender,
                            Some(session_id),
                            ErrorCode::InternalError,
                            format!("Failed to read history: {}", e),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::ListSessions => match session_store.read_index() {
                Ok(index) => {
                    let sessions: Vec<ProtocolSessionMeta> = index
                        .sessions
                        .into_iter()
                        .map(|e| ProtocolSessionMeta {
                            id: e.id,
                            created_at: e.created_at.unwrap_or(e.updated_at),
                            updated_at: e.updated_at,
                            model: e.model,
                            provider: e.provider,
                            title: e.title,
                            total_tokens: e.total_tokens,
                        })
                        .collect();
                    let _ = client
                        .sender
                        .send(ServerMessage::SessionList { sessions })
                        .await;
                }
                Err(e) => {
                    send_error(
                        &client.sender,
                        None,
                        ErrorCode::InternalError,
                        format!("Failed to read session index: {}", e),
                    )
                    .await;
                }
            },
            ClientMessage::ResumeSession { session_id } => {
                match resume_session(
                    session_id,
                    &session_manager,
                    &session_store,
                    &default_config,
                )
                .await
                {
                    Ok(()) => {
                        let mut mgr = session_manager.write().await;
                        if let Some(event_rx) = mgr.take_event_receiver(&session_id) {
                            drop(mgr);
                            let sender = client.sender.clone();
                            let store = Arc::clone(&session_store);
                            tokio::spawn(async move {
                                relay_session_events(session_id, event_rx, sender, store).await;
                            });
                        }
                        let _ = client
                            .sender
                            .send(ServerMessage::SessionResumed { session_id })
                            .await;
                    }
                    Err(e) => {
                        send_error(
                            &client.sender,
                            Some(session_id),
                            ErrorCode::SessionNotFound,
                            format!("Failed to resume session: {}", e),
                        )
                        .await;
                    }
                }
            }
            ClientMessage::ConfirmToolCall {
                session_id,
                tool_id,
                decision,
            } => {
                let found = confirm_router.resolve(session_id, &tool_id, decision).await;
                if !found {
                    warn!(
                        "ConfirmToolCall for ({}, {}) had no pending waiter; ignoring",
                        session_id, tool_id
                    );
                }
            }
            ClientMessage::ListMcpServers => {
                send_error(
                    &client.sender,
                    None,
                    ErrorCode::InvalidRequest,
                    "MCP server support is not yet available",
                )
                .await;
            }
        }
    }

    info!("Client {} disconnected", client_id);
}

/// 把会话任务的 `AgentEvent` 中继到客户端 WS 发送端，并观察 `TurnEnd`
/// 事件更新 `meta.json` / `index.json`。
async fn relay_session_events(
    session_id: uuid::Uuid,
    mut event_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    sender: tokio::sync::mpsc::Sender<ServerMessage>,
    session_store: Arc<SessionStore>,
) {
    while let Some(event) = event_rx.recv().await {
        // TurnEnd 事件时更新 meta.json（已完成 turn 的新生命周期边界，
        // EndTurn 与 Aborted 都算）。
        if let AgentEvent::TurnEnd { usage, .. } = &event {
            if let Err(e) = session_store.update_meta(session_id, |m| {
                m.updated_at = chrono::Utc::now();
                m.total_tokens = m
                    .total_tokens
                    .saturating_add((usage.input_tokens + usage.output_tokens) as u64);
            }) {
                warn!("Failed to update meta.json for {}: {}", session_id, e);
            }
        }

        let msg = ServerMessage::AgentEvent { event };
        if sender.send(msg).await.is_err() {
            break;
        }
    }
}

async fn collect_models(provider_registry: &ProviderRegistry) -> Vec<ProtocolModelInfo> {
    let mut all = Vec::new();
    for provider_id in provider_registry.provider_ids().await {
        let Some(provider) = provider_registry.get(&provider_id).await else {
            continue;
        };
        match provider.list_models().await {
            Ok(models) => all.extend(models.into_iter().map(|m| ProtocolModelInfo {
                id: m.id,
                name: m.name,
                provider: m.provider,
                context_window: m.context_window,
                max_output_tokens: m.max_output_tokens,
            })),
            Err(e) => {
                warn!("list_models failed for provider {}: {}", provider_id, e);
            }
        }
    }
    all
}

/// 会话在磁盘上的 meta 是否存在（读错误转成字符串错误）。
fn meta_exists(session_store: &SessionStore, session_id: uuid::Uuid) -> Result<bool, String> {
    session_store
        .read_meta(session_id)
        .map_err(|e| format!("read meta: {e}"))
        .map(|m| m.is_some())
}

/// 某会话的目录路径（`{sessions_dir}/{id}/`）。
fn session_dir(session_store: &SessionStore, session_id: uuid::Uuid) -> std::path::PathBuf {
    session_store.sessions_dir().join(session_id.to_string())
}

async fn read_history(
    session_manager: &RwLock<SessionManager>,
    session_store: &SessionStore,
    session_id: uuid::Uuid,
) -> Result<Vec<PersistedAgentEvent>, String> {
    let in_memory = session_manager
        .read()
        .await
        .get_handle(&session_id)
        .is_some();
    let on_disk = meta_exists(session_store, session_id)?;

    if !in_memory && !on_disk {
        return Err(format!("session {session_id} not found"));
    }

    let log = EventLog::new(session_dir(session_store, session_id));
    log.replay().map_err(|e| format!("replay: {e}"))
}

/// resume 一个已持久化的会话。步骤：
///   1. 会话已在 `SessionManager` 里活跃 → 直接返回。
///   2. 否则读 `meta.json` 仅确认磁盘上存在（meta 里的 model /
///      system_prompt 刻意不复用——resume 把持久化会话当纯对话历史，
///      喂给 daemon 当前的 `default_config`，因此改过的 parrot.toml
///      在 resume 时生效，而不是复活一份已过时的配置），跑
///      `EventLog::replay_for_resume`（截半成品 turn、写 corrupted.log、
///      返回可选 IntegrityIssue），重建上下文，并带 resume 元数据经
///      `create_resumed_session` spawn 一个全新引擎任务。
async fn resume_session(
    session_id: uuid::Uuid,
    session_manager: &RwLock<SessionManager>,
    session_store: &SessionStore,
    default_config: &GenerateConfig,
) -> Result<(), String> {
    if session_manager.read().await.contains(&session_id) {
        return Ok(());
    }

    // 确认会话在磁盘上存在。不复读 meta 里的 model / system_prompt：
    // 这两者取自 default_config，让 resume 后的会话跟随 daemon 当前
    // 配置，而不是创建时的旧配置。
    if !meta_exists(session_store, session_id)? {
        return Err(format!("session {session_id} not found on disk"));
    }

    let mut log = EventLog::new(session_dir(session_store, session_id));
    let (events, integrity_issue) = log
        .replay_for_resume()
        .map_err(|e| format!("replay: {e}"))?;

    let replayed_context = rebuild_context(&events);
    let resumed_from_seq = events.len() as u64;

    let mut mgr = session_manager.write().await;
    mgr.create_resumed_session(
        session_id,
        default_config.clone(),
        // resume 不带 per-session prompt：为 None 时引擎注入 daemon 默认
        // system prompt（`default_system_prompt()`），使 resume 会话与
        // 当前 daemon 策略一致，而不是复活旧 persona。
        None,
        replayed_context,
        resumed_from_seq,
        integrity_issue,
    )
    .await
    .map_err(|e| format!("spawn resumed session: {e}"))?;

    Ok(())
}
