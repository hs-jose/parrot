use crossterm::event::{self, Event, KeyEvent};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) enum UiEvent {
    Key(KeyEvent),
    #[allow(dead_code)]
    Resize(u16, u16),
    Paste(String),
    /// poll/read 出错时发出，主循环据此退出。
    Quit,
}

/// 在独立 std::thread 跑 crossterm 阻塞 `event::poll` + `event::read`，
/// 通过 mpsc::Sender::blocking_send 把 UiEvent 注入主 tokio 循环。
/// Windows 输入可靠性最佳路径（不依赖 event-stream feature）。
///
/// 不启用鼠标捕获：终端保留原生文本选择/复制能力（Windows Terminal 上
/// 尤其重要）。翻页走键盘：PgUp/PgDn（10 行）与 Shift+↑/↓（1 行）。
pub(crate) fn spawn_input_thread(tx: mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).is_err() {
            let _ = tx.blocking_send(UiEvent::Quit);
            break;
        }
        match event::read() {
            Ok(ev) => match ev {
                Event::Key(k) if tx.blocking_send(UiEvent::Key(k)).is_err() => {
                    break;
                }
                Event::Resize(w, h) => match tx.blocking_send(UiEvent::Resize(w, h)) {
                    Ok(()) => {}
                    Err(_) => break,
                },
                Event::Paste(s) => match tx.blocking_send(UiEvent::Paste(s)) {
                    Ok(()) => {}
                    Err(_) => break,
                },
                _ => {}
            },
            Err(_) => {
                let _ = tx.blocking_send(UiEvent::Quit);
                break;
            }
        }
    })
}
