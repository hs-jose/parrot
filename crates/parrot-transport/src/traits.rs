use async_trait::async_trait;
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use uuid::Uuid;

pub type ClientId = Uuid;

/// Connection from the server's perspective: a client has connected.
/// The server can send ServerMessages to this client, and receive ClientMessages from it.
pub struct ClientConnection {
    pub id: ClientId,
    pub sender: mpsc::Sender<ServerMessage>,
    pub receiver: mpsc::Receiver<ClientMessage>,
}

/// Connection from the client's perspective: connected to a server.
/// The client can send ClientMessages to the server, and receive ServerMessages from it.
pub struct ServerConnection {
    pub id: ClientId,
    pub sender: mpsc::Sender<ClientMessage>,
    pub receiver: mpsc::Receiver<ServerMessage>,
}

#[async_trait]
pub trait TransportServer: Send + Sync {
    /// Bind the server and return a listener. Call once.
    async fn bind(&self) -> Result<tokio::net::TcpListener, crate::error::TransportError>;
}

#[async_trait]
pub trait TransportClient: Send + Sync {
    async fn connect(
        &self,
        url: &str,
        token: &str,
    ) -> Result<ServerConnection, crate::error::TransportError>;
}
