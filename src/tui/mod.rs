pub(crate) mod app;
pub(crate) mod confirm;
pub(crate) mod input;
pub(crate) mod ui;

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

use crate::conn::{create_session, wait_session_resumed, Connection};
use crate::tui::app::Mode;
use crate::tui::input::{spawn_input_thread, UiEvent};

pub(crate) async fn run_tui(mut conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 先发 ListSessions；若空或选 New 则发 CreateSession；若选已有项则发 ResumeSession。
    let session_id = match choose_session(&mut conn).await? {
        SessionChoice::New => create_session(&conn.sender, &mut conn.receiver).await?,
        SessionChoice::Resume(id) => {
            conn.sender
                .send(ClientMessage::ResumeSession { session_id: id })
                .await?;
            wait_session_resumed(&mut conn.receiver).await?
        }
    };

    let mut app = app::App::new(session_id);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
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

    // teardown (best-effort; don't mask run_loop's error)
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    result
}

enum SessionChoice {
    New,
    Resume(SessionId),
}

/// 轻量文本列表（不开 ratatui）。
async fn choose_session(
    conn: &mut Connection,
) -> Result<SessionChoice, Box<dyn std::error::Error>> {
    conn.sender.send(ClientMessage::ListSessions).await?;
    let sessions = loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::SessionList { sessions }) => break sessions,
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("ListSessions error: {}", message).into());
            }
            _ => {}
        }
    };
    if sessions.is_empty() {
        println!("No prior sessions; creating a new one.");
        return Ok(SessionChoice::New);
    }
    loop {
        println!("\nExisting sessions (Ctrl+C to abort):");
        for (i, s) in sessions.iter().enumerate() {
            let title = s.title.clone().unwrap_or_else(|| "-".into());
            println!(
                "  [{:>2}] {} {} {} {}",
                i,
                s.id,
                s.model,
                s.updated_at.format("%Y-%m-%d %H:%M"),
                title
            );
        }
        println!("  [ N]  create a new session");
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(SessionChoice::New);
        }
        let trimmed = line.trim().to_lowercase();
        if trimmed == "n" || trimmed.is_empty() {
            return Ok(SessionChoice::New);
        }
        if let Ok(idx) = trimmed.parse::<usize>() {
            if let Some(s) = sessions.get(idx) {
                return Ok(SessionChoice::Resume(s.id));
            }
        }
        println!("invalid choice; try again.");
    }
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
                }
            }
            Some(msg) = conn.receiver.recv() => {
                let should_quit = app.apply_server_message(msg);
                *dirty = true;
                if should_quit {
                    break;
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
                    app.push_user_input(text.clone());
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
                app.scroll_up(5);
                Ok(None)
            }
            KeyCode::PageDown => {
                app.scroll_down(5);
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
