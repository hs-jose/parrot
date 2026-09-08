use parrot_protocol::agent_event::{AgentEvent, MessageDeltaPayload};
use parrot_protocol::types::ConfirmDecision;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use std::io::Write;

use crate::conn::Connection;

/// 单条消息流式输出。`interactive` 控制遇到 ToolConfirmRequired 时
/// 是否提示用户 stdin y/n；非交互场景（-m / 非 tty）自动 Reject。
pub(crate) async fn print_stream(
    conn: &mut Connection,
    session_id: SessionId,
    interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::AgentEvent { event }) => match event {
                AgentEvent::AgentStart { .. } | AgentEvent::MessageStart { .. } => {}
                AgentEvent::AgentEnd { reason, .. } => {
                    writeln!(stdout, "\n--- Agent ended: {:?} ---", reason)?;
                    stdout.flush()?;
                    return Ok(());
                }
                AgentEvent::TurnStart { .. } => {}
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
                AgentEvent::MessageDelta { payload, .. } => match payload {
                    MessageDeltaPayload::TextDelta { delta } => {
                        write!(stdout, "{}", delta)?;
                        stdout.flush()?;
                    }
                    MessageDeltaPayload::ToolCallStart { tool_name, .. } => {
                        writeln!(stdout, "\n[Calling tool: {}]", tool_name)?;
                        stdout.flush()?;
                    }
                    MessageDeltaPayload::ToolCallArgsDelta { .. } => {}
                },
                AgentEvent::MessageEnd { .. } => {}
                AgentEvent::ToolStart { .. } => {}
                AgentEvent::ToolUpdate { .. } => {}
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
                    conn.sender
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
                AgentEvent::HookFired { .. } => {}
                AgentEvent::CompactionStart { .. } => {}
                AgentEvent::CompactionSummary { .. } => {}
            },
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

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
