use clap::{Parser, Subcommand};
use parrot_config::AppConfig;
use parrot_protocol::types::{ConfirmDecision, SessionMeta, ToolDefinitionWire};
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

    /// Send one message and exit (non-interactive mode). Mutually exclusive
    /// with subcommands. Kept as a top-level flag for backward compatibility
    /// with `parrot -m "..."`.
    #[arg(short, long)]
    message: Option<String>,

    /// Use simple stdin input instead of rustyline (for non-TTY environments)
    #[arg(long)]
    simple: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List and inspect persisted sessions
    Sessions(SessionsCmd),
    /// List available models
    Models,
    /// List available tools
    Tools,
}

#[derive(Parser)]
struct SessionsCmd {
    #[command(subcommand)]
    action: SessionsAction,
}

#[derive(Subcommand)]
enum SessionsAction {
    /// List all sessions known to the daemon
    List,
    /// Show the event-log history of a session
    Show { session_id: String },
    /// Resume a session and enter interactive mode
    Resume { session_id: String },
}

fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        return Err(format!("Token file '{}' is empty", path).into());
    }
    Ok(token)
}

struct Connection {
    sender: tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: tokio::sync::mpsc::Receiver<ServerMessage>,
}

async fn connect(cli: &Cli, config: &AppConfig) -> Result<Connection, Box<dyn std::error::Error>> {
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let token = read_token(&token_path)?;

    let client = WsTransportClient::new();
    let mut conn = client.connect(&cli.connect, &token).await?;

    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);

    Ok(Connection {
        sender: conn.sender,
        receiver: conn.receiver,
    })
}

