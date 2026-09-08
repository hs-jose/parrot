pub(crate) mod app;
pub(crate) mod confirm;
pub(crate) mod input;
pub(crate) mod ui;

#[cfg(test)]
mod replay_test;

use std::io::Stdout;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use parrot_protocol::types::ConfirmDecision;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use crate::conn::Connection;
use crate::tui::app::Mode;
use crate::tui::input::{spawn_input_thread, UiEvent};

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
}

pub(crate) async fn run_tui(
    mut conn: Connection,
    session_id: SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = app::App::new(session_id);

    // resume 的会话要先加载历史对话。
    conn.sender
        .send(ClientMessage::GetHistory { session_id })
        .await?;
    loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::History { events, .. }) => {
                app.load_history(&events);
                break;
            }
            Some(ServerMessage::Error { message, .. }) => {
                eprintln!("Failed to load history: {}", message);
                break;
            }
            Some(_) => {}
            None => return Err("Connection closed waiting for history".into()),
        }
    }

    enable_raw_mode()?;
    let _raw_guard = RawModeGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // 键盘增强协议（CSI-u）：让 VS Code 终端 / Windows Terminal 可靠上报
    // Ctrl+Enter / Shift+Enter 等组合键的修饰符（修复"Ctrl+Enter 误发送"）。
    // 不支持的终端会返回错误，忽略即可（退回默认键处理）。
    let _ = execute!(
        stdout,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    );
    let _ = execute!(stdout, crossterm::event::EnableBracketedPaste);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input_area = TextArea::default();

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(128);
    let _input_handle = spawn_input_thread(ui_tx);

    let mut dirty = true;
    let result = run_loop(
        &mut terminal,
        &mut conn,
        &mut app,
        &mut input_area,
        &mut ui_rx,
        &mut dirty,
    )
    .await;

    // 终端状态由 `_raw_guard` 的 Drop 统一恢复（尽力而为，
    // 不掩盖 run_loop 的错误）。
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &mut Connection,
    app: &mut app::App,
    input: &mut TextArea<'_>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    dirty: &mut bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_esc: Option<Instant> = None;
    loop {
        if *dirty {
            terminal.draw(|f| ui::draw(f, app, input))?;
            *dirty = false;
        }
        tokio::select! {
            biased;
            Some(ev) = ui_rx.recv() => {
                match ev {
                    UiEvent::Quit => break,
                    UiEvent::Resize => {
                        let _ = terminal.clear();
                        *dirty = true;
                    }
                    UiEvent::Paste(s) => {
                        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
                        for (i, line) in normalized.split('\n').enumerate() {
                            if i > 0 {
                                input.insert_newline();
                            }
                            for c in line.chars() {
                                input.insert_char(c);
                            }
                        }
                        *dirty = true;
                    }
                    UiEvent::Key(k) => {
                        // 双击 Esc（仅 Normal 模式，500ms 窗口）：模型输出中中断消息
                        // (Abort)，空闲时退出 TUI。键盘增强协议 (CSI-u) 下一次物理按下
                        // 会连发 Press + Release 两个事件，因此必须只对 Press 计数双击，
                        // 并让 Esc 的 Release 静默忽略、不重置 last_esc——否则第二次
                        // Press 永远等到的是已清空状态。
                        if k.code == KeyCode::Esc && app.mode == Mode::Normal {
                            if k.kind == KeyEventKind::Press {
                                let now = Instant::now();
                                let is_double = last_esc.is_some_and(|t| {
                                    now.duration_since(t) < Duration::from_millis(500)
                                });
                                last_esc = None;
                                if is_double {
                                    if app.is_turn_active() {
                                        conn.sender
                                            .send(ClientMessage::Abort {
                                                session_id: app.session_id,
                                            })
                                            .await?;
                                        *dirty = true;
                                        continue;
                                    }
                                    break;
                                } else {
                                    last_esc = Some(now);
                                    app.clear_tool_selection();
                                }
                            }
                            // Esc 的 Release/Repeat：忽略，不动 last_esc。
                        } else {
                            last_esc = None;
                        }
                        if let Some(should_quit) = handle_key(k, app, input, conn).await? {
                            if should_quit {
                                break;
                            }
                        }
                        *dirty = true;
                    }
                }
            }
            msg = conn.receiver.recv() => {
                match msg {
                    Some(m) => {
                        let should_quit = app.apply_server_message(m);
                        *dirty = true;
                        if should_quit {
                            break;
                        }
                    }
                    None => {
                        app.entries.push(app::ChatEntry::Error("Connection closed".into()));
                        *dirty = true;
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // 50ms 心跳：没有事件也保持光标/刷新
                *dirty = true;
            }
        }
    }
    Ok(())
}

async fn handle_key(
    k: KeyEvent,
    app: &mut app::App,
    input: &mut TextArea<'_>,
    conn: &mut Connection,
) -> Result<Option<bool>, Box<dyn std::error::Error>> {
    match app.mode {
        Mode::ConfirmPending => match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some((id, decision)) = app.confirm_decision(ConfirmDecision::Approve) {
                    conn.sender
                        .send(ClientMessage::ConfirmToolCall {
                            session_id: app.session_id,
                            tool_id: id,
                            decision,
                        })
                        .await?;
                }
                Ok(None)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Some((id, decision)) = app.confirm_decision(ConfirmDecision::Reject) {
                    conn.sender
                        .send(ClientMessage::ConfirmToolCall {
                            session_id: app.session_id,
                            tool_id: id,
                            decision,
                        })
                        .await?;
                }
                Ok(None)
            }
            _ => Ok(None),
        },
        Mode::Normal => match k.code {
            // Shift+Enter / Ctrl+Enter / Ctrl+J 插入换行（不同终端对
            // 组合键的上报不一致，三种都接住）。必须放在裸 Enter 分支之前。
            KeyCode::Enter
                if k.modifiers.contains(KeyModifiers::SHIFT)
                    || k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                input.insert_newline();
                Ok(None)
            }
            KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                input.insert_newline();
                Ok(None)
            }
            KeyCode::Enter => {
                let text = input.lines().join("\n");
                // 有选中工具且输入框为空时，Enter 优先切换该条目展开/收起。
                if text.trim().is_empty() && app.selected_tool.is_some() {
                    app.toggle_selected_tool();
                    return Ok(None);
                }
                if !text.trim().is_empty() {
                    *input = TextArea::default();
                    if let Some(should_quit) = handle_command(app, conn, &text).await? {
                        return Ok(Some(should_quit));
                    }
                    conn.sender
                        .send(ClientMessage::Chat {
                            session_id: app.session_id,
                            message: text,
                        })
                        .await?;
                }
                Ok(None)
            }
            // Tab（+Shift 反向）在工具条目间循环选中；原先落入 `_` 分支会往
            // 输入框插入制表符，现改作选中导航。
            KeyCode::Tab => {
                app.select_next_tool(!k.modifiers.contains(KeyModifiers::SHIFT));
                Ok(None)
            }
            KeyCode::PageUp => {
                app.scroll_up((app.view_height / 2).max(1));
                Ok(None)
            }
            KeyCode::PageDown => {
                app.scroll_down((app.view_height / 2).max(1));
                Ok(None)
            }
            KeyCode::Home if k.modifiers.contains(KeyModifiers::CONTROL) => {
                app.scroll_to_top();
                Ok(None)
            }
            KeyCode::End if k.modifiers.contains(KeyModifiers::CONTROL) => {
                app.scroll_to_bottom();
                Ok(None)
            }
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => {
                app.scroll_up(1);
                Ok(None)
            }
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                app.scroll_down(1);
                Ok(None)
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.is_turn_active() {
                    conn.sender
                        .send(ClientMessage::Abort {
                            session_id: app.session_id,
                        })
                        .await?;
                }
                Ok(None)
            }
            _ => {
                // 其余按键交给 TextArea（光标/退格等）
                input.input(ratatui_textarea::Input::from(k));
                Ok(None)
            }
        },
    }
}

