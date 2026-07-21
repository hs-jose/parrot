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
///
/// 启用 `EnableMouseCapture` 以接收鼠标滚轮事件用于翻页；代价是终端
/// 原生的文本选择会被劫持（Windows Terminal 尤其明显）。如需复制文本，
/// 可用键盘中断后从滚动回看（buffer 仍在），后续如需兼顾可加按键复制。
pub(crate) fn spawn_input_thread(tx: mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()> {
    // 启用鼠标捕获以接收滚轮。
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
                    let up = matches!(m.kind, crossterm::event::MouseEventKind::ScrollUp);
                    let down = matches!(m.kind, crossterm::event::MouseEventKind::ScrollDown);
                    if up && tx.blocking_send(UiEvent::MouseScrollUp).is_err() {
                        break;
                    }
                    if down && tx.blocking_send(UiEvent::MouseScrollDown).is_err() {
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

/// 退出时关闭鼠标捕获，恢复终端原生选择能力（程序一旦退出即可正常选复制）。
pub(crate) struct MouseGuard;
impl Drop for MouseGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}
