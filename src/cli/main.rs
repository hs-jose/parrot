use clap::Parser;
use parrot_config::AppConfig;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::{TransportClient, WsTransportClient};
use std::io::Write;

#[derive(Parser)]
#[command(name = "parrot", version, about = "Parrot LLM Agent CLI")]
struct Cli {
    /// Connect to daemon at this URL
    #[arg(long, default_value = "ws://127.0.0.1:9876")]
    connect: String,

    /// Read auth token from this file
    #[arg(long)]
    token_file: Option<String>,

    /// Send one message and exit (non-interactive mode)
    #[arg(long)]
    message: Option<String>,

    /// Use simple stdin input instead of rustyline (for non-TTY environments)
    #[arg(long)]
    simple: bool,
}

fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let token = std::fs::read_to_string(path)?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("Token file '{}' is empty", path).into());
    }
    Ok(token)
}

async fn wait_hello(
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
) -> Result<String, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(ServerMessage::HelloAck { server_version }) => {
                return Ok(server_version);
            }
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error during handshake: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message during handshake: {:?}", msg);
            }
            None => {
                return Err("Connection closed during handshake".into());
            }
        }
    }
}

async fn create_session(
    sender: &tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    sender.send(ClientMessage::CreateSession { config: None }).await?;

    loop {
        match receiver.recv().await {
            Some(ServerMessage::SessionCreated { session_id }) => {
                return Ok(session_id);
            }
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error creating session: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message waiting for session: {:?}", msg);
            }
            None => {
                return Err("Connection closed waiting for session".into());
            }
        }
    }
}

async fn send_chat_and_stream(
    sender: &tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
    session_id: SessionId,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sender
        .send(ClientMessage::Chat {
            session_id,
            message: message.to_string(),
        })
        .await?;

    print_stream(receiver).await
}

async fn print_stream(
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    loop {
        match receiver.recv().await {
            Some(ServerMessage::TextDelta { delta, .. }) => {
                write!(stdout, "{}", delta)?;
                stdout.flush()?;
            }
            Some(ServerMessage::ToolCallStart { tool_name, .. }) => {
                writeln!(stdout, "\n[Calling tool: {}]", tool_name)?;
                stdout.flush()?;
            }
            Some(ServerMessage::ToolCallDelta { .. }) => {}
            Some(ServerMessage::ToolCallEnd { .. }) => {}
            Some(ServerMessage::ToolResult { result, .. }) => {
                if result.is_error {
                    writeln!(stdout, "[Tool error: {}]", result.content)?;
                    stdout.flush()?;
                }
            }
            Some(ServerMessage::Finished { stop_reason, usage, .. }) => {
                writeln!(stdout)?;
                writeln!(
                    stdout,
                    "\n--- Finished (reason: {:?}, tokens: {}+{}) ---",
                    stop_reason, usage.input_tokens, usage.output_tokens
                )?;
                stdout.flush()?;
                return Ok(());
            }
            Some(ServerMessage::Error { message, .. }) => {
                writeln!(stdout, "\nError: {}", message)?;
                stdout.flush()?;
                return Err(message.into());
            }
            Some(msg) => {
                eprintln!("Unexpected stream message: {:?}", msg);
            }
            None => {
                writeln!(stdout, "\nConnection closed.")?;
                return Err("Connection closed unexpectedly".into());
            }
        }
    }
}

/// Read a line from stdin asynchronously using tokio::io::BufReader
async fn read_line_stdin() -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    lines.next_line().await.ok().flatten()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load config to get default token file path
    let config = AppConfig::load()?;

    let token_path = cli
        .token_file
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());

    let token = read_token(&token_path)?;

    // Connect to daemon
    let client = WsTransportClient::new();
    let mut conn = client.connect(&cli.connect, &token).await?;

    // Wait for HelloAck
    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);

    // Create a session
    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    eprintln!("Session created: {}", session_id);

    if let Some(message) = cli.message {
        // Non-interactive mode
        send_chat_and_stream(&conn.sender, &mut conn.receiver, session_id, &message).await?;
    } else {
        // Interactive mode
        let use_rustyline = !cli.simple && atty::is(atty::Stream::Stdin);

        if use_rustyline {
            // Rustyline mode: history, arrow keys, line editing
            let mut rl = rustyline::DefaultEditor::new()?;
            println!("Parrot CLI - type messages and press Enter. Ctrl+D or Ctrl+C to exit.");

            loop {
                let readline = rl.readline("parrot> ");
                match readline {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        rl.add_history_entry(&line).ok();
                        send_chat_and_stream(&conn.sender, &mut conn.receiver, session_id, trimmed).await?;
                    }
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        println!("Ctrl+C");
                        break;
                    }
                    Err(rustyline::error::ReadlineError::Eof) => {
                        println!("Ctrl+D");
                        break;
                    }
                    Err(err) => {
                        eprintln!("Readline error: {}", err);
                        break;
                    }
                }
            }
        } else {
            // Simple stdin mode: no history, no line editing, works in all environments
            println!("Parrot CLI (simple mode) - type messages and press Enter. Ctrl+C to exit.");

            loop {
                eprint!("parrot> ");
                let input = read_line_stdin().await;
                match input {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        send_chat_and_stream(&conn.sender, &mut conn.receiver, session_id, trimmed).await?;
                    }
                    None => break, // EOF
                }
            }
        }
    }

    Ok(())
}