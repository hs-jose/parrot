use crate::auth::Auth;
use crate::session_adapter::SessionAdapter;
use parrot_config::AppConfig;
use parrot_core::provider::ProviderRegistry;
use parrot_core::session::{SessionCmd, SessionManager};
use parrot_core::tool::ToolRegistry;
use parrot_core::types::GenerateConfig;
use parrot_protocol::ClientMessage;
use parrot_protocol::ServerMessage;
use parrot_transport::{WsTransportServer, TransportServer, accept_connection};
use std::sync::Arc;
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
/// providers from config.
pub async fn run_with(
    config: AppConfig,
    auth: Arc<Auth>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create default generate config from first provider
    let default_config = config.providers.first()
        .map(|p| GenerateConfig {
            model: p.default_model.clone(),
            temperature: None,
            max_tokens: Some(8192),
            stop_sequences: None,
        })
        .unwrap_or_default();

    let data_dir = std::path::PathBuf::from(&config.session.data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    info!("Parrot daemon starting on {}:{}; token file at {:?}",
          config.daemon.host, config.daemon.port, token_path);

    // Session manager is shared across connections via RwLock
    let session_manager = Arc::new(RwLock::new(SessionManager::new(
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        default_config,
        data_dir,
        working_dir,
    )));

    // Create the WS transport server and bind
    let ws_server = WsTransportServer::new(&config.daemon.host, config.daemon.port);
    let listener = ws_server.bind().await?;

    // Accept connections in a loop
    loop {
        match accept_connection(&listener).await {
            Ok(client_conn) => {
                let auth = Arc::clone(&auth);
                let session_manager = Arc::clone(&session_manager);

                tokio::spawn(async move {
                    handle_connection(client_conn, auth, session_manager).await;
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
) {
    let client_id = client.id;
    info!("Handling connection from client {}", client_id);

    let mut ctx = ConnectionContext {
        authenticated: false,
    };

    while let Some(msg) = client.receiver.recv().await {
        match msg {
            ClientMessage::Hello { token, client_version } => {
                if auth.validate(&token) {
                    ctx.authenticated = true;
                    info!("Client {} authenticated (version: {})", client_id, client_version);
                    let _ = client.sender.send(ServerMessage::HelloAck {
                        server_version: env!("CARGO_PKG_VERSION").to_string(),
                    }).await;
                } else {
                    warn!("Client {} auth failed", client_id);
                    let _ = client.sender.send(ServerMessage::Error {
                        session_id: None,
                        code: parrot_protocol::types::ErrorCode::AuthFailed,
                        message: "Authentication failed".to_string(),
                    }).await;
                    return;
                }
            }
            _ if !ctx.authenticated => {
                let _ = client.sender.send(ServerMessage::Error {
                    session_id: None,
                    code: parrot_protocol::types::ErrorCode::AuthFailed,
                    message: "Not authenticated".to_string(),
                }).await;
                return;
            }
            ClientMessage::CreateSession { config } => {
                let proto_config = config.map(|c| {
                    parrot_protocol::types::SessionConfig {
                        model: c.model,
                        provider: c.provider,
                        system_prompt: c.system_prompt,
                    }
                });

                let mut mgr = session_manager.write().await;
                match mgr.create_session(proto_config).await {
                    Ok(session_id) => {
                        info!("Created session {} for client {}", session_id, client_id);

                        // Take event receiver for relaying to client
                        let mut event_rx = mgr.take_event_receiver(&session_id)
                            .expect("session just created but receiver missing");
                        drop(mgr);

                        // Spawn event relay task
                        let sender = client.sender.clone();
                        tokio::spawn(async move {
                            while let Some(event) = event_rx.recv().await {
                                let msg = SessionAdapter::stream_event_to_server(session_id, event);
                                if sender.send(msg).await.is_err() {
                                    break;
                                }
                            }
                        });

                        let _ = client.sender.send(ServerMessage::SessionCreated { session_id }).await;
                    }
                    Err(e) => {
                        error!("Failed to create session for client {}: {}", client_id, e);
                        let _ = client.sender.send(ServerMessage::Error {
                            session_id: None,
                            code: parrot_protocol::types::ErrorCode::InternalError,
                            message: format!("Failed to create session: {}", e),
                        }).await;
                    }
                }
            }
            ClientMessage::Chat { session_id, message } => {
                let mgr = session_manager.read().await;
                match mgr.get_handle(&session_id) {
                    Some(handle) => {
                        if let Err(e) = handle.cmd_tx.send(SessionCmd::Chat { message }).await {
                            error!("Failed to send chat command to session {}: {}", session_id, e);
                        }
                    }
                    None => {
                        let _ = client.sender.send(ServerMessage::Error {
                            session_id: Some(session_id),
                            code: parrot_protocol::types::ErrorCode::SessionNotFound,
                            message: "Session not found".to_string(),
                        }).await;
                    }
                }
            }
            ClientMessage::Abort { session_id } => {
                let mgr = session_manager.read().await;
                match mgr.get_handle(&session_id) {
                    Some(handle) => {
                        if let Err(e) = handle.cmd_tx.send(SessionCmd::Abort).await {
                            error!("Failed to send abort command to session {}: {}", session_id, e);
                        }
                    }
                    None => {
                        let _ = client.sender.send(ServerMessage::Error {
                            session_id: Some(session_id),
                            code: parrot_protocol::types::ErrorCode::SessionNotFound,
                            message: "Session not found".to_string(),
                        }).await;
                    }
                }
            }
            ClientMessage::ListModels => {
                let _ = client.sender.send(ServerMessage::Error {
                    session_id: None,
                    code: parrot_protocol::types::ErrorCode::InvalidRequest,
                    message: "ListModels not yet implemented".to_string(),
                }).await;
            }
            ClientMessage::ListTools { session_id } => {
                let mgr = session_manager.read().await;
                let defs = mgr.list_tool_definitions().await;
                drop(mgr);

                // Send tool definitions as a JSON text delta then finish
                let json = serde_json::to_string_pretty(&defs).unwrap_or_default();
                let _ = client.sender.send(ServerMessage::TextDelta {
                    session_id,
                    delta: json,
                }).await;
                let _ = client.sender.send(ServerMessage::Finished {
                    session_id,
                    stop_reason: parrot_protocol::types::StopReason::EndTurn,
                    usage: parrot_protocol::types::Usage { input_tokens: 0, output_tokens: 0 },
                }).await;
            }
            ClientMessage::GetHistory { session_id } => {
                let _ = client.sender.send(ServerMessage::Error {
                    session_id: Some(session_id),
                    code: parrot_protocol::types::ErrorCode::InvalidRequest,
                    message: "GetHistory not yet implemented".to_string(),
                }).await;
            }
        }
    }

    info!("Client {} disconnected", client_id);
}