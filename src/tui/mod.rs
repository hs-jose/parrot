pub(crate) mod app;
pub(crate) mod confirm;
pub(crate) mod input;
pub(crate) mod ui;

#[cfg(test)]
mod replay_test;

use std::io::Stdout;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use parrot_protocol::types::ConfirmDecision;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use crate::conn::Connection;
use crate::tui::app::Mode;
use crate::tui::input::{spawn_input_thread, MouseGuard, UiEvent};

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

pub(crate) async fn run_tui(
    mut conn: Connection,
    session_id: SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = app::App::new(session_id);

    // Load prior conversation history for resumed sessions.
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
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input_area = TextArea::default();

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(128);
    let _input_handle = spawn_input_thread(ui_tx);
    let _mouse_guard = MouseGuard;

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

    // teardown (best-effort; don't mask run_loop's error)
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
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
                    UiEvent::Resize(_, _) => *dirty = true,
                    UiEvent::Paste(s) => {
                        for c in s.chars() {
                            input.insert_char(c);
                        }
                        *dirty = true;
                    }
                    UiEvent::Key(k) => {
                        if let Some(should_quit) = handle_key(k, app, input, conn).await? {
                            if should_quit {
                                break;
                            }
                        }
                        *dirty = true;
                    }
                    UiEvent::MouseScrollUp => {
                        app.scroll_up(3);
                        *dirty = true;
                    }
                    UiEvent::MouseScrollDown => {
                        app.scroll_down(3);
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
                // 50ms tick for cursor/refresh even when no events arrive
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
            KeyCode::Enter => {
                let text = input.lines().join("\n");
                if !text.trim().is_empty() {
                    *input = TextArea::default();
                    conn.sender
                        .send(ClientMessage::Chat {
                            session_id: app.session_id,
                            message: text,
                        })
                        .await?;
                }
                Ok(None)
            }
            KeyCode::PageUp => {
                app.scroll_up(10);
                Ok(None)
            }
            KeyCode::PageDown => {
                app.scroll_down(10);
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
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Ok(Some(true)),
            _ => {
                // TextArea handles other keys (cursor/backspace/etc.)
                input.input(tui_textarea::Input::from(k));
                Ok(None)
            }
        },
    }
}
