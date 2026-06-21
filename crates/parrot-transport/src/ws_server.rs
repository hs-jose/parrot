use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::accept_hdr_async;
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

/// Check the `Origin` header on the WebSocket upgrade request.
///
/// Per the security model: reject non-empty origins that are not localhost.
/// An absent Origin is allowed (our own CLI client doesn't send one). This
/// defends against DNS-rebinding and browser CSRF attacks, where a malicious
/// web page would issue a cross-origin WS handshake carrying its own Origin.
///
/// Allowed when present: `http://localhost:*`, `http://127.0.0.1:*`.
fn origin_allowed(origin: &str) -> bool {
    let (scheme, rest) = match origin.split_once("://") {
        Some(pair) => pair,
        None => return false, // malformed origin
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host = rest.split('/').next().unwrap_or(rest);
    let (hostname, _port) = match host.split_once(':') {
        Some(pair) => pair,
        None => (host, ""),
    };
    hostname == "localhost" || hostname == "127.0.0.1" || hostname == "::1"
}

/// Accept a single connection on the given listener and upgrade to WebSocket.
/// Call in a loop after binding.
///
/// Validates the `Origin` header during the WS handshake (see `origin_allowed`).
/// Rejected origins yield `TransportError::OriginRejected`.
pub async fn accept_connection(
    listener: &tokio::net::TcpListener,
) -> Result<ClientConnection, TransportError> {
    let (stream, remote_addr) = listener.accept().await?;
    info!("New connection from {}", remote_addr);

    let origin_check = |req: &Request, response: Response| {
        match req.headers().get("Origin").and_then(|v| v.to_str().ok()) {
            None => Ok(response), // no origin header — allow (local CLI doesn't send one)
            Some(origin) if origin_allowed(origin) => Ok(response),
            Some(origin) => {
                warn!("Rejecting WS upgrade from {}: bad origin {:?}", remote_addr, origin);
                // Build a 403 response body to signal rejection.
                let body = format!("Origin not allowed: {origin}\n");
                let reject = Response::builder()
                    .status(403)
                    .body(Some(body))
                    .expect("valid 403 response");
                Err(reject)
            }
        }
    };

    let ws_stream = match accept_hdr_async(stream, origin_check).await {
        Ok(s) => s,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            // Our callback returns a 403 when the Origin is rejected; the
            // handshake surfaces that as an Http error carrying the response.
            let status = resp.status().as_u16();
            return Err(TransportError::OriginRejected(format!(
                "WS handshake rejected (status {status})"
            )));
        }
        Err(e) => return Err(TransportError::WsError(e)),
    };
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