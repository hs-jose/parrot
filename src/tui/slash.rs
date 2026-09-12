use crate::conn::Connection;
use crate::tui::app::{App, ChatEntry};
use parrot_protocol::ClientMessage;

/// 斜杠命令的动作。分发在 [`execute`]，穷尽匹配保证新增变体不漏实现。
pub(crate) enum SlashAction {
    Help,
    Usage,
    Abort,
    Mcp,
    Model,
    Exit,
}

/// 斜杠命令定义：注册表条目，同时是补全弹窗的数据源。
pub(crate) struct SlashCommand {
    /// 命令名（不含 `/` 前缀），如 "help"。
    pub name: &'static str,
    /// 弹窗里显示的一行说明。
    pub description: &'static str,
    pub action: SlashAction,
}

/// 斜杠命令注册表。新增命令 = 加 `SlashAction` 变体 + 这里一条 + `execute` 一个分支。
pub(crate) const REGISTRY: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        description: "显示可用命令",
        action: SlashAction::Help,
    },
    SlashCommand {
        name: "usage",
        description: "查看本会话 token 用量",
        action: SlashAction::Usage,
    },
    SlashCommand {
        name: "abort",
        description: "中断当前进行中的轮次",
        action: SlashAction::Abort,
    },
    SlashCommand {
        name: "mcp",
        description: "查看 MCP server 状态",
        action: SlashAction::Mcp,
    },
    SlashCommand {
        name: "model",
        description: "切换会话模型",
        action: SlashAction::Model,
    },
    SlashCommand {
        name: "exit",
        description: "退出 Parrot",
        action: SlashAction::Exit,
    },
];

/// 按名字精确查找（不含 `/`）。
pub(crate) fn find(name: &str) -> Option<&'static SlashCommand> {
    REGISTRY.iter().find(|c| c.name == name)
}

/// 大小写不敏感前缀匹配；空 query 返回全量。
pub(crate) fn filter(query: &str) -> Vec<&'static SlashCommand> {
    let q = query.to_lowercase();
    REGISTRY
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&q))
        .collect()
}

/// 拆分命令行文本为命令名与参数：首个空白起算，参数取去空白后的剩余部分
/// （空串视为无参数）。如 `model gpt-5` → `("model", Some("gpt-5"))`。
pub(crate) fn parse_invocation(text: &str) -> (&str, Option<&str>) {
    let text = text.trim();
    match text.split_once(char::is_whitespace) {
        Some((name, arg)) => {
            let arg = arg.trim();
            (name, (!arg.is_empty()).then_some(arg))
        }
        None => (text, None),
    }
}

/// 执行命令。`arg` 为命令名之后的参数（无参数为 `None`）。返回
/// `Some(true)` 表示调用方应退出 TUI。
pub(crate) async fn execute(
    cmd: &SlashCommand,
    arg: Option<&str>,
    app: &mut App,
    conn: &mut Connection,
) -> Result<Option<bool>, Box<dyn std::error::Error>> {
    match cmd.action {
        SlashAction::Help => {
            // 斜杠命令列表由输入 "/" 时的补全弹窗展示，help 不再重复枚举；
            // 这里只补弹窗覆盖不到的信息：`!` shell 命令与快捷键。
            // 每行一个绑定，按 \n 拆行渲染（见 ui.rs Info 渲染）。
            app.entries.push(ChatEntry::Info(
                "输入 / 弹出命令补全；!<命令> 在 daemon 执行 shell 命令\n\
                 \n\
                 快捷键:\n\
                 · Shift+Enter / Ctrl+J  换行\n\
                 · Tab / Shift+Tab  在工具条目间选中\n\
                 · Enter  展开/收起选中的工具条目\n\
                 · PgUp / PgDn  半页滚动\n\
                 · Shift+↑ / Shift+↓  逐行滚动\n\
                 · Ctrl+Home / Ctrl+End  滚动到顶部/底部\n\
                 · Esc  关闭命令弹窗 / 清除选中\n\
                 · 双击 Esc  中断进行中轮次（空闲时退出）"
                    .into(),
            ));
            Ok(Some(false))
        }
        SlashAction::Usage => {
            let u = &app.total_usage;
            app.entries.push(ChatEntry::Info(format!(
                "Token 用量: {} in / {} out",
                u.input_tokens, u.output_tokens
            )));
            Ok(Some(false))
        }
        SlashAction::Abort => {
            if app.is_turn_active() {
                conn.sender
                    .send(ClientMessage::Abort {
                        session_id: app.session_id,
                    })
                    .await?;
            } else {
                app.entries
                    .push(ChatEntry::Info("当前没有进行中的轮次".into()));
            }
            Ok(Some(false))
        }
        SlashAction::Mcp => {
            conn.sender.send(ClientMessage::ListMcpServers).await?;
            app.entries
                .push(ChatEntry::Info("已请求 MCP server 状态…".into()));
            Ok(Some(false))
        }
        SlashAction::Model => {
            match arg {
                Some(model) => {
                    if app.is_turn_active() {
                        app.entries
                            .push(ChatEntry::Info("当前轮进行中，请稍后再切换".into()));
                    } else {
                        conn.sender
                            .send(ClientMessage::Model {
                                session_id: app.session_id,
                                model: model.to_string(),
                            })
                            .await?;
                    }
                }
                None => {
                    app.entries
                        .push(ChatEntry::Info("用法: /model <name>".into()));
                }
            }
            Ok(Some(false))
        }
        SlashAction::Exit => Ok(Some(true)),
    }
}

