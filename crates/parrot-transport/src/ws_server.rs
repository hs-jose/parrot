use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::TransportError;
use crate::traits::{ClientConnection, TransportServer};

const CHANNEL_BUFFER: usize = 256;

pub struct WsTransportServer {
    addr: std::net::SocketAddr,
}

impl WsTransportServer {
    pub fn new(host: &str, port: u16) -> Self {
        let addr = format!("{host}:{port}")
            .parse()
            .expect("invalid bind address");
        Self { addr }
    }
}

#[async_trait]
impl TransportServer for WsTransportServer {
    async fn bind(&self) -> Result<tokio::net::TcpListener, TransportError> {
        let listener = tokio::net::TcpListener::bind(self.addr).await?;
        info!("WS server listening on {}", self.addr);
        Ok(listener)
    }
}

/// Accept a single connection on the given listener and upgrade to WebSocket.
/// Call in a loop after binding.
pub async fn accept_connection(
    listener: &tokio::net::TcpListener,
) -> Result<ClientConnection, TransportError> {
    let (stream, remote_addr) = listener.accept().await?;
    info!("New connection from {}", remote_addr);

    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    let (ws_sink, ws_stream) = ws_stream.split();

    let (client_tx, mut client_rx) = mpsc::channel::<ServerMessage>(CHANNEL_BUFFER);
    let (server_tx, server_rx) = mpsc::channel::<ClientMessage>(CHANNEL_BUFFER);

    let client_id = Uuid::new_v4();

    // Spawn writer task: server messages -> WebSocket
    let write_id = client_id;
    tokio::spawn(async move {
        let mut ws_sink = ws_sink;
        while let Some(msg) = client_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    warn!("Serialize error for client {}: {}", write_id, e);
                    continue;
                }
            };
            if ws_sink
                .send(tungstenite::Message::Text(json.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Spawn reader task: WebSocket -> client messages
    let read_id = client_id;
    tokio::spawn(async move {
        let mut ws_stream = ws_stream;
        while let Some(Ok(msg)) = ws_stream.next().await {
            match msg {
                tungstenite::Message::Text(text) => {
                    let client_msg: ClientMessage = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("Deserialize error from client {}: {}", read_id, e);
                            continue;
                        }
                    };
                    if server_tx.send(client_msg).await.is_err() {
                        break;
                    }
                }
                tungstenite::Message::Close(_) => break,
                _ => {}
            }
        }
    });

    Ok(ClientConnection {
        id: client_id,
        sender: client_tx,
        receiver: server_rx,
    })
}