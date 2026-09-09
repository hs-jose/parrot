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
}
