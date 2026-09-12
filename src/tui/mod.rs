pub(crate) mod app;
pub(crate) mod confirm;
pub(crate) mod input;
pub(crate) mod slash;
pub(crate) mod ui;

#[cfg(test)]
mod replay_test;

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

/// 键盘增强协议 / Windows 下一次物理按键会连发 Press + Release（Repeat 为
/// 长按连发）。Release 必须整路忽略，否则自定义按键分支会双触发：Enter
/// 展开闪烁、Tab 跳两格、Shift+Enter 双换行。与 ratatui-textarea
/// `Input::from`（上游 #14）的处理一致：只放行 Press / Repeat。
fn key_actionable(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Release
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
    // 不掩盖 run_loop 的错误）。先显式恢复终端，退出提示才不会被
    // 备用屏/裸模式吞掉。
    drop(_raw_guard);
    // 退出时打印完整 session id，便于复制（`parrot --session <id>` 续会话）。
    println!("Session: {session_id}");
    result
}

async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    conn: &mut Connection,
    app: &mut app::App,
    input: &mut TextArea<'_>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    dirty: &mut bool,
) -> Result<(), Box<dyn std::error::Error>>
where
    B::Error: 'static,
{
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
                        slash::sync_slash_popup(app, &input.lines().join("\n"));
                        *dirty = true;
                    }
                    UiEvent::Key(k) => {
                        // Release 一律忽略（见 key_actionable 注释）；Esc 的
                        // 双击计数因此在下方只需处理 Press / Repeat。
                        if !key_actionable(k.kind) {
                            continue;
                        }
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
                        slash::sync_slash_popup(app, &input.lines().join("\n"));
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
        Mode::Normal => {
            // 斜杠补全弹窗存活时优先拦截（Shift 组合不拦，聊天滚动照常）。
            if app.slash_popup.is_some() {
                match k.code {
                    KeyCode::Up if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                        if let Some(p) = app.slash_popup.as_mut() {
                            p.move_up();
                        }
                        return Ok(None);
                    }
                    KeyCode::Down if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                        if let Some(p) = app.slash_popup.as_mut() {
                            p.move_down();
                        }
                        return Ok(None);
                    }
                    KeyCode::Enter
                        if !k.modifiers.contains(KeyModifiers::SHIFT)
                            && !k.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if let Some(cmd) = app.slash_popup.as_ref().and_then(|p| p.selected_cmd()) {
                            *input = TextArea::default();
                            app.slash_popup = None;
                            app.slash_dismissed_query = None;
                            return slash::execute(cmd, None, app, conn).await;
                        }
                        // 无选中项（列表空）→ 不拦截，走普通 Enter 路径
                    }
                    KeyCode::Esc => {
                        app.slash_dismissed_query =
                            slash::popup_query(&input.lines().join("\n")).map(str::to_string);
                        app.slash_popup = None;
                        return Ok(None);
                    }
                    _ => {}
                }
            }
            match k.code {
                // ↓↓↓ 以下为原有分支，原样保留 ↓↓↓
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
                    // Repeat 长按会连发，切换展开必须只认物理按下。
                    if k.kind == KeyEventKind::Press
                        && text.trim().is_empty()
                        && app.selected_tool.is_some()
                    {
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
                // Tab / Shift+Tab 在工具条目间循环选中（后移/前移）。crossterm 0.29
                // 在 Windows 与 Unix 上均把 Shift+Tab 上报为 KeyCode::BackTab 且携带
                // SHIFT 修饰键，故此处不能只在 Tab 分支里检查 SHIFT（那是死代码，
                // BackTab 会落入 `_` 分支往输入框插入制表符）。反向选中靠
                // `!contains(SHIFT)`：BackTab 通常自带 SHIFT；个别终端只上报无修饰
                // 键的 BackTab 时兜底按正向处理。
                KeyCode::Tab | KeyCode::BackTab => {
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
                _ => {
                    // 其余按键交给 TextArea（光标/退格等）
                    input.input(ratatui_textarea::Input::from(k));
                    Ok(None)
                }
            }
        }
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
        let (name, arg) = slash::parse_invocation(rest);
        match slash::find(name) {
            Some(c) => slash::execute(c, arg, app, conn).await,
            None => {
                app.entries.push(app::ChatEntry::Info(format!(
                    "未知命令 /{name}，输入 /help 查看可用命令"
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
    use crate::tui::slash::sync_slash_popup;
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
    async fn handle_command_model_without_arg_pushes_usage_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/model").await.unwrap();
        assert_eq!(quit, Some(false));
        match app.entries.last() {
            Some(app::ChatEntry::Info(s)) => assert!(s.contains("/model <name>"), "{s}"),
            other => panic!("expected Info, got {other:?}"),
        }
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_command_model_with_arg_sends_model_message() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/model gpt-5")
            .await
            .unwrap();
        assert_eq!(quit, Some(false));
        match server_rx.try_recv().unwrap() {
            ClientMessage::Model { session_id, model } => {
                assert_eq!(session_id, app.session_id);
                assert_eq!(model, "gpt-5");
            }
            other => panic!("expected Model, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_command_unknown_pushes_info() {
        let (mut conn, _server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/nope").await.unwrap();
        assert_eq!(quit, Some(false));
        assert!(matches!(app.entries.last(), Some(app::ChatEntry::Info(_))));
    }

    #[test]
    fn key_actionable_ignores_only_release() {
        use crossterm::event::KeyEventKind;
        assert!(key_actionable(KeyEventKind::Press));
        assert!(key_actionable(KeyEventKind::Repeat));
        assert!(!key_actionable(KeyEventKind::Release));
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

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// 弹窗激活态的测试环境：输入 "/’" 并 sync。rx 一并返回以保持
    /// channel 另一端存活（避免 conn.sender.send 失败）。
    async fn popup_app() -> (
        app::App,
        Connection,
        TextArea<'static>,
        mpsc::Receiver<ClientMessage>,
    ) {
        let (conn, rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let mut input = TextArea::default();
        for c in "/".chars() {
            input.insert_char(c);
        }
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        (app, conn, input, rx)
    }

    #[tokio::test]
    async fn popup_down_up_move_selection_not_scroll() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        assert_eq!(app.slash_popup.as_ref().unwrap().selected(), 1);
        assert_eq!(app.scroll_offset, 0, "弹窗存活时 Down 不应滚动聊天区");
        handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        assert_eq!(app.slash_popup.as_ref().unwrap().selected(), 0);
    }

    #[tokio::test]
    async fn popup_down_then_sync_keeps_selection() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        // 复刻 run_loop Key 分支的真实顺序：按键后 sync 同文本
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        assert_eq!(
            app.slash_popup.as_ref().unwrap().selected(),
            1,
            "真实循环中 Down 移动后 sync 不得重置选中"
        );
    }

    #[tokio::test]
    async fn popup_shift_up_still_scrolls_chat() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        handle_key(
            key(KeyCode::Up, KeyModifiers::SHIFT),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        assert_eq!(app.scroll_offset, 1, "Shift+Up 不被弹窗拦截，仍滚动聊天区");
    }

    #[tokio::test]
    async fn popup_enter_executes_and_clears_input() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        for c in "exit".chars() {
            input.insert_char(c);
        }
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        let quit = handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        assert_eq!(quit, Some(true), "/exit 应请求退出");
        assert!(input.lines()[0].is_empty(), "执行后输入框应清空");
        assert!(app.slash_popup.is_none());
        assert!(app.slash_dismissed_query.is_none());
    }

    #[tokio::test]
    async fn popup_esc_closes_keeps_text() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        for c in "he".chars() {
            input.insert_char(c);
        }
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        handle_key(
            key(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut input,
            &mut conn,
        )
        .await
        .unwrap();
        assert!(app.slash_popup.is_none());
        assert_eq!(app.slash_dismissed_query.as_deref(), Some("he"));
        assert_eq!(input.lines().join("\n"), "/he", "Esc 不应改动输入文本");
    }

    /// 设计文档 §6：弹窗打开时双击 Esc 的中断/退出语义与既有行为一致——
    /// 第一次关弹窗并计数，500ms 内第二次触发中断（回合活跃 → Abort）。
    #[tokio::test]
    async fn popup_open_double_esc_aborts_active_turn() {
        use parrot_protocol::agent_event::AgentEvent;
        use ratatui::backend::TestBackend;

        let (mut conn, mut server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        app.apply_event(AgentEvent::TurnStart {
            session_id: app.session_id,
            turn_id: uuid::Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert!(app.is_turn_active());
        let mut input = TextArea::default();
        for c in "/".chars() {
            input.insert_char(c);
        }
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        assert!(app.slash_popup.is_some());

        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let (ui_tx, mut ui_rx) = mpsc::channel(16);
        ui_tx
            .send(UiEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        ui_tx
            .send(UiEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        ui_tx.send(UiEvent::Quit).await.unwrap();
        drop(ui_tx);

        let mut dirty = true;
        run_loop(
            &mut terminal,
            &mut conn,
            &mut app,
            &mut input,
            &mut ui_rx,
            &mut dirty,
        )
        .await
        .unwrap();

        assert!(app.slash_popup.is_none(), "第一次 Esc 应关闭弹窗");
        assert_eq!(input.lines().join("\n"), "/", "Esc 不应改动输入文本");
        match server_rx.try_recv().unwrap() {
            ClientMessage::Abort { session_id } => assert_eq!(session_id, app.session_id),
            other => panic!("expected Abort, got {other:?}"),
        }
        assert!(server_rx.try_recv().is_err(), "不应有第二条消息");
    }
}