/// 处理 `/` 斜杠命令与 `!` shell 命令。返回 `None` 表示不是命令
/// （调用方应把文本当普通消息发送）；`Some(should_quit)` 表示已处理。
async fn handle_command(
    app: &mut app::App,
    conn: &mut Connection,
    text: &str,
) -> Result<Option<bool>, Box<dyn std::error::Error>> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let cmd = rest.trim();
        match cmd {
            "help" => {
                app.entries.push(app::ChatEntry::Info(
                    "可用命令: /help /usage /abort /exit\n!<命令> 在 daemon 执行 shell 命令".into(),
                ));
                Ok(Some(false))
            }
            "usage" => {
                let u = &app.total_usage;
                app.entries.push(app::ChatEntry::Info(format!(
                    "Token 用量: {} in / {} out",
                    u.input_tokens, u.output_tokens
                )));
                Ok(Some(false))
            }
            "abort" => {
                if app.is_turn_active() {
                    conn.sender
                        .send(ClientMessage::Abort {
                            session_id: app.session_id,
                        })
                        .await?;
                } else {
                    app.entries
                        .push(app::ChatEntry::Info("当前没有进行中的轮次".into()));
                }
                Ok(Some(false))
            }
            "exit" => Ok(Some(true)),
            _ => {
                app.entries.push(app::ChatEntry::Info(format!(
                    "未知命令 /{cmd}，输入 /help 查看可用命令"
                )));
                Ok(Some(false))
            }
        }
    } else if let Some(cmd) = trimmed.strip_prefix('!') {
        let command = cmd.trim().to_string();
        if command.is_empty() {
            app.entries
                .push(app::ChatEntry::Info("用法: !<shell 命令>".into()));
        } else {
            app.entries.push(app::ChatEntry::Shell {
                command: command.clone(),
                output: None,
                exit_code: None,
            });
            conn.sender
                .send(ClientMessage::Shell {
                    session_id: app.session_id,
                    command,
                })
                .await?;
        }
        Ok(Some(false))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Connection;
    use tokio::sync::mpsc;

    fn test_conn() -> (Connection, mpsc::Receiver<ClientMessage>) {
        let (client_tx, server_rx) = mpsc::channel(16);
        let (server_tx, client_rx) = mpsc::channel(16);
        let _ = server_tx;
        (
            Connection {
                sender: client_tx,
                receiver: client_rx,
            },
            server_rx,
        )
    }

    #[tokio::test]
    async fn handle_command_shell_sends_shell_message() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());

        let quit = handle_command(&mut app, &mut conn, "!echo hi")
            .await
            .unwrap();
        assert_eq!(quit, Some(false));
        match app.entries.last() {
            Some(app::ChatEntry::Shell {
                command, output, ..
            }) => {
                assert_eq!(command, "echo hi");
                assert!(output.is_none());
            }
            other => panic!("expected Shell entry, got {:?}", other),
        }
        let msg = server_rx.try_recv().unwrap();
        match msg {
            ClientMessage::Shell { command, .. } => assert_eq!(command, "echo hi"),
            other => panic!("expected Shell, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn handle_command_help_pushes_info_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/help").await.unwrap();
        assert_eq!(quit, Some(false));
        assert!(matches!(app.entries.last(), Some(app::ChatEntry::Info(_))));
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_command_exit_requests_quit() {
        let (mut conn, _server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/exit").await.unwrap();
        assert_eq!(quit, Some(true));
    }

    #[tokio::test]
    async fn handle_command_plain_text_returns_none() {
        let (mut conn, _server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let result = handle_command(&mut app, &mut conn, "hello there")
            .await
            .unwrap();
        assert_eq!(result, None);
        assert!(app.entries.is_empty());
    }
}
