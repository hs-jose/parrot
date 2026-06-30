use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) enum UiEvent {
    Key(KeyEvent),
    #[allow(dead_code)]
    Resize(u16, u16),
    Paste(String),
    MouseScrollUp,
    MouseScrollDown,
    /// Ctrl+C 或 poll/read 出错时发出，主循环据此退出。
    Quit,
}

/// 在独立 std::thread 跑 crossterm 阻塞 `event::poll` + `event::read`，
/// 通过 mpsc::Sender::blocking_send 把 UiEvent 注入主 tokio 循环。
/// Windows 输入可靠性最佳路径（不依赖 event-stream feature）。
pub(crate) fn spawn_input_thread(tx: mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()> {
    // Enable mouse capture so we receive scroll wheel events.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);

    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).is_err() {
            let _ = tx.blocking_send(UiEvent::Quit);
            break;
        }
        match event::read() {
            Ok(ev) => match ev {
                Event::Key(k) => {
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        let _ = tx.blocking_send(UiEvent::Quit);
                        break;
                    }
                    if tx.blocking_send(UiEvent::Key(k)).is_err() {
                        break;
                    }
                }
                Event::Resize(w, h) => match tx.blocking_send(UiEvent::Resize(w, h)) {
                    Ok(()) => {}
                    Err(_) => break,
                },
                Event::Paste(s) => match tx.blocking_send(UiEvent::Paste(s)) {
                    Ok(()) => {}
                    Err(_) => break,
                },
                Event::Mouse(m) => {
                    let scroll_up = matches!(m.kind, crossterm::event::MouseEventKind::ScrollUp);
                    let scroll_down =
                        matches!(m.kind, crossterm::event::MouseEventKind::ScrollDown);
                    if scroll_up && tx.blocking_send(UiEvent::MouseScrollUp).is_err() {
                        break;
                    }
                    if scroll_down && tx.blocking_send(UiEvent::MouseScrollDown).is_err() {
                        break;
                    }
                }
                _ => {}
            },
            Err(_) => {
                let _ = tx.blocking_send(UiEvent::Quit);
                break;
            }
        }
    })
}

/// Disable mouse capture on drop to restore terminal state.
pub(crate) struct MouseGuard;
impl Drop for MouseGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}
