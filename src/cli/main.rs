mod conn;
mod stream;
#[path = "../tui/mod.rs"]
mod tui;

use clap::{Parser, Subcommand};
use parrot_config::AppConfig;
use parrot_protocol::agent_event::{AgentEvent, PersistedAgentEvent};
use parrot_protocol::types::{SessionMeta, ToolDefinitionWire};
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use std::io::IsTerminal;

use crate::conn::{
    connect as connect_with_token, create_session, wait_hello, wait_session_resumed, Connection,
};
use crate::stream::print_stream;

#[derive(Parser)]
#[command(name = "parrot", version, about = "Parrot LLM Agent CLI")]
struct Cli {
    #[arg(long, default_value = "ws://127.0.0.1:9876")]
    connect: String,

    #[arg(long)]
    token_file: Option<String>,

    #[arg(short, long)]
    message: Option<String>,

    #[arg(short, long)]
    session: Option<String>,

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
    Show {
        session_id: String,
    },
    Resume {
        session_id: String,
    },
    /// Export a session's event log as JSON (Vec<PersistedAgentEvent>).
    /// Output can be saved to a file for TUI replay tests:
    ///   parrot sessions export <id> > tests/tui_replay/my_fixture.json
    Export {
        session_id: String,
    },
}

async fn run_rustyline_loop(
    conn: &mut Connection,
    session_id: SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
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
                conn.sender
                    .send(ClientMessage::Chat {
                        session_id,
                        message: trimmed.to_string(),
                    })
                    .await?;
                print_stream(&mut *conn, session_id, true).await?;
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
                format!("TurnStart({})", crate::stream::truncate(user_message, 60))
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
                    crate::stream::truncate(final_content, 60)
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
                crate::stream::truncate(&result.content, 60)
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
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect_with_token(&cli.connect, &token_path).await?;
    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);

    let session_id = if let Some(sid_str) = &cli.session {
        let id: SessionId = sid_str
            .parse()
            .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", sid_str, e))?;
        conn.sender
            .send(ClientMessage::ResumeSession { session_id: id })
            .await?;
        wait_session_resumed(&mut conn.receiver).await?
    } else {
        create_session(&conn.sender, &mut conn.receiver).await?
    };

    if let Some(message) = cli.message {
        conn.sender
            .send(ClientMessage::Chat {
                session_id,
                message: message.clone(),
            })
            .await?;
        print_stream(&mut conn, session_id, false).await?;
        return Ok(());
    }

    if cli.simple || !std::io::stdin().is_terminal() {
        run_rustyline_loop(&mut conn, session_id).await?;
        return Ok(());
    }

    tui::run_tui(conn, session_id).await
}

async fn run_sessions(
    action: SessionsAction,
    cli: Cli,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    match action {
        SessionsAction::List => {
            let mut conn = connect_with_token(&cli.connect, &token_path).await?;
            let server_version = wait_hello(&mut conn.receiver).await?;
            eprintln!("Connected to server v{}", server_version);
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
            let mut conn = connect_with_token(&cli.connect, &token_path).await?;
            let server_version = wait_hello(&mut conn.receiver).await?;
            eprintln!("Connected to server v{}", server_version);
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
            let session_id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = connect_with_token(&cli.connect, &token_path).await?;
            let server_version = wait_hello(&mut conn.receiver).await?;
            eprintln!("Connected to server v{}", server_version);
            conn.sender
                .send(ClientMessage::ResumeSession { session_id })
                .await?;
            let id = wait_session_resumed(&mut conn.receiver).await?;
            eprintln!("Resumed session {}", id);
            run_rustyline_loop(&mut conn, id).await?;
            Ok(())
        }
        SessionsAction::Export { session_id } => {
            let id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = connect_with_token(&cli.connect, &token_path).await?;
            let server_version = wait_hello(&mut conn.receiver).await?;
            eprintln!("Connected to server v{}", server_version);
            conn.sender
                .send(ClientMessage::GetHistory { session_id: id })
                .await?;
            loop {
                match conn.receiver.recv().await {
                    Some(ServerMessage::History { events, .. }) => {
                        let json = serde_json::to_string_pretty(&events)?;
                        println!("{}", json);
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
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect_with_token(&cli.connect, &token_path).await?;
    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);
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
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect_with_token(&cli.connect, &token_path).await?;
    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);
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