/// 输入文本 → 弹窗过滤词：以 `/` 开头、不含空白（换行/空格都算）时返回
/// `/` 之后的子串，否则 `None`（弹窗应关闭）。权威规则见设计 §2。
pub(crate) fn popup_query(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// 斜杠补全弹窗状态：过滤后的命令列表 + 选中索引。
pub(crate) struct SlashPopup {
    items: Vec<&'static SlashCommand>,
    selected: usize,
    query: String,
}

impl SlashPopup {
    pub(crate) fn new() -> Self {
        Self {
            items: REGISTRY.iter().collect(),
            selected: 0,
            query: String::new(),
        }
    }

    /// 按 query 过滤后直接构造（sync 复活弹窗用）。
    pub(crate) fn filtered(query: &str) -> Self {
        let mut p = Self::new();
        p.filter(query);
        p
    }

    /// 重新过滤；query 未变时早退保持选中位——run_loop 在每个按键后都会
    /// 用当前文本调 sync → filter，若不早退，Up/Down 移动的选中会被立即
    /// 重置回 0（用户报告的"上下键无效"根因）。query 变化时选中归 0
    /// （列表内容变化后旧选中位不再有意义）。
    pub(crate) fn filter(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.items = filter(query);
        self.selected = 0;
    }

    pub(crate) fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    pub(crate) fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub(crate) fn selected_cmd(&self) -> Option<&'static SlashCommand> {
        self.items.get(self.selected).copied()
    }

    /// items 为空时 UI 不画弹窗，但状态仍存活（退格可恢复匹配）。
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn items(&self) -> &[&'static SlashCommand] {
        &self.items
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }
}

