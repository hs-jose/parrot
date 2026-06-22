use clap::{Parser, Subcommand};
use parrot_config::AppConfig;
use parrot_protocol::agent_event::{AgentEvent, PersistedAgentEvent};
use parrot_protocol::types::{ConfirmDecision, SessionMeta, ToolDefinitionWire};
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::{TransportClient, WsTransportClient};
use std::io::Write;

#[derive(Parser)]
#[command(name = "parrot", version, about = "Parrot LLM Agent CLI")]
struct Cli {
    #[arg(long, default_value = "ws://127.0.0.1:9876")]
    connect: String,

    #[arg(long)]
    token_file: Option<String>,

    #[arg(short, long)]
    message: Option<String>,

    #[arg(long)]
    simple: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Sessions(SessionsCmd),
    Models,
    Tools,
}

#[derive(Parser)]
struct SessionsCmd {
    #[command(subcommand)]
    action: SessionsAction,
}

#[derive(Subcommand)]
enum SessionsAction {
    List,
    Show { session_id: String },
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

/// Print the streaming response for one chat turn by unwrapping
/// `ServerMessage::AgentEvent` envelopes and rendering the lifecycle events.
/// `interactive` controls how `ToolConfirmRequired` is handled:
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
            Some(ServerMessage::AgentEvent { event }) => {
                match event {
                    AgentEvent::AgentStart { .. } => {
                        // Silent — session already acknowledged above.
                    }
                    AgentEvent::AgentEnd { reason, .. } => {
                        writeln!(stdout, "\n--- Agent ended: {:?} ---", reason)?;
                        stdout.flush()?;
                        return Ok(());
                    }
                    AgentEvent::TurnStart { .. } => {
                        // Silent — the user's prompt was just echoed.
                    }
                    AgentEvent::TurnEnd {
                        stop_reason, usage, ..
                    } => {
                        writeln!(stdout)?;
                        writeln!(
                            stdout,
                            "\n--- Turn end (reason: {:?}, tokens: {}+{}) ---",
                            stop_reason, usage.input_tokens, usage.output_tokens
                        )?;
                        stdout.flush()?;
                        return Ok(());
                    }
                    AgentEvent::MessageStart { .. } => {
                        // Silent.
                    }
                    AgentEvent::MessageDelta { payload, .. } => match payload {
                        parrot_protocol::agent_event::MessageDeltaPayload::TextDelta { delta } => {
                            write!(stdout, "{}", delta)?;
                            stdout.flush()?;
                        }
                        parrot_protocol::agent_event::MessageDeltaPayload::ToolCallStart {
                            tool_name,
                            ..
                        } => {
                            writeln!(stdout, "\n[Calling tool: {}]", tool_name)?;
                            stdout.flush()?;
                        }
                        parrot_protocol::agent_event::MessageDeltaPayload::ToolCallArgsDelta {
                            ..
                        } => {}
                    },
                    AgentEvent::MessageEnd { .. } => {
                        // Silent — text already streamed via deltas.
                    }
                    AgentEvent::ToolStart { .. } => {
                        // Silent — the ToolCallStart delta already announced it.
                    }
                    AgentEvent::ToolUpdate { .. } => {
                        // MVP doesn't render tool progress.
                    }
                    AgentEvent::ToolEnd { result, .. } => {
                        if result.is_error {
                            writeln!(stdout, "[Tool error: {}]", result.content)?;
                            stdout.flush()?;
                        }
                    }
                    AgentEvent::ToolConfirmRequired {
                        tool_call_id,
                        tool_name,
                        arguments,
                        ..
                    } => {
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
                                tool_id: tool_call_id,
                                decision,
                            })
                            .await?;
                    }
                    AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                        writeln!(
                            stdout,
                            "\n[WARNING] Replay integrity issue: {:?} ({} events dropped)",
                            issue.kind, issue.dropped_event_count
                        )?;
                        stdout.flush()?;
                    }
                }
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

async fn read_line_stdin() -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    lines.next_line().await.ok().flatten()
}

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

fn print_history(events: &[PersistedAgentEvent]) {
    if events.is_empty() {
        println!("Session history is empty.");
        return;
    }
    for e in events {
        let kind = match &e.event {
            AgentEvent::AgentStart {
                model,
                provider,
                resumed_from_seq,
                ..
            } => {
                let resume = resumed_from_seq
                    .map(|n| format!(" (resumed from {n})"))
                    .unwrap_or_default();
                format!(
                    "AgentStart(model={}, provider={}{})",
                    model, provider, resume
                )
            }
            AgentEvent::AgentEnd {
                reason,
                total_usage,
                ..
            } => {
                format!(
                    "AgentEnd({:?} in={} out={})",
                    reason, total_usage.input_tokens, total_usage.output_tokens
                )
            }
            AgentEvent::TurnStart { user_message, .. } => {
                format!("TurnStart({})", truncate(user_message, 60))
            }
            AgentEvent::TurnEnd {
                stop_reason, usage, ..
            } => {
                format!(
                    "TurnEnd({:?} in={} out={})",
                    stop_reason, usage.input_tokens, usage.output_tokens
                )
            }
            AgentEvent::MessageStart { .. } => "MessageStart".to_string(),
            AgentEvent::MessageDelta { .. } => "MessageDelta (non-persistent)".to_string(),
            AgentEvent::MessageEnd {
                final_content,
                tool_calls,
                stop_reason,
                ..
            } => {
                format!(
                    "MessageEnd({:?} tools={} {})",
                    stop_reason,
                    tool_calls.len(),
                    truncate(final_content, 60)
                )
            }
            AgentEvent::ToolStart { tool_name, .. } => format!("ToolStart({})", tool_name),
            AgentEvent::ToolUpdate { .. } => "ToolUpdate (non-persistent)".to_string(),
            AgentEvent::ToolEnd {
                tool_call_id,
                result,
                ..
            } => format!(
                "ToolEnd(id={} err={} {})",
                tool_call_id,
                result.is_error,
                truncate(&result.content, 60)
            ),
            AgentEvent::ToolConfirmRequired { tool_name, .. } => {
                format!("ToolConfirmRequired({})", tool_name)
            }
            AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                format!("ReplayIntegrityWarning({:?})", issue.kind)
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

async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = connect(&cli, config).await?;

    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    eprintln!("Session created: {}", session_id);

    if let Some(message) = cli.message {
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
                    Some(ServerMessage::History { events, .. }) => {
                        print_history(&events);
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
