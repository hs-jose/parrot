use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::{TransportClient, WsTransportClient};
use tokio::sync::mpsc;

pub(crate) struct Connection {
    pub sender: mpsc::Sender<ClientMessage>,
    pub receiver: mpsc::Receiver<ServerMessage>,
    /// 已 create_session 但又没把 id 立即放在调用栈里的可选槽，给 TUI 用。
    pub session_id: Option<SessionId>,
}

pub(crate) fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        return Err(format!("Token file '{}' is empty", path).into());
    }
    Ok(token)
}

pub(crate) async fn connect(
    connect_url: &str,
    token_path: &str,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let token = read_token(token_path)?;
    let client = WsTransportClient::new();
    let conn = client.connect(connect_url, &token).await?;
    Ok(Connection {
        sender: conn.sender,
        receiver: conn.receiver,
        session_id: None,
    })
}

pub(crate) async fn wait_hello(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<String, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(ServerMessage::HelloAck { server_version }) => return Ok(server_version),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error during handshake: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message during handshake: {:?}", msg);
            }
            None => return Err("Connection closed during handshake".into()),
        }
    }
}

pub(crate) async fn create_session(
    sender: &mpsc::Sender<ClientMessage>,
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    sender
        .send(ClientMessage::CreateSession { config: None })
        .await?;
    loop {
        match receiver.recv().await {
            Some(ServerMessage::SessionCreated { session_id }) => return Ok(session_id),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error creating session: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message waiting for session: {:?}", msg);
            }
            None => return Err("Connection closed waiting for session".into()),
        }
    }
}

pub(crate) async fn wait_session_resumed(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(ServerMessage::SessionResumed { session_id }) => return Ok(session_id),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error resuming session: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message waiting for resume: {:?}", msg);
            }
            None => return Err("Connection closed waiting for resume".into()),
        }
    }
}
