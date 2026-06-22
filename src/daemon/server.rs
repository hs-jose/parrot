use crate::auth::Auth;
use crate::session_adapter::SessionAdapter;
use crate::session_store::SessionStore;
use parrot_config::AppConfig;
use parrot_core::confirm::ConfirmRouter;
use parrot_core::event_log::{EventLog, StreamEvent};
use parrot_core::provider::ProviderRegistry;
use parrot_core::session::{ConfirmConfig, SessionCmd, SessionManager};
use parrot_core::tool::ToolRegistry;
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig};
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
    // Initialize auth
    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    let auth = Auth::new(&token_path).await?;
    let auth = Arc::new(auth);

    // Initialize registries
    let tool_registry = Arc::new(ToolRegistry::new());
    let provider_registry = Arc::new(ProviderRegistry::new());

    // Register tools and providers
    crate::tools::register_all(&tool_registry, &config).await;
    crate::providers::register_all(&provider_registry, &config).await;

    run_with(config, auth, provider_registry, tool_registry).await
}

/// Run the daemon with pre-built registries. Exposed so integration tests can
/// inject a mock `LlmProvider` (and custom tools) instead of loading real
/// providers from config. Uses the production 60s confirmation timeout.
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

/// Same as `run_with` but lets the caller override the confirmation timeout.
/// Only used by tests to keep the `ConfirmDecision::Timeout` path fast
/// (production always uses 60s via `run_with`).
pub async fn run_with_confirm_timeout(
    config: AppConfig,
    auth: Arc<Auth>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    confirm_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create default generate config from first provider
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

    // Session store manages meta.json + index.json (§7). Shared across all
    // connection handlers so any of them can update a session's metadata
    // when observing Finished events.
    let session_store = Arc::new(SessionStore::new(sessions_dir.clone()));
    session_store.ensure_dir()?;

    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    info!(
        "Parrot daemon starting on {}:{}; token file at {:?}",
        config.daemon.host, config.daemon.port, token_path
    );

    // Phase 1.5: confirmation router bridges `ClientMessage::ConfirmToolCall`
    // responses from any connection handler to the waiting session task. One
    // shared `Arc<ConfirmRouter>` is handed to both the SessionManager (so
    // engine instances get it via `ConfirmConfig`) and each connection
    // handler (so it can call `resolve`).
    let confirm_router = Arc::new(ConfirmRouter::new());
    let confirm_config = ConfirmConfig {
        require_confirmation: config.tools.sandbox.require_confirmation.clone(),
        timeout: confirm_timeout,
        router: Some(Arc::clone(&confirm_router)),
    };

    // Session manager is shared across connections via RwLock
    let session_manager = Arc::new(RwLock::new(
        SessionManager::new(
            Arc::clone(&tool_registry),
            Arc::clone(&provider_registry),
            default_config,
            sessions_dir,
            working_dir,
        )
        .with_confirm_config(confirm_config),
    ));

    // Create the WS transport server and bind
    let ws_server = WsTransportServer::new(&config.daemon.host, config.daemon.port);
    let listener = ws_server.bind().await?;

    // Accept connections in a loop
    loop {
        match accept_connection(&listener).await {
            Ok(client_conn) => {
                let auth = Arc::clone(&auth);
                let session_manager = Arc::clone(&session_manager);
                let session_store = Arc::clone(&session_store);
                let provider_registry = Arc::clone(&provider_registry);
                let confirm_router = Arc::clone(&confirm_router);

                tokio::spawn(async move {
                    handle_connection(
                        client_conn,
                        auth,
                        session_manager,
                        session_store,
                        provider_registry,
                        confirm_router,
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

                // Capture the model + system_prompt before moving the config
                // into the manager, so we can record them in meta.json. The
                // model falls back to the session manager's default inside
                // `create_session` when `None`; we record `None` here and
                // the meta.json will show an empty model string for sessions
                // that used the default. Acceptable for MVP; a follow-up
                // would thread the resolved model back out of create_session.
                let model_for_meta = proto_config.as_ref().and_then(|c| c.model.clone());
                let system_prompt_for_meta =
                    proto_config.as_ref().and_then(|c| c.system_prompt.clone());

                let mut mgr = session_manager.write().await;
                match mgr.create_session(proto_config).await {
                    Ok(session_id) => {
                        info!("Created session {} for client {}", session_id, client_id);

                        // Determine the provider for the chosen model so we
                        // can record it in meta.json. Falls back to "unknown"
                        // if the model isn't resolvable (e.g. mock test setup
                        // that registered the provider AFTER create_session).
                        let provider_id = provider_registry
                            .resolve(&model_for_meta.clone().unwrap_or_default())
                            .await
                            .map(|p| p.provider_id().to_string())
                            .unwrap_or_else(|| "unknown".to_string());

                        // Persist initial meta.json + index.json entry.
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

                        // Take event receiver for relaying to client
                        let mut event_rx = mgr
                            .take_event_receiver(&session_id)
                            .expect("session just created but receiver missing");
                        drop(mgr);

                        // Spawn event relay task. Also observes Finished
                        // events to update meta.json (§4.4 lifecycle).
                        let sender = client.sender.clone();
                        let store = Arc::clone(&session_store);
                        let model_for_meta = model_for_meta.clone().unwrap_or_default();
                        tokio::spawn(async move {
                            relay_session_events(
                                session_id,
                                &mut event_rx,
                                sender,
                                store,
                                model_for_meta,
                            )
                            .await;
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

                // Phase 1.5: dedicated `ToolList` message replaces the
                // previous `TextDelta`+`Finished` hack for ferrying tool
                // schemas back to the client. See `2026-06-21-parrot-phase-1.5.md`
                // §3.3.
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
                    Ok(entries) => {
                        let _ = client
                            .sender
                            .send(ServerMessage::History {
                                session_id,
                                entries,
                            })
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
            ClientMessage::ListSessions => {
                // Read index.json (single file) and convert each entry to a
                // wire `SessionMeta`. Falls back to `updated_at` for
                // `created_at` when the entry predates the schema upgrade
                // (older index.json files have `created_at: null`).
                match session_store.read_index() {
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
                }
            }
            ClientMessage::ResumeSession { session_id } => {
                match resume_session(
                    session_id,
                    &session_manager,
                    &session_store,
                    &provider_registry,
                )
                .await
                {
                    Ok(()) => {
                        // Take the event receiver for the freshly-spawned
                        // session task and spawn a relay task, same as
                        // CreateSession does.
                        let mut mgr = session_manager.write().await;
                        if let Some(mut event_rx) = mgr.take_event_receiver(&session_id) {
                            drop(mgr);
                            let sender = client.sender.clone();
                            let store = Arc::clone(&session_store);
                            tokio::spawn(async move {
                                relay_session_events(
                                    session_id,
                                    &mut event_rx,
                                    sender,
                                    store,
                                    String::new(),
                                )
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
                // Route the decision to the waiting session task. If no
                // pending confirmation exists (late response, buggy client,
                // or daemon restart mid-confirmation), `resolve` returns
                // false — we log but don't error, since the engine has
                // already timed out and moved on.
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

/// Relay `StreamEvent`s from a session task to a client's WS sender, and
/// observe `Finished` events to update `meta.json` / `index.json`.
///
/// This task is spawned per `CreateSession` and lives until either the
/// session task ends or the client sender breaks (client disconnect). If
/// the client disconnects mid-turn, `meta.json` stops being updated — but
/// `events.log` is still being written by the engine, so session state is
/// not lost; a later `ResumeSession` (Phase 1.5) can reconstruct meta from
/// the event log.
async fn relay_session_events(
    session_id: uuid::Uuid,
    event_rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    sender: tokio::sync::mpsc::Sender<ServerMessage>,
    session_store: Arc<SessionStore>,
    _model_for_meta: String,
) {
    while let Some(event) = event_rx.recv().await {
        // Update meta.json on Finish events (both EndTurn and Aborted).
        if let StreamEvent::Finish {
            stop_reason: _,
            usage,
        } = &event
        {
            if let Err(e) = session_store.update_meta(session_id, |m| {
                m.updated_at = chrono::Utc::now();
                m.total_tokens = m
                    .total_tokens
                    .saturating_add((usage.input_tokens + usage.output_tokens) as u64);
            }) {
                warn!("Failed to update meta.json for {}: {}", session_id, e);
            }
        }

        let msg = SessionAdapter::stream_event_to_server(session_id, event);
        if sender.send(msg).await.is_err() {
            break;
        }
    }
}

/// Aggregate models from all registered providers, converting core's
/// `ModelInfo` to the protocol wire type. Called in response to
/// `ClientMessage::ListModels`.
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

/// Read a session's event log from disk and return the entries for
/// `ServerMessage::History`. If the session directory doesn't exist, returns
/// an empty vec (the caller can decide whether to treat that as an error).
async fn read_history(
    session_manager: &RwLock<SessionManager>,
    session_store: &SessionStore,
    session_id: uuid::Uuid,
) -> Result<Vec<parrot_protocol::types::EventLogEntryWithMeta>, String> {
    // Validate that the session exists in the manager OR on disk. A session
    // that was created in a previous daemon lifetime won't be in the manager
    // but its event log is still on disk — we want GetHistory to work for
    // both.
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

    // The EventLog path is `{sessions_dir}/{session_id}/events.log`. We
    // don't have direct access to the engine's EventLog instance (it's
    // owned by the spawned task), so we construct a throwaway EventLog
    // pointing at the same directory purely to call `replay()`.
    //
    // The sessions_dir is the same one the SessionStore was constructed
    // with — recover it from session_store's layout. Since SessionStore
    // doesn't expose its dir, we derive it from the meta path: read_meta
    // knows the layout but doesn't return it. Instead, we read via a
    // helper that reconstructs the path.
    let sessions_dir = session_store.sessions_dir();
    let session_dir = sessions_dir.join(session_id.to_string());
    let log = EventLog::new(session_dir);
    log.replay().map_err(|e| format!("replay: {e}"))
}

/// Phase 1.5: resume a previously-persisted session. Steps:
///   1. If the session is already live in the `SessionManager`, do nothing
///      (the caller still gets `SessionResumed` and can send `Chat` to it).
///   2. Otherwise read `meta.json` for model/system_prompt, replay
///      `events.log`, rebuild a `Vec<ChatMessage>`, and call
///      `create_session_with_context` to spawn a fresh engine task with the
///      reconstructed context.
///
/// The replayed context does NOT include the system prompt — the engine
/// injects it at the head from `meta.system_prompt` (or the daemon default
/// if that's `None`). This means a daemon upgrade that changes the default
/// system prompt template also changes the prompt for resumed sessions that
/// were originally created with the default — intentional, per
/// `2026-06-21-parrot-phase-1.5.md` §3.2.
async fn resume_session(
    session_id: uuid::Uuid,
    session_manager: &RwLock<SessionManager>,
    session_store: &SessionStore,
    provider_registry: &ProviderRegistry,
) -> Result<(), String> {
    // Short-circuit: already live.
    if session_manager.read().await.contains(&session_id) {
        return Ok(());
    }

    // Load persisted meta. If missing, the session never existed (or its
    // data dir was wiped) — surface as an error so the client gets
    // `Error{SessionNotFound}`.
    let meta = session_store
        .read_meta(session_id)
        .map_err(|e| format!("read meta: {e}"))?
        .ok_or_else(|| format!("session {session_id} not found on disk"))?;

    // Replay the event log into a context. The system prompt is NOT part of
    // the replayed context — the engine injects it from `meta.system_prompt`.
    let sessions_dir = session_store.sessions_dir();
    let session_dir = sessions_dir.join(session_id.to_string());
    let log = EventLog::new(session_dir);
    let entries = log.replay().map_err(|e| format!("replay: {e}"))?;

    let replayed_context = rebuild_context_from_entries(&entries);

    // Build a GenerateConfig matching the persisted model. Provider-specific
    // settings (temperature, max_tokens) aren't persisted in meta yet — use
    // defaults. A future schema bump can store the full GenerateConfig.
    let gen_config = GenerateConfig {
        model: meta.model.clone(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };

    // Sanity-check the model resolves to a registered provider; otherwise
    // the engine will fail on the next Chat with a confusing "no provider"
    // error. Better to surface it here.
    if provider_registry.resolve(&meta.model).await.is_none() {
        return Err(format!(
            "model '{}' is not registered with any provider; cannot resume session {}",
            meta.model, session_id
        ));
    }

    let mut mgr = session_manager.write().await;
    mgr.create_session_with_context(
        session_id,
        gen_config,
        meta.system_prompt.clone(),
        replayed_context,
    )
    .await
    .map_err(|e| format!("spawn resumed session: {e}"))?;

    Ok(())
}

/// Rebuild a `Vec<ChatMessage>` from a replayed event log. This is the
/// inverse of the engine's append path: each `EventLogEntry` maps to one or
/// more `ChatMessage`s. `SessionCreated` is skipped (the engine injects the
/// system prompt separately). `Finish` produces no context message (it's
/// metadata only). Tool-call entries reconstruct the assistant message's
/// `tool_calls` field by grouping consecutive `ToolCall` entries followed by
/// `ToolResult` entries.
///
/// MVP simplification: we model each `ToolCall` + its `ToolResult` as two
/// separate messages (an Assistant with `tool_calls: Some`, then a Tool
/// message). This matches how the engine appends them and how Anthropic
/// expects tool_use/tool_result blocks paired. Multi-tool turns (parallel
/// tool calls) require grouping all ToolCalls before their ToolResults —
/// the event log interleaves them per-tool, so we collect all ToolCalls
/// up to the first ToolResult, then emit a single assistant message with
/// all tool_calls, then the Tool messages.
fn rebuild_context_from_entries(
    entries: &[parrot_protocol::types::EventLogEntryWithMeta],
) -> Vec<ChatMessage> {
    use parrot_protocol::types::EventLogEntry;

    let mut context = Vec::new();
    // Accumulate tool calls until we see the first ToolResult for this
    // turn, then flush as a single assistant message. This handles both
    // single-tool and parallel-multi-tool turns.
    let mut pending_tool_calls: Vec<parrot_core::types::ToolCallInfo> = Vec::new();
    let mut pending_assistant_text: Option<String> = None;

    for entry in entries {
        match &entry.entry {
            EventLogEntry::SessionCreated { .. } => {
                // System prompt is injected by the engine from meta.json.
            }
            EventLogEntry::UserMessage { content } => {
                context.push(ChatMessage {
                    role: ChatRole::User,
                    content: content.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                });
            }
            EventLogEntry::AssistantText { content } => {
                // If we're accumulating tool calls, this text is the
                // pre-amble of the same assistant turn — hold it to emit
                // alongside the tool_calls. Otherwise it's a standalone
                // assistant message.
                if pending_tool_calls.is_empty() {
                    // Could be a standalone assistant message OR the
                    // preamble before tool calls in the same turn. We
                    // can't tell yet — buffer it.
                    pending_assistant_text = Some(
                        pending_assistant_text
                            .take()
                            .map_or_else(|| content.clone(), |prev| format!("{prev}\n{content}")),
                    );
                } else {
                    // Text after tool calls in the same turn is unusual;
                    // treat as part of the same assistant message's
                    // content. In practice the engine emits AssistantText
                    // before ToolCall entries.
                    pending_assistant_text = Some(
                        pending_assistant_text
                            .take()
                            .map_or_else(|| content.clone(), |prev| format!("{prev}\n{content}")),
                    );
                }
            }
            EventLogEntry::ToolCall {
                tool_id,
                tool_name,
                arguments,
            } => {
                pending_tool_calls.push(parrot_core::types::ToolCallInfo {
                    id: tool_id.clone(),
                    name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
            }
            EventLogEntry::ToolResult { tool_id, output } => {
                // First ToolResult for this turn: flush the accumulated
                // assistant message (text + all tool_calls).
                if !pending_tool_calls.is_empty() {
                    context.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: pending_assistant_text.take().unwrap_or_default(),
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Some(std::mem::take(&mut pending_tool_calls)),
                    });
                }
                context.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: output.content.clone(),
                    tool_call_id: Some(tool_id.clone()),
                    tool_name: None,
                    tool_calls: None,
                });
            }
            EventLogEntry::Finish { .. } => {
                // If there's buffered assistant text with no tool calls,
                // flush it as a standalone assistant message.
                if !pending_tool_calls.is_empty() {
                    context.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: pending_assistant_text.take().unwrap_or_default(),
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Some(std::mem::take(&mut pending_tool_calls)),
                    });
                } else if let Some(text) = pending_assistant_text.take() {
                    context.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: text,
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: None,
                    });
                }
            }
        }
    }

    // Flush any trailing buffered content (shouldn't happen if the log
    // always ends with Finish, but be defensive).
    if !pending_tool_calls.is_empty() {
        context.push(ChatMessage {
            role: ChatRole::Assistant,
            content: pending_assistant_text.take().unwrap_or_default(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Some(std::mem::take(&mut pending_tool_calls)),
        });
    } else if let Some(text) = pending_assistant_text {
        context.push(ChatMessage {
            role: ChatRole::Assistant,
            content: text,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });
    }

    context
}
