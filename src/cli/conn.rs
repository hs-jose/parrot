use crate::Cli;
use parrot_config::AppConfig;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::{TransportClient, WsTransportClient};
use tokio::sync::mpsc;

pub(crate) struct Connection {
    pub sender: mpsc::Sender<ClientMessage>,
    pub receiver: mpsc::Receiver<ServerMessage>,
}

/// 通用"收循环直到期望消息"：`extract` 命中的消息（按引用匹配，
/// 由闭包内部 clone）原样返回；`Error` 消息转成带 `context` 前缀的
/// `Err`；其他消息打印警告后继续等；连接关闭报错。
pub(crate) async fn expect_msg<T>(
    receiver: &mut mpsc::Receiver<ServerMessage>,
    context: &str,
    mut extract: impl FnMut(&ServerMessage) -> Option<T>,
) -> Result<T, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(msg) => {
                if let Some(t) = extract(&msg) {
                    return Ok(t);
                }
                match &msg {
                    ServerMessage::Error { message, .. } => {
                        return Err(format!("Server error {}: {}", context, message).into());
                    }
                    _ => eprintln!("Unexpected message during {}: {:?}", context, msg),
                }
            }
            None => return Err(format!("Connection closed during {}", context).into()),
        }
    }
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
    })
}

/// 握手：等 HelloAck 并打印 Connected 提示。
pub(crate) async fn wait_hello(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<(), Box<dyn std::error::Error>> {
    let version = expect_msg(receiver, "handshake", |m| match m {
        ServerMessage::HelloAck { server_version } => Some(server_version.clone()),
        _ => None,
    })
    .await?;
    eprintln!("Connected to server v{}", version);
    Ok(())
}

/// 统一的"连接 + 握手"入口：解析 token 路径 → 连接 → 等 HelloAck。
/// CLI 各子命令共用，新增子命令不再复制握手样板。
pub(crate) async fn open_conn(
    cli: &Cli,
    config: &AppConfig,
    connect_url: &str,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect(connect_url, &token_path).await?;
    wait_hello(&mut conn.receiver).await?;
    Ok(conn)
}

pub(crate) async fn create_session(
    sender: &mpsc::Sender<ClientMessage>,
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    sender
        .send(ClientMessage::CreateSession { config: None })
        .await?;
    expect_msg(receiver, "creating session", |m| match m {
        ServerMessage::SessionCreated { session_id } => Some(*session_id),
        _ => None,
    })
    .await
}

pub(crate) async fn wait_session_resumed(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    expect_msg(receiver, "waiting for resume", |m| match m {
        ServerMessage::SessionResumed { session_id } => Some(*session_id),
        _ => None,
    })
    .await
}
