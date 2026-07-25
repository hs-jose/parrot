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
    ModelInfo as ProtocolModelInfo, SessionMeta as ProtocolSessionMeta, ToolDefinitionWire,
};
use parrot_protocol::{ClientMessage, ServerMessage};
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

    let data_dir = std::path::PathBuf::from(&config.session.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let sessions_dir = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;

    let session_store = Arc::new(SessionStore::new(sessions_dir.clone()));
    session_store.ensure_dir()?;

    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

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
            working_dir,
        )
        .with_confirm_config(confirm_config)
        .with_hooks(hook_registry),
    ));
    // Wrap once in an Arc so each spawned handler can share the daemon's
    // current default config without per-connection cloning.
    let default_config = Arc::new(default_config);

    let ws_server = WsTransportServer::new(&config.daemon.host, config.daemon.port);
    let listener = ws_server.bind().await?;

    loop {
        match accept_connection(&listener).await {
            Ok(client_conn) => {
                let auth = Arc::clone(&auth);
                let session_manager = Arc::clone(&session_manager);
                let session_store = Arc::clone(&session_store);
                let provider_registry = Arc::clone(&provider_registry);
                let confirm_router = Arc::clone(&confirm_router);
                let default_config = Arc::clone(&default_config);

                tokio::spawn(async move {
                    handle_connection(
                        client_conn,
                        auth,
                        session_manager,
                        session_store,
                        provider_registry,
                        confirm_router,
                        default_config,
                    )
                    .await;
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}

struct ConnectionContext {
    authenticated: bool,
}

async fn handle_connection(
    mut client: parrot_transport::ClientConnection,
    auth: Arc<Auth>,
    session_manager: Arc<RwLock<SessionManager>>,
    session_store: Arc<SessionStore>,
    provider_registry: Arc<ProviderRegistry>,
    confirm_router: Arc<ConfirmRouter>,
    default_config: Arc<GenerateConfig>,
) {
    let client_id = client.id;
    info!("Handling connection from client {}", client_id);

    let mut ctx = ConnectionContext {
        authenticated: false,
    };

    while let Some(msg) = client.receiver.recv().await {
        match msg {
            ClientMessage::Hello {
                token,
                client_version,
            } => {
                if auth.validate(&token) {
                    ctx.authenticated = true;
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
                    let _ = client
                        .sender
                        .send(ServerMessage::Error {
                            session_id: None,
                            code: parrot_protocol::types::ErrorCode::AuthFailed,
                            message: "Authentication failed".to_string(),
                        })
                        .await;
                    return;
                }
            }
            _ if !ctx.authenticated => {
                let _ = client
                    .sender
                    .send(ServerMessage::Error {
                        session_id: None,
                        code: parrot_protocol::types::ErrorCode::AuthFailed,
                        message: "Not authenticated".to_string(),
                    })
                    .await;
                return;
            }
            ClientMessage::CreateSession { config } => {
                let proto_config = config.map(|c| parrot_protocol::types::SessionConfig {
                    model: c.model,
                    provider: c.provider,
                    system_prompt: c.system_prompt,
                });

                let model_for_meta = proto_config
                    .as_ref()
                    .and_then(|c| c.model.clone())
                    .or_else(|| Some(default_config.model.clone()));
                let system_prompt_for_meta =
                    proto_config.as_ref().and_then(|c| c.system_prompt.clone());

                let mut mgr = session_manager.write().await;
                match mgr.create_session(proto_config).await {
                    Ok(session_id) => {
                        info!("Created session {} for client {}", session_id, client_id);

                        let provider_id = provider_registry
                            .resolve(&model_for_meta.clone().unwrap_or_default())
                            .await
                            .map(|p| p.provider_id().to_string())
                            .unwrap_or_else(|| "unknown".to_string());

                        if let Err(e) = session_store.init_session(
                            session_id,
                            &model_for_meta.clone().unwrap_or_default(),
                            &provider_id,
                            system_prompt_for_meta.as_deref(),
                        ) {
                            warn!(
                                "Failed to write initial meta.json for {}: {}",
                                session_id, e
                            );
                        }

                        let mut event_rx = mgr
                            .take_event_receiver(&session_id)
                            .expect("session just created but receiver missing");
                        drop(mgr);

                        let sender = client.sender.clone();
                        let store = Arc::clone(&session_store);
                        tokio::spawn(async move {
                            relay_session_events(session_id, &mut event_rx, sender, store).await;
                        });

                        let _ = client
                            .sender
                            .send(ServerMessage::SessionCreated { session_id })
                            .await;
                    }
                    Err(e) => {
                        error!("Failed to create session for client {}: {}", client_id, e);
                        let _ = client
                            .sender
                            .send(ServerMessage::Error {
                                session_id: None,
                                code: parrot_protocol::types::ErrorCode::InternalError,
                                message: format!("Failed to create session: {}", e),
                            })
                            .await;
                    }
                }
            }
            ClientMessage::Chat {
                session_id,
                message,
            } => {
                let mgr = session_manager.read().await;
                match mgr.get_handle(&session_id) {
                    Some(handle) => {
                        if let Err(e) = handle.cmd_tx.send(SessionCmd::Chat { message }).await {
                            error!(
                                "Failed to send chat command to session {}: {}",
                                session_id, e
                            );
                        }
                    }
                    None => {
                        let _ = client
                            .sender
                            .send(ServerMessage::Error {
                                session_id: Some(session_id),
                                code: parrot_protocol::types::ErrorCode::SessionNotFound,
                                message: "Session not found".to_string(),
                            })
                            .await;
                    }
                }
            }
            ClientMessage::Abort { session_id } => {
                let mgr = session_manager.read().await;
                match mgr.get_handle(&session_id) {
                    Some(handle) => {
                        if let Err(e) = handle.cmd_tx.send(SessionCmd::Abort).await {
                            error!(
                                "Failed to send abort command to session {}: {}",
                                session_id, e
                            );
                        }
                    }
                    None => {
                        let _ = client
                            .sender
                            .send(ServerMessage::Error {
                                session_id: Some(session_id),
                                code: parrot_protocol::types::ErrorCode::SessionNotFound,
                                message: "Session not found".to_string(),
                            })
                            .await;
                    }
                }
            }
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
                        let _ = client
                            .sender
                            .send(ServerMessage::Error {
                                session_id: Some(session_id),
                                code: parrot_protocol::types::ErrorCode::InternalError,
                                message: format!("Failed to read history: {}", e),
                            })
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
                    let _ = client
                        .sender
                        .send(ServerMessage::Error {
                            session_id: None,
                            code: parrot_protocol::types::ErrorCode::InternalError,
                            message: format!("Failed to read session index: {}", e),
                        })
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
                        if let Some(mut event_rx) = mgr.take_event_receiver(&session_id) {
                            drop(mgr);
                            let sender = client.sender.clone();
                            let store = Arc::clone(&session_store);
                            tokio::spawn(async move {
                                relay_session_events(session_id, &mut event_rx, sender, store)
                                    .await;
                            });
                        }
                        let _ = client
                            .sender
                            .send(ServerMessage::SessionResumed { session_id })
                            .await;
                    }
                    Err(e) => {
                        let _ = client
                            .sender
                            .send(ServerMessage::Error {
                                session_id: Some(session_id),
                                code: parrot_protocol::types::ErrorCode::SessionNotFound,
                                message: format!("Failed to resume session: {}", e),
                            })
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
        }
    }

    info!("Client {} disconnected", client_id);
}

/// Relay `AgentEvent`s from a session task to a client's WS sender, and
/// observe `TurnEnd` events to update `meta.json` / `index.json`.
async fn relay_session_events(
    session_id: uuid::Uuid,
    event_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    sender: tokio::sync::mpsc::Sender<ServerMessage>,
    session_store: Arc<SessionStore>,
) {
    while let Some(event) = event_rx.recv().await {
        // Update meta.json on TurnEnd events (the new lifecycle boundary
        // for a completed turn — both EndTurn and Aborted).
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
        if let Some(provider) = provider_registry.get(&provider_id).await {
            match provider.list_models().await {
                Ok(models) => {
                    for m in models {
                        all.push(ProtocolModelInfo {
                            id: m.id,
                            name: m.name,
                            provider: m.provider,
                            context_window: m.context_window,
                            max_output_tokens: m.max_output_tokens,
                        });
                    }
                }
                Err(e) => {
                    warn!("list_models failed for provider {}: {}", provider_id, e);
                }
            }
        }
    }
    all
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
    let on_disk = session_store
        .read_meta(session_id)
        .map_err(|e| format!("read meta: {e}"))?
        .is_some();

    if !in_memory && !on_disk {
        return Err(format!("session {session_id} not found"));
    }

    let sessions_dir = session_store.sessions_dir();
    let session_dir = sessions_dir.join(session_id.to_string());
    let log = EventLog::new(session_dir);
    log.replay().map_err(|e| format!("replay: {e}"))
}

/// Resume a previously-persisted session. Steps:
///   1. If the session is already live in the `SessionManager`, do nothing.
///   2. Otherwise read `meta.json` only to confirm the session exists on
///      disk (its `meta.model` / `meta.system_prompt` are intentionally
///      NOT reused — resume treats the persisted session as pure
///      conversation history fed to the daemon's current `default_config`,
///      so a changed `parrot.toml` takes effect on resume rather than
///      resurrecting a now-dead config), run `EventLog::replay_for_resume`
///      (truncates partial turns, writes corrupted.log, returns optional
///      IntegrityIssue), rebuild the context, and spawn a fresh engine
///      task via `create_resumed_session` with the resume metadata.
async fn resume_session(
    session_id: uuid::Uuid,
    session_manager: &RwLock<SessionManager>,
    session_store: &SessionStore,
    default_config: &GenerateConfig,
) -> Result<(), String> {
    if session_manager.read().await.contains(&session_id) {
        return Ok(());
    }

    // Confirm the session exists on disk. We do NOT read back
    // `meta.model` / `meta.system_prompt`: those come from
    // `default_config` so the resumed session follows the current daemon
    // config, not the one it was originally created under.
    let on_disk = session_store
        .read_meta(session_id)
        .map_err(|e| format!("read meta: {e}"))?
        .is_some();
    if !on_disk {
        return Err(format!("session {session_id} not found on disk"));
    }

    let sessions_dir = session_store.sessions_dir();
    let session_dir = sessions_dir.join(session_id.to_string());
    let mut log = EventLog::new(session_dir);
    let (events, integrity_issue) = log
        .replay_for_resume()
        .map_err(|e| format!("replay: {e}"))?;

    let replayed_context = rebuild_context(&events);
    let resumed_from_seq = events.len() as u64;

    let mut mgr = session_manager.write().await;
    mgr.create_resumed_session(
        session_id,
        default_config.clone(),
        // No per-session prompt on resume: the engine injects the daemon
        // default system prompt (`default_system_prompt()`) when this is
        // `None`, keeping resumed sessions consistent with the current
        // daemon policy rather than resurrecting an old persona.
        None,
        replayed_context,
        resumed_from_seq,
        integrity_issue,
    )
    .await
    .map_err(|e| format!("spawn resumed session: {e}"))?;

    Ok(())
}
