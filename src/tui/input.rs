use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) enum UiEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    Paste(String),
    /// Ctrl+C 或 poll/read 出错时发出，主循环据此退出。
    Quit,
}

/// 在独立 std::thread 跑 crossterm 阻塞 `event::poll` + `event::read`，
/// 通过 mpsc::Sender::blocking_send 把 UiEvent 注入主 tokio 循环。
/// Windows 输入可靠性最佳路径（不依赖 event-stream feature）。
pub(crate) fn spawn_input_thread(tx: mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).is_err() {
            let _ = tx.blocking_send(UiEvent::Quit);
            return;
        }
        match event::read() {
            Ok(ev) => match ev {
                Event::Key(k) => {
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        let _ = tx.blocking_send(UiEvent::Quit);
                        return;
                    }
                    if tx.blocking_send(UiEvent::Key(k)).is_err() {
                        return;
                    }
                }
                Event::Resize(w, h) => match tx.blocking_send(UiEvent::Resize(w, h)) {
                    Ok(()) => {}
                    Err(_) => return,
                },
                Event::Paste(s) => match tx.blocking_send(UiEvent::Paste(s)) {
                    Ok(()) => {}
                    Err(_) => return,
                },
                _ => {}
            },
            Err(_) => {
                let _ = tx.blocking_send(UiEvent::Quit);
                return;
            }
        }
    })
}
