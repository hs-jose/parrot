use crate::conn::Connection;
use crate::tui::app::{App, ChatEntry};
use parrot_protocol::ClientMessage;

/// 斜杠命令的动作。分发在 [`execute`]，穷尽匹配保证新增变体不漏实现。
pub(crate) enum SlashAction {
    Help,
    Usage,
    Abort,
    Exit,
}

/// 斜杠命令定义：注册表条目，同时是补全弹窗的数据源。
pub(crate) struct SlashCommand {
    /// 命令名（不含 `/` 前缀），如 "help"。
    pub name: &'static str,
    /// 弹窗里显示的一行说明。
    // 渲染层（Task 6）才消费，届时移除 allow。
    #[allow(dead_code)]
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

/// 执行命令。返回 `Some(true)` 表示调用方应退出 TUI。
pub(crate) async fn execute(
    cmd: &SlashCommand,
    app: &mut App,
    conn: &mut Connection,
) -> Result<Option<bool>, Box<dyn std::error::Error>> {
    match cmd.action {
        SlashAction::Help => {
            let names = REGISTRY
                .iter()
                .map(|c| format!("/{}", c.name))
                .collect::<Vec<_>>()
                .join(" ");
            app.entries.push(ChatEntry::Info(format!(
                "可用命令: {names}\n!<命令> 在 daemon 执行 shell 命令"
            )));
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
}

impl SlashPopup {
    pub(crate) fn new() -> Self {
        Self {
            items: REGISTRY.iter().collect(),
            selected: 0,
        }
    }

    /// 按 query 过滤后直接构造（sync 复活弹窗用）。
    pub(crate) fn filtered(query: &str) -> Self {
        let mut p = Self::new();
        p.filter(query);
        p
    }

    /// 重新过滤；选中归 0（列表内容变化后旧选中位不再有意义）。
    pub(crate) fn filter(&mut self, query: &str) {
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
    // 以下三个 getter 的消费方在 Task 6 渲染层，届时移除 allow。
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn items(&self) -> &[&'static SlashCommand] {
        &self.items
    }

    #[allow(dead_code)]
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
        assert_eq!(execute(cmd, &mut app, &mut conn).await.unwrap(), Some(true));
    }

    #[tokio::test]
    async fn execute_help_pushes_info_without_sending() {
        let (mut conn, mut server_rx) = test_conn();
        let mut app = App::new(SessionId::new_v4());
        let cmd = find("help").unwrap();
        assert_eq!(
            execute(cmd, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        assert!(matches!(app.entries.last(), Some(ChatEntry::Info(_))));
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
            execute(cmd, &mut app, &mut conn).await.unwrap(),
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
            execute(cmd, &mut app, &mut conn).await.unwrap(),
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
            execute(cmd, &mut app, &mut conn).await.unwrap(),
            Some(false)
        );
        match server_rx.try_recv().unwrap() {
            ClientMessage::Abort { .. } => {}
            other => panic!("expected Abort, got {other:?}"),
        }
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
