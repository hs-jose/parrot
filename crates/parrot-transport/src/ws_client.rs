use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{info, warn};
use uuid::Uuid;

use crate::error::TransportError;
use crate::traits::{ServerConnection, TransportClient};

const CHANNEL_BUFFER: usize = 256;

pub struct WsTransportClient;

impl Default for WsTransportClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WsTransportClient {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TransportClient for WsTransportClient {
    async fn connect(&self, url: &str, token: &str) -> Result<ServerConnection, TransportError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url).await?;
        let (ws_sink, ws_stream) = ws_stream.split();

        // client_tx/client_rx: client sends ClientMessages, writer task reads them and writes to WS
        // server_tx/server_rx: reader task reads ServerMessages from WS and sends them here
        let (client_tx, mut client_rx) = mpsc::channel::<ClientMessage>(CHANNEL_BUFFER);
        let (server_tx, server_rx) = mpsc::channel::<ServerMessage>(CHANNEL_BUFFER);

        let client_id = Uuid::new_v4();
        info!("Connected to {} as client {}", url, client_id);

        // Send Hello message immediately
        let hello = ClientMessage::Hello {
            token: token.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let hello_json = serde_json::to_string(&hello)?;

        // Spawn writer task: client messages -> WebSocket
        let write_id = client_id;
        tokio::spawn(async move {
            let mut ws_sink = ws_sink;
            // Send Hello first
            if ws_sink
                .send(tungstenite::Message::Text(hello_json.into()))
                .await
                .is_err()
            {
                return;
            }
            while let Some(msg) = client_rx.recv().await {
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        warn!("Client serialize error {}: {}", write_id, e);
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

        // Spawn reader task: WebSocket -> server messages
        let read_id = client_id;
        tokio::spawn(async move {
            let mut ws_stream = ws_stream;
            while let Some(Ok(msg)) = ws_stream.next().await {
                match msg {
                    tungstenite::Message::Text(text) => {
                        let server_msg: ServerMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("Client deserialize error {}: {}", read_id, e);
                                continue;
                            }
                        };
                        if server_tx.send(server_msg).await.is_err() {
                            break;
                        }
                    }
                    tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // ServerConnection.sender: send ClientMessages to the server
        // ServerConnection.receiver: receive ServerMessages from the server
        Ok(ServerConnection {
            id: client_id,
            sender: client_tx,
            receiver: server_rx,
        })
    }
}