async fn wait_hello(
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
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

async fn create_session(
    sender: &tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
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

async fn send_chat_and_stream(
    sender: &tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
    session_id: SessionId,
    message: &str,
    interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sender
        .send(ClientMessage::Chat {
            session_id,
            message: message.to_string(),
        })
        .await?;

    print_stream(sender, receiver, session_id, interactive).await
}

/// Print the streaming response for one chat turn. `interactive` controls
/// how `ToolCallConfirmationRequired` is handled:
///   - `true`  → prompt the user on stdin for y/n, send `ConfirmToolCall`
///   - `false` → auto-reject (no operator present to approve dangerous tools)
async fn print_stream(
    sender: &tokio::sync::mpsc::Sender<ClientMessage>,
    receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>,
    session_id: SessionId,
    interactive: bool,
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
            Some(ServerMessage::ToolCallDelta { .. }) | Some(ServerMessage::ToolCallEnd { .. }) => {
            }
            Some(ServerMessage::ToolResult { result, .. }) => {
                if result.is_error {
                    writeln!(stdout, "[Tool error: {}]", result.content)?;
                    stdout.flush()?;
                }
            }
            Some(ServerMessage::ToolCallConfirmationRequired {
                tool_id,
                tool_name,
                arguments,
                ..
            }) => {
                writeln!(
                    stdout,
                    "\n[Confirmation required] tool: {} args: {}",
                    tool_name, arguments
                )?;
                stdout.flush()?;
                let decision = if interactive {
                    prompt_confirm(&mut stdout)?
                } else {
                    writeln!(
                        stdout,
                        "[Non-interactive mode — auto-rejecting confirmation request]"
                    )?;
                    stdout.flush()?;
                    ConfirmDecision::Reject
                };
                sender
                    .send(ClientMessage::ConfirmToolCall {
                        session_id,
                        tool_id,
                        decision,
                    })
                    .await?;
            }
            Some(ServerMessage::Finished {
                stop_reason, usage, ..
            }) => {
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
            // Non-streaming response messages aren't expected during a chat
            // turn; log and keep waiting for the turn's Finished.
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

/// Prompt the user for a y/n confirmation. Reads one line from stdin
/// synchronously — this blocks the stream task, which is acceptable in
/// interactive CLI mode (the user is at a terminal). TUI will use a
/// non-blocking modal instead.
fn prompt_confirm(
    stdout: &mut std::io::StdoutLock,
) -> Result<ConfirmDecision, Box<dyn std::error::Error>> {
    write!(stdout, "approve? (y/n) > ")?;
    stdout.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "y" | "yes" => ConfirmDecision::Approve,
        _ => ConfirmDecision::Reject,
    })
}

/// Read a line from stdin asynchronously using tokio::io::BufReader
async fn read_line_stdin() -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    lines.next_line().await.ok().flatten()
}

/// Run the interactive REPL. Shared by the default path (no subcommand) and
/// `sessions resume`. `use_rustyline` picks the input backend.
async fn run_interactive(
    conn: &mut Connection,
    session_id: SessionId,
    use_rustyline: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if use_rustyline {
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
                    send_chat_and_stream(
                        &conn.sender,
                        &mut conn.receiver,
                        session_id,
                        trimmed,
                        true,
                    )
                    .await?;
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
                    send_chat_and_stream(
                        &conn.sender,
                        &mut conn.receiver,
                        session_id,
                        trimmed,
                        true,
                    )
                    .await?;
                }
                None => break,
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand handlers
// ---------------------------------------------------------------------------

fn print_session_table(sessions: &[SessionMeta]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }
    println!(
        "{:-38}  {:20}  {:20}  {:>10}",
        "ID", "MODEL", "UPDATED", "TOKENS"
    );
    for s in sessions {
        let title = s.title.clone().unwrap_or_else(|| "-".to_string());
        println!(
            "{:-38}  {:20}  {:20}  {:>10}  {}",
            s.id,
            s.model,
            s.updated_at.format("%Y-%m-%d %H:%M"),
            s.total_tokens,
            title
        );
    }
}

fn print_history(entries: &[parrot_protocol::types::EventLogEntryWithMeta]) {
    use parrot_protocol::types::EventLogEntry;
    if entries.is_empty() {
        println!("Session history is empty.");
        return;
    }
    for e in entries {
        let kind = match &e.entry {
            EventLogEntry::SessionCreated { model, provider } => {
                format!("SessionCreated(model={}, provider={})", model, provider)
            }
            EventLogEntry::UserMessage { content } => {
                format!("UserMessage({})", truncate(content, 60))
            }
            EventLogEntry::AssistantText { content } => {
                format!("AssistantText({})", truncate(content, 60))
            }
            EventLogEntry::ToolCall {
                tool_name,
                arguments,
                ..
            } => format!("ToolCall({} args={})", tool_name, arguments),
            EventLogEntry::ToolResult { output, .. } => {
                format!(
                    "ToolResult(err={} {})",
                    output.is_error,
                    truncate(&output.content, 60)
                )
            }
            EventLogEntry::Finish { stop_reason, usage } => {
                format!(
                    "Finish({:?} in={} out={})",
                    stop_reason, usage.input_tokens, usage.output_tokens
                )
            }
        };
        println!("#{:-4} {} {}", e.seq, e.ts.format("%H:%M:%S"), kind);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn print_tool_list(tools: &[ToolDefinitionWire]) {
    if tools.is_empty() {
        println!("No tools registered.");
        return;
    }
    for t in tools {
        println!("{:20}  {}", t.name, t.description);
    }
}

fn print_model_list(models: &[parrot_protocol::types::ModelInfo]) {
    if models.is_empty() {
        println!("No models available.");
        return;
    }
    println!(
        "{:-30}  {:20}  {:>10}  {:>10}",
        "ID", "PROVIDER", "CONTEXT", "MAX-OUT"
    );
    for m in models {
        println!(
            "{:-30}  {:20}  {:>10}  {:>10}",
            m.id, m.provider, m.context_window, m.max_output_tokens
        );
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cli = Cli::parse();
    let config = AppConfig::load()?;

    let command = std::mem::take(&mut cli.command);
    match command {
        None => run_default(cli, &config).await,
        Some(Command::Sessions(SessionsCmd { action })) => run_sessions(action, cli, &config).await,
        Some(Command::Models) => run_models(cli, &config).await,
        Some(Command::Tools) => run_tools(cli, &config).await,
    }
}

/// Default path: `parrot` (no subcommand) or `parrot -m "..."`. Backward
/// compatible with the pre-subcommand CLI.
async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = connect(&cli, config).await?;

    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    eprintln!("Session created: {}", session_id);

    if let Some(message) = cli.message {
        // Non-interactive single message. Confirmations auto-rejected
        // (no operator present).
        send_chat_and_stream(
            &conn.sender,
            &mut conn.receiver,
            session_id,
            &message,
            false,
        )
        .await?;
    } else {
        let use_rustyline = !cli.simple && atty::is(atty::Stream::Stdin);
        run_interactive(&mut conn, session_id, use_rustyline).await?;
    }
    Ok(())
}

async fn run_sessions(
    action: SessionsAction,
    cli: Cli,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        SessionsAction::List => {
            let mut conn = connect(&cli, config).await?;
            conn.sender.send(ClientMessage::ListSessions).await?;
            loop {
                match conn.receiver.recv().await {
                    Some(ServerMessage::SessionList { sessions }) => {
                        print_session_table(&sessions);
                        return Ok(());
                    }
                    Some(ServerMessage::Error { message, .. }) => {
                        return Err(format!("Server error: {}", message).into());
                    }
                    Some(msg) => eprintln!("Unexpected message: {:?}", msg),
                    None => return Err("Connection closed".into()),
                }
            }
        }
        SessionsAction::Show { session_id } => {
            let id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = connect(&cli, config).await?;
            conn.sender
                .send(ClientMessage::GetHistory { session_id: id })
                .await?;
            loop {
                match conn.receiver.recv().await {
                    Some(ServerMessage::History { entries, .. }) => {
                        print_history(&entries);
                        return Ok(());
                    }
                    Some(ServerMessage::Error { message, .. }) => {
                        return Err(format!("Server error: {}", message).into());
                    }
                    Some(msg) => eprintln!("Unexpected message: {:?}", msg),
                    None => return Err("Connection closed".into()),
                }
            }
        }
        SessionsAction::Resume { session_id } => {
            let id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = connect(&cli, config).await?;
            conn.sender
                .send(ClientMessage::ResumeSession { session_id: id })
                .await?;
            loop {
                match conn.receiver.recv().await {
                    Some(ServerMessage::SessionResumed { .. }) => {
                        eprintln!("Resumed session {}", id);
                        let use_rustyline = !cli.simple && atty::is(atty::Stream::Stdin);
                        run_interactive(&mut conn, id, use_rustyline).await?;
                        return Ok(());
                    }
                    Some(ServerMessage::Error { message, .. }) => {
                        return Err(format!("Server error: {}", message).into());
                    }
                    Some(msg) => eprintln!("Unexpected message: {:?}", msg),
                    None => return Err("Connection closed".into()),
                }
            }
        }
    }
}

async fn run_models(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = connect(&cli, config).await?;
    conn.sender.send(ClientMessage::ListModels).await?;
    loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::ModelList { models }) => {
                print_model_list(&models);
                return Ok(());
            }
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error: {}", message).into());
            }
            Some(msg) => eprintln!("Unexpected message: {:?}", msg),
            None => return Err("Connection closed".into()),
        }
    }
}

async fn run_tools(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = connect(&cli, config).await?;
    // ListTools requires a session_id per the protocol. We create a throwaway
    // session to get one, then immediately ask for tools. A future protocol
    // revision could lift the session_id requirement (tools are global to the
    // daemon, not per-session).
    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    conn.sender
        .send(ClientMessage::ListTools { session_id })
        .await?;
    loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::ToolList { tools, .. }) => {
                print_tool_list(&tools);
                return Ok(());
            }
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error: {}", message).into());
            }
            Some(msg) => eprintln!("Unexpected message: {:?}", msg),
            None => return Err("Connection closed".into()),
        }
    }
}
