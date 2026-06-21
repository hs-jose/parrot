use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;
use crate::types::GenerateConfig;
use crate::tool::ToolRegistry;
use crate::provider::ProviderRegistry;
use crate::engine::ReActEngine;
use crate::event_log::StreamEvent;
use std::collections::HashMap;
use std::sync::Arc;
use parrot_protocol::types::SessionConfig as ProtocolSessionConfig;

pub enum SessionCmd {
    Chat { message: String },
    Abort,
}

pub struct SessionHandle {
    pub id: Uuid,
    pub cmd_tx: mpsc::Sender<SessionCmd>,
    pub event_rx: mpsc::Receiver<StreamEvent>,
    pub abort_handle: AbortHandle,
}

pub struct SessionManager {
    sessions: HashMap<Uuid, SessionHandle>,
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    default_config: GenerateConfig,
    data_dir: std::path::PathBuf,
}

impl SessionManager {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        provider_registry: Arc<ProviderRegistry>,
        default_config: GenerateConfig,
        data_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            sessions: HashMap::new(),
            tool_registry,
            provider_registry,
            default_config,
            data_dir,
        }
    }

    pub async fn create_session(
        &mut self,
        config: Option<ProtocolSessionConfig>,
    ) -> Result<Uuid, crate::error::AgentError> {
        let id = Uuid::new_v4();

        let gen_config = match config {
            Some(c) => GenerateConfig {
                model: c.model.unwrap_or_else(|| self.default_config.model.clone()),
                temperature: self.default_config.temperature,
                max_tokens: self.default_config.max_tokens,
                stop_sequences: self.default_config.stop_sequences.clone(),
            },
            None => self.default_config.clone(),
        };

        let session_dir = self.data_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(64);

        let engine = ReActEngine::new(
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.provider_registry),
            gen_config,
            session_dir,
        );

        let abort_handle = tokio::spawn(async move {
            engine.run(cmd_rx, event_tx).await;
        }).abort_handle();

        self.sessions.insert(id, SessionHandle {
            id,
            cmd_tx,
            event_rx,
            abort_handle,
        });

        Ok(id)
    }

    pub fn get_handle(&self, id: &Uuid) -> Option<&SessionHandle> {
        self.sessions.get(id)
    }

    pub fn get_handle_mut(&mut self, id: &Uuid) -> Option<&mut SessionHandle> {
        self.sessions.get_mut(id)
    }

    /// Takes the event receiver for a session, leaving `None` in its place.
    /// This is used by the server to relay events to the client.
    pub fn take_event_receiver(&mut self, id: &Uuid) -> Option<mpsc::Receiver<StreamEvent>> {
        // We need to temporarily remove and reinsert the handle to take the receiver
        let mut handle = self.sessions.remove(id)?;
        let rx = std::mem::replace(&mut handle.event_rx, mpsc::channel::<StreamEvent>(64).1);
        self.sessions.insert(*id, handle);
        Some(rx)
    }

    /// Get tool definitions from the tool registry
    pub async fn list_tool_definitions(&self) -> Vec<crate::tool::ToolDefinition> {
        self.tool_registry.list_definitions().await
    }
}