use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("connection closed")]
    ConnectionClosed,
    #[error("authentication failed")]
    AuthFailed,
    #[error("WebSocket error: {0}")]
    WsError(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection refused: {0}")]
    ConnectionRefused(String),
    #[error("origin rejected: {0}")]
    OriginRejected(String),
}
