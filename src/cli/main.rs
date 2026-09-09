mod conn;
mod daemon;
mod stream;
#[path = "../tool_display.rs"]
mod tool_display;
#[path = "../tui/mod.rs"]
mod tui;

use clap::{Parser, Subcommand};
use parrot_config::AppConfig;
use parrot_protocol::agent_event::{AgentEvent, PersistedAgentEvent};
use parrot_protocol::types::{SessionMeta, ToolDefinitionWire};
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use std::io::IsTerminal;

use crate::conn::{create_session, expect_msg, open_conn, wait_session_resumed, Connection};
use crate::stream::print_stream;

#[derive(Parser)]
#[command(name = "parrot", version, about = "Parrot LLM Agent CLI")]
struct Cli {
    /// 显式指定要连接的 daemon URL（例如手动起的 `parrotd`）。
    /// 不提供时自动起一个绑定随机端口的 `parrotd` 子进程，退出即终止。
    #[arg(long)]
    connect: Option<String>,

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
    /// 导出某会话的事件日志为 JSON（`Vec<PersistedAgentEvent>`）。
    /// 输出可存盘供 TUI 回放测试使用：
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
                format!(
                    "TurnStart({})",
                    crate::tool_display::truncate_str(user_message, 60)
                )
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
                    crate::tool_display::truncate_str(final_content, 60)
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
                crate::tool_display::truncate_str(&result.content, 60)
            ),
            AgentEvent::ToolConfirmRequired { tool_name, .. } => {
                format!("ToolConfirmRequired({})", tool_name)
            }
            AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                format!("ReplayIntegrityWarning({:?})", issue.kind)
            }
            AgentEvent::HookFired { hook_id, .. } => {
                format!("HookFired({})", hook_id)
            }
            AgentEvent::CompactionStart { .. } => "CompactionStart".to_string(),
            AgentEvent::CompactionSummary {
                dropped_message_count,
                kept_message_count,
                ..
            } => {
                format!(
                    "CompactionSummary(dropped={} kept={})",
                    dropped_message_count, kept_message_count
                )
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

/// 解析 daemon 连接 URL。给了 `--connect` 就原样使用（调用方自行保证
/// daemon 已在运行）；否则在随机本地端口 spawn 一个全新的 parrotd 子
/// 进程，由返回的 `DaemonChild` 的 kill-on-drop 守卫负责清理。
async fn resolve_connect(
    cli: &Cli,
    config: &AppConfig,
) -> Result<(String, Option<crate::daemon::DaemonChild>), Box<dyn std::error::Error>> {
    match &cli.connect {
        Some(url) => Ok((url.clone(), None)),
        None => {
            let (url, guard) = crate::daemon::ensure_running(config).await?;
            Ok((url, Some(guard)))
        }
    }
}

async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let (connect_url, _guard) = resolve_connect(&cli, config).await?;
    let mut conn = open_conn(&cli, config, &connect_url).await?;

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
    let (connect_url, _guard) = resolve_connect(&cli, config).await?;
    match action {
        SessionsAction::List => {
            let mut conn = open_conn(&cli, config, &connect_url).await?;
            conn.sender.send(ClientMessage::ListSessions).await?;
            let sessions = expect_msg(&mut conn.receiver, "listing sessions", |m| match m {
                ServerMessage::SessionList { sessions } => Some(sessions.clone()),
                _ => None,
            })
            .await?;
            print_session_table(&sessions);
            Ok(())
        }
        SessionsAction::Show { session_id } => {
            let id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = open_conn(&cli, config, &connect_url).await?;
            conn.sender
                .send(ClientMessage::GetHistory { session_id: id })
                .await?;
            let events = expect_msg(&mut conn.receiver, "reading history", |m| match m {
                ServerMessage::History { events, .. } => Some(events.clone()),
                _ => None,
            })
            .await?;
            print_history(&events);
            Ok(())
        }
        SessionsAction::Resume { session_id } => {
            let session_id: SessionId = session_id
                .parse()
                .map_err(|e: uuid::Error| format!("invalid session id '{}': {}", session_id, e))?;
            let mut conn = open_conn(&cli, config, &connect_url).await?;
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
            let mut conn = open_conn(&cli, config, &connect_url).await?;
            conn.sender
                .send(ClientMessage::GetHistory { session_id: id })
                .await?;
            let events = expect_msg(&mut conn.receiver, "exporting history", |m| match m {
                ServerMessage::History { events, .. } => Some(events.clone()),
                _ => None,
            })
            .await?;
            let json = serde_json::to_string_pretty(&events)?;
            println!("{}", json);
            Ok(())
        }
    }
}

async fn run_models(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let (connect_url, _guard) = resolve_connect(&cli, config).await?;
    let mut conn = open_conn(&cli, config, &connect_url).await?;
    conn.sender.send(ClientMessage::ListModels).await?;
    let models = expect_msg(&mut conn.receiver, "listing models", |m| match m {
        ServerMessage::ModelList { models } => Some(models.clone()),
        _ => None,
    })
    .await?;
    print_model_list(&models);
    Ok(())
}

async fn run_tools(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let (connect_url, _guard) = resolve_connect(&cli, config).await?;
    let mut conn = open_conn(&cli, config, &connect_url).await?;
    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    conn.sender
        .send(ClientMessage::ListTools { session_id })
        .await?;
    let tools = expect_msg(&mut conn.receiver, "listing tools", |m| match m {
        ServerMessage::ToolList { tools, .. } => Some(tools.clone()),
        _ => None,
    })
    .await?;
    print_tool_list(&tools);
    Ok(())
}