/// 每次按键/粘贴后按当前输入文本校正弹窗状态（权威规则见设计 §2）：
/// query 失效即关闭；弹窗开着就随 query 重新过滤；被 Esc 关闭后同 query
/// 不复活，query 变化才重新弹出。
pub(crate) fn sync_slash_popup(app: &mut App, text: &str) {
    match popup_query(text) {
        None => {
            app.slash_popup = None;
            app.slash_dismissed_query = None;
        }
        Some(q) => {
            if let Some(p) = app.slash_popup.as_mut() {
                p.filter(q);
            } else if app.slash_dismissed_query.as_deref() != Some(q) {
                app.slash_popup = Some(SlashPopup::filtered(q));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Connection;
    use crate::tui::app::{App, ChatEntry};
    use parrot_protocol::agent_event::AgentEvent;
    use parrot_protocol::types::Usage;
    use parrot_protocol::{ClientMessage, SessionId};
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

    #[test]
    fn filter_prefix_case_insensitive() {
        let names: Vec<_> = filter("US").iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["usage"]);
    }

    #[test]
    fn filter_empty_query_returns_all() {
        assert_eq!(filter("").len(), REGISTRY.len());
    }

    #[test]
    fn filter_no_match_is_empty() {
        assert!(filter("zzz").is_empty());
    }

    #[test]
    fn find_exact_match() {
        assert_eq!(find("help").map(|c| c.name), Some("help"));
        assert!(find("nope").is_none());
    }

    #[test]
    fn registry_names_unique_non_empty_no_slash() {
        let mut seen = std::collections::HashSet::new();
        for c in REGISTRY {
            assert!(!c.name.is_empty());
            assert!(!c.name.starts_with('/'));
            assert!(!c.description.is_empty());
            assert!(seen.insert(c.name), "duplicate name: {}", c.name);
        }
    }

    #[tokio::test]
    async fn execute_exit_requests_quit() {
        let (mut conn, _rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("exit").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn execute_help_pushes_info_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("help").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        match app.entries.last() {
            Some(ChatEntry::Info(s)) => {
                // help 不再枚举斜杠命令（弹窗负责展示），只保留 `!` 与快捷键说明。
                assert!(s.contains("!<命令>"), "应说明 ! shell 命令：{s}");
                assert!(s.contains("快捷键"), "应列出快捷键：{s}");
                assert!(!s.contains("/usage"), "不应枚举斜杠命令：{s}");
            }
            other => panic!("expected Info, got {other:?}"),
        }
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn execute_usage_reports_totals() {
        let (mut conn, _rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        app.total_usage = Usage {
            input_tokens: 3,
            output_tokens: 5,
        };
        let cmd = find("usage").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        match app.entries.last() {
            Some(ChatEntry::Info(s)) => assert!(s.contains("3 in / 5 out"), "{s}"),
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_abort_inactive_turn_pushes_info() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("abort").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        assert!(matches!(app.entries.last(), Some(ChatEntry::Info(_))));
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn execute_abort_active_sends_abort() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        app.apply_event(AgentEvent::TurnStart {
            session_id: app.session_id,
            turn_id: uuid::Uuid::new_v4(),
            user_message: "hi".into(),
        });
        let cmd = find("abort").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        match server_rx.try_recv().unwrap() {
            ClientMessage::Abort { .. } => {}
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_model_without_arg_pushes_usage_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("model").unwrap();
        assert_eq!(
            execute(cmd, None, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        match app.entries.last() {
            Some(ChatEntry::Info(s)) => assert!(s.contains("/model <name>"), "{s}"),
            other => panic!("expected Info, got {other:?}"),
        }
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn execute_model_with_arg_sends_model_message() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("model").unwrap();
        assert_eq!(
            execute(cmd, Some("gpt-5"), &mut app, &mut conn)
                .await
                .unwrap(),
            Some(false)
        );
        match server_rx.try_recv().unwrap() {
            ClientMessage::Model { session_id, model } => {
                assert_eq!(session_id, app.session_id);
                assert_eq!(model, "gpt-5");
            }
            other => panic!("expected Model, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_model_mid_turn_pushes_info_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        app.apply_event(AgentEvent::TurnStart {
            session_id: app.session_id,
            turn_id: uuid::Uuid::new_v4(),
            user_message: "hi".into(),
        });
        let cmd = find("model").unwrap();
        assert_eq!(
            execute(cmd, Some("gpt-5"), &mut app, &mut conn)
                .await
                .unwrap(),
            Some(false)
        );
        match app.entries.last() {
            Some(ChatEntry::Info(s)) => {
                assert!(s.contains("当前轮进行中，请稍后再切换"), "{s}");
            }
            other => panic!("expected Info, got {other:?}"),
        }
        assert!(server_rx.try_recv().is_err(), "轮中不得发送 Model");
    }

    #[test]
    fn parse_invocation_splits_name_and_arg() {
        assert_eq!(parse_invocation("model"), ("model", None));
        assert_eq!(parse_invocation("model gpt-5"), ("model", Some("gpt-5")));
        assert_eq!(
            parse_invocation("model   gpt-5  "),
            ("model", Some("gpt-5"))
        );
        assert_eq!(parse_invocation("model "), ("model", None));
        assert_eq!(parse_invocation(""), ("", None));
    }

    #[test]
    fn popup_query_basic() {
        assert_eq!(popup_query("/us"), Some("us"));
        assert_eq!(popup_query("/"), Some(""));
    }

    #[test]
    fn popup_query_rejects_whitespace_multiline_and_non_slash() {
        assert_eq!(popup_query("/he llo"), None);
        assert_eq!(popup_query("/ab\ncd"), None);
        assert_eq!(popup_query(" /us"), None);
        assert_eq!(popup_query("hello"), None);
        assert_eq!(popup_query(""), None);
    }

    #[test]
    fn popup_moves_wrap_around() {
        let mut p = SlashPopup::new();
        p.move_down();
        assert_eq!(p.selected(), 1);
        p.move_up();
        assert_eq!(p.selected(), 0);
        p.move_up();
        assert_eq!(p.selected(), REGISTRY.len() - 1, "向上应环绕到末尾");
        p.move_down();
        assert_eq!(p.selected(), 0, "从末尾向下应环绕回开头");
    }

    #[test]
    fn popup_empty_items_move_is_noop() {
        let mut p = SlashPopup::filtered("zzz");
        assert!(p.is_empty());
        p.move_up();
        p.move_down();
        assert!(p.selected_cmd().is_none());
    }

    #[test]
    fn popup_filter_resets_selection() {
        let mut p = SlashPopup::new();
        p.move_down();
        p.filter("us");
        assert_eq!(p.selected(), 0);
        assert_eq!(p.items().len(), 1);
        assert_eq!(p.selected_cmd().map(|c| c.name), Some("usage"));
    }

    #[test]
    fn filter_same_query_preserves_selection() {
        let mut p = SlashPopup::filtered(""); // 空 query 全量条目
        p.move_down();
        p.filter(""); // run_loop 每键后 sync 会以同 query 重入 filter
        assert_eq!(p.selected(), 1, "同 query 重入 filter 不得重置选中位");
    }

    #[test]
    fn popup_filtered_constructs_filtered() {
        let p = SlashPopup::filtered("ex");
        assert_eq!(p.items().len(), 1);
        assert_eq!(p.selected_cmd().map(|c| c.name), Some("exit"));
    }

    #[test]
    fn sync_opens_filters_and_closes() {
        let mut app = App::new(SessionId::new_v4());
        sync_slash_popup(&mut app, "/");
        assert!(app.slash_popup.is_some(), "空 query 应显示全量");
        sync_slash_popup(&mut app, "/us");
        let p = app.slash_popup.as_ref().unwrap();
        assert_eq!(p.items().len(), 1, "query 变化应重新过滤");
        sync_slash_popup(&mut app, "hello");
        assert!(app.slash_popup.is_none(), "非 / 开头应关闭");
        assert!(app.slash_dismissed_query.is_none());
    }

    #[test]
    fn sync_dismissed_same_query_stays_closed() {
        let mut app = App::new(SessionId::new_v4());
        sync_slash_popup(&mut app, "/he");
        // 模拟 Esc 关闭（Task 5 的 handle_key 会做同样的事）：
        app.slash_dismissed_query = Some("he".into());
        app.slash_popup = None;
        sync_slash_popup(&mut app, "/he");
        assert!(app.slash_popup.is_none(), "同 query 不复活");
        sync_slash_popup(&mut app, "/hel");
        assert!(app.slash_popup.is_some(), "query 变化应复活");
    }

    #[test]
    fn sync_invalid_query_clears_dismissed() {
        let mut app = App::new(SessionId::new_v4());
        sync_slash_popup(&mut app, "/he");
        app.slash_dismissed_query = Some("he".into());
        app.slash_popup = None;
        sync_slash_popup(&mut app, "text");
        assert!(app.slash_popup.is_none());
        assert!(
            app.slash_dismissed_query.is_none(),
            "失效 query 应清 dismissed"
        );
    }
}
