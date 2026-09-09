# Slash 命令补全弹窗 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 Parrot TUI 增加斜杠命令补全弹窗：输入 `/` 弹出、随输入过滤、↑/↓ 选择、Enter 立即执行；命令经可扩展注册表管理。

**Architecture:** 新模块 `src/tui/slash.rs` 承载命令注册表（纯数据 `REGISTRY` + `SlashAction` 枚举分发器）、过滤/查找函数、`SlashPopup` 弹窗状态机和 `sync_slash_popup` 同步函数。`App` 增加两个状态字段；`handle_key` 在弹窗存活时优先拦截按键；`ui.rs` 在输入框上方悬浮渲染弹窗。设计文档：`docs/superpowers/specs/2026-09-10-slash-command-popup-design.md`。

**Tech Stack:** Rust 2021 / ratatui 0.30（启用 `unstable-rendered-line-info`）+ crossterm 0.29 + ratatui-textarea 0.9 + unicode-width 0.2。

## Global Constraints

- TUI 代码编译在根包 `parrot` 的 **bin target** 下（`src/cli/main.rs` 通过 `#[path = "../tui/mod.rs"] mod tui;` 引入），单元测试命令为 `cargo test --bin parrot`。
- 注释风格：中文、简短、解释"为什么"（与 `src/tui/` 现有风格一致）。
- 错误处理：TUI 层沿用现有 `Result<_, Box<dyn std::error::Error>>`。
- 不改动 daemon、parrot-protocol、`!` shell 命令路径、普通消息发送路径。
- 不做 Tab 补全、不做参数补全、不做模糊匹配（大小写不敏感前缀即可）。
- 每个任务结束必须通过：`cargo test --bin parrot` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`（fmt 若报错先 `cargo fmt --all` 修正）。
- commit 风格沿用仓库惯例：`feat(tui): <中文描述>`。

---

### Task 1: 注册表 + 查找/过滤/执行（`src/tui/slash.rs`）

**Files:**
- Create: `src/tui/slash.rs`
- Modify: `src/tui/mod.rs`（模块声明区，第 1-4 行附近）

**Interfaces:**
- Consumes: `crate::tui::app::{App, ChatEntry}`（已存在）；`crate::conn::Connection`（已存在）；`parrot_protocol::ClientMessage`（已存在）
- Produces（后续任务依赖，签名逐字照抄）:
  - `pub(crate) enum SlashAction { Help, Usage, Abort, Exit }`
  - `pub(crate) struct SlashCommand { pub name: &'static str, pub description: &'static str, pub action: SlashAction }`
  - `pub(crate) const REGISTRY: &[SlashCommand]`
  - `pub(crate) fn find(name: &str) -> Option<&'static SlashCommand>`
  - `pub(crate) fn filter(query: &str) -> Vec<&'static SlashCommand>`
  - `pub(crate) async fn execute(cmd: &SlashCommand, app: &mut App, conn: &mut Connection) -> Result<Option<bool>, Box<dyn std::error::Error>>`

- [ ] **Step 1: 写失败测试**

创建 `src/tui/slash.rs`，先只写测试模块和最小占位（保证编译失败可见）：

```rust
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
        assert_eq!(execute(cmd, &mut app, &mut conn).await.unwrap(), Some(false));
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
        assert_eq!(execute(cmd, &mut app, &mut conn).await.unwrap(), Some(false));
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
        assert_eq!(execute(cmd, &mut app, &mut conn).await.unwrap(), Some(false));
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
        assert_eq!(execute(cmd, &mut app, &mut conn).await.unwrap(), Some(false));
        match server_rx.try_recv().unwrap() {
            ClientMessage::Abort { .. } => {}
            other => panic!("expected Abort, got {other:?}"),
        }
    }
}
```

同时在 `src/tui/mod.rs` 的模块声明区（`pub(crate) mod input;` 之后）加入：

```rust
pub(crate) mod slash;
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot slash::`
Expected: 编译错误（`SlashAction`/`REGISTRY`/`find`/`filter`/`execute` 未定义）

- [ ] **Step 3: 最小实现**

在 `src/tui/slash.rs` 顶部（tests 模块之前）加入：

```rust
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin parrot slash::`
Expected: 全部 PASS（10 个测试）

- [ ] **Step 5: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add src/tui/slash.rs src/tui/mod.rs
git commit -m "feat(tui): 斜杠命令注册表与执行器"
```

---

### Task 2: `popup_query` + `SlashPopup` 弹窗状态机

**Files:**
- Modify: `src/tui/slash.rs`

**Interfaces:**
- Consumes: Task 1 的 `filter`、`REGISTRY`
- Produces（后续任务依赖）:
  - `pub(crate) fn popup_query(text: &str) -> Option<&str>`
  - `pub(crate) struct SlashPopup`，方法：
    - `pub(crate) fn new() -> Self`
    - `pub(crate) fn filtered(query: &str) -> Self`
    - `pub(crate) fn filter(&mut self, query: &str)`
    - `pub(crate) fn move_up(&mut self)` / `pub(crate) fn move_down(&mut self)`（循环）
    - `pub(crate) fn selected_cmd(&self) -> Option<&'static SlashCommand>`
    - `pub(crate) fn is_empty(&self) -> bool`
    - `pub(crate) fn items(&self) -> &[&'static SlashCommand]`
    - `pub(crate) fn selected(&self) -> usize`

- [ ] **Step 1: 写失败测试**

在 `src/tui/slash.rs` 的 tests 模块追加：

```rust
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot slash::tests::popup`
Expected: 编译错误（`popup_query`/`SlashPopup` 未定义）

- [ ] **Step 3: 最小实现**

在 `src/tui/slash.rs` 的 `execute` 之后、tests 模块之前加入：

```rust
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
            items: REGISTRY.to_vec(),
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin parrot slash::`
Expected: 全部 PASS

- [ ] **Step 5: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add src/tui/slash.rs
git commit -m "feat(tui): 斜杠补全弹窗状态机与触发规则"
```

---

### Task 3: `App` 字段 + `sync_slash_popup` 同步函数

**Files:**
- Modify: `src/tui/app.rs`（`App` 结构体与 `App::new`）
- Modify: `src/tui/slash.rs`（新增 `sync_slash_popup` + 测试）

**Interfaces:**
- Consumes: Task 2 的 `SlashPopup`、`popup_query`
- Produces:
  - `App` 新增 pub 字段：`pub slash_popup: Option<SlashPopup>`、`pub slash_dismissed_query: Option<String>`
  - `pub(crate) fn sync_slash_popup(app: &mut App, text: &str)`

- [ ] **Step 1: 写失败测试**

在 `src/tui/slash.rs` 的 tests 模块追加：

```rust
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
        assert!(app.slash_dismissed_query.is_none(), "失效 query 应清 dismissed");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot slash::tests::sync`
Expected: 编译错误（`slash_popup`/`slash_dismissed_query` 字段与 `sync_slash_popup` 未定义）

- [ ] **Step 3: 最小实现**

`src/tui/app.rs`：顶部 use 区加入：

```rust
use crate::tui::slash::SlashPopup;
```

在 `App` 结构体的 `pub selected_tool: Option<String>,` 之后加入两个字段：

```rust
    /// 斜杠命令补全弹窗（存活 = Some）。由 sync_slash_popup 按输入文本校正。
    pub slash_popup: Option<SlashPopup>,
    /// Esc 关闭弹窗时的 query；同 query 不复活（防下一帧重新弹出），见设计 §2。
    pub slash_dismissed_query: Option<String>,
```

在 `App::new` 的 `selected_tool: None,` 之后加入：

```rust
            slash_popup: None,
            slash_dismissed_query: None,
```

`src/tui/slash.rs`：在 `SlashPopup` impl 之后加入：

```rust
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin parrot`
Expected: 全部 PASS（含 app.rs 既有测试）

- [ ] **Step 5: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add src/tui/app.rs src/tui/slash.rs
git commit -m "feat(tui): 弹窗状态入 App 并接 sync_slash_popup"
```

---

### Task 4: `handle_command` 的 `/` 分支接注册表

**Files:**
- Modify: `src/tui/mod.rs`（`handle_command` 函数，约 342-408 行）

**Interfaces:**
- Consumes: Task 1 的 `slash::find` / `slash::execute`
- Produces: `handle_command` 行为变化：`/` 命令改由注册表分发（对外签名不变）

- [ ] **Step 1: 写失败测试（未知命令场景补测）**

在 `src/tui/mod.rs` 的 tests 模块追加：

```rust
    #[tokio::test]
    async fn handle_command_unknown_pushes_info() {
        let (mut conn, _server_rx) = test_conn();
        let mut app = app::App::new(SessionId::new_v4());
        let quit = handle_command(&mut app, &mut conn, "/nope").await.unwrap();
        assert_eq!(quit, Some(false));
        assert!(matches!(app.entries.last(), Some(app::ChatEntry::Info(_))));
    }
```

- [ ] **Step 2: 运行测试确认现状**

Run: `cargo test --bin parrot handle_command`
Expected: 既有 4 个测试 PASS，新增的 `handle_command_unknown_pushes_info` 也 PASS（现状已满足）——此任务的价值在实现简化与行为统一，测试先行锁行为。

- [ ] **Step 3: 改造 `handle_command` 的 `/` 分支**

将 `handle_command` 中 `if let Some(rest) = trimmed.strip_prefix('/')` 的整个 match 块替换为注册表查找：

```rust
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let cmd = rest.trim();
        return match slash::find(cmd) {
            Some(c) => slash::execute(c, app, conn).await,
            None => {
                app.entries.push(app::ChatEntry::Info(format!(
                    "未知命令 /{cmd}，输入 /help 查看可用命令"
                )));
                Ok(Some(false))
            }
        };
    } else if let Some(cmd) = trimmed.strip_prefix('!') {
```

（`!` 分支与末尾 `else { Ok(None) }` 保持原样不动。）注意：`slash` 模块在 `mod.rs` 内直接以 `slash::...` 引用即可（同文件已 `pub(crate) mod slash;`）。

- [ ] **Step 4: 运行全部相关测试**

Run: `cargo test --bin parrot`
Expected: 全部 PASS（含 `handle_command_help_pushes_info_without_sending` 与 `handle_command_exit_requests_quit`——行为不变）

- [ ] **Step 5: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add src/tui/mod.rs
git commit -m "refactor(tui): handle_command 的 / 分支改查斜杠命令注册表"
```

---

### Task 5: `handle_key` 弹窗按键拦截 + `run_loop` 接 `sync_slash_popup`

**Files:**
- Modify: `src/tui/mod.rs`（`handle_key` 的 `Mode::Normal` 分支；`run_loop` 的 `UiEvent::Key` / `UiEvent::Paste` 分支）

**Interfaces:**
- Consumes: Task 2/3 的 `SlashPopup` 方法、`slash::popup_query`、`slash::execute`、`slash::sync_slash_popup`
- Produces: 弹窗存活时 `handle_key` 拦截 Up/Down/Enter/Esc；`run_loop` 每次按键与粘贴后调用 sync

- [ ] **Step 1: 写失败测试**

在 `src/tui/mod.rs` 的 tests 模块追加（`use` 需补：`use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};`——若 tests 模块尚未引入则加；`use crate::tui::slash::sync_slash_popup;`）：

```rust
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
        handle_key(key(KeyCode::Down, KeyModifiers::NONE), &mut app, &mut input, &mut conn)
            .await
            .unwrap();
        assert_eq!(app.slash_popup.as_ref().unwrap().selected(), 1);
        assert_eq!(app.scroll_offset, 0, "弹窗存活时 Down 不应滚动聊天区");
        handle_key(key(KeyCode::Up, KeyModifiers::NONE), &mut app, &mut input, &mut conn)
            .await
            .unwrap();
        assert_eq!(app.slash_popup.as_ref().unwrap().selected(), 0);
    }

    #[tokio::test]
    async fn popup_shift_up_still_scrolls_chat() {
        let (mut app, mut conn, mut input, _rx) = popup_app().await;
        handle_key(key(KeyCode::Up, KeyModifiers::SHIFT), &mut app, &mut input, &mut conn)
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
        let quit = handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut app, &mut input, &mut conn)
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
        handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut app, &mut input, &mut conn)
            .await
            .unwrap();
        assert!(app.slash_popup.is_none());
        assert_eq!(app.slash_dismissed_query.as_deref(), Some("he"));
        assert_eq!(input.lines().join("\n"), "/he", "Esc 不应改动输入文本");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot popup_`
Expected: 编译失败或断言失败（`handle_key` 尚未拦截弹窗按键：Down 会落入 `_` 分支进 TextArea，选中不变）

- [ ] **Step 3: 改造 `handle_key` 与 `run_loop`**

（a）`handle_key` 的 `Mode::Normal` 分支：把 `Mode::Normal => match k.code { ... }` 改为先拦截弹窗按键、再走既有 match。完整替换为：

```rust
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
                        if let Some(cmd) =
                            app.slash_popup.as_ref().and_then(|p| p.selected_cmd())
                        {
                            *input = TextArea::default();
                            app.slash_popup = None;
                            app.slash_dismissed_query = None;
                            return Ok(slash::execute(cmd, app, conn).await?);
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
```

（b）`run_loop` 的 `UiEvent::Key` 分支：在 `if let Some(should_quit) = handle_key(...)` 之后、`*dirty = true;` 之前加入：

```rust
                        slash::sync_slash_popup(app, &input.lines().join("\n"));
```

（c）`run_loop` 的 `UiEvent::Paste` 分支：在逐字符插入循环之后、`*dirty = true;` 之前加入同样的：

```rust
                        slash::sync_slash_popup(app, &input.lines().join("\n"));
```

（d）`handle_key` 的 `Mode::ConfirmPending` 分支不动（确认模态优先，弹窗按键不会到达 Normal 分支）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin parrot`
Expected: 全部 PASS

- [ ] **Step 5: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add src/tui/mod.rs
git commit -m "feat(tui): 弹窗存活时拦截按键并接 sync_slash_popup"
```

---

### Task 6: 输入框上方悬浮渲染 `draw_slash_popup`

**Files:**
- Modify: `Cargo.toml`（根包 `[dependencies]`）
- Modify: `src/tui/ui.rs`

**Interfaces:**
- Consumes: Task 3 的 `app.slash_popup`（`SlashPopup::items()`/`selected()`/`is_empty()`）
- Produces: `ui::draw` 在 `Mode::Normal` 且弹窗存活非空时于输入卡片上方绘制弹窗

- [ ] **Step 1: 加依赖**

`Cargo.toml` 的 `[dependencies]` 末尾加入（unicode-width 已存在于 Cargo.lock，不会引入新包）：

```toml
unicode-width       = "0.2"
```

- [ ] **Step 2: 写失败测试**

在 `src/tui/ui.rs` 的 tests 模块追加（现有 tests 模块已有 `TestBackend`/`Terminal`/`SessionId` 等 use，无需重复）：

```rust
    use crate::tui::slash::{SlashPopup, sync_slash_popup};
    use crate::tui::app::PendingConfirmation;

    fn backend_text(term: &Terminal<TestBackend>) -> String {
        term.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    fn row_text(term: &Terminal<TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        let w = buf.area().width as usize;
        let start = y as usize * w;
        buf.content()[start..start + w]
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn slash_popup_renders_when_active() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        app.slash_popup = Some(SlashPopup::filtered(""));
        let mut input = ratatui_textarea::TextArea::default();
        input.insert_char('/');
        terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        let text = backend_text(&terminal);
        assert!(text.contains("/help"), "应渲染命令名：{text}");
        assert!(text.contains("❯"), "应渲染选中标记：{text}");
        assert!(text.contains("查看本会话"), "应渲染描述：{text}");
    }

    #[test]
    fn slash_popup_hidden_when_confirm_pending() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        app.slash_popup = Some(SlashPopup::filtered(""));
        app.mode = Mode::ConfirmPending;
        app.pending_confirmation = Some(PendingConfirmation {
            tool_call_id: "t1".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({}),
        });
        let input = ratatui_textarea::TextArea::default();
        terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        assert!(!backend_text(&terminal).contains("查看本会话"), "ConfirmPending 时弹窗不应绘制");
    }

    #[test]
    fn slash_popup_hidden_when_input_not_slash() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        let mut input = ratatui_textarea::TextArea::default();
        for c in "hi".chars() {
            input.insert_char(c);
        }
        sync_slash_popup(&mut app, &input.lines().join("\n"));
        terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        assert!(app.slash_popup.is_none());
        assert!(!backend_text(&terminal).contains("查看本会话"));
    }

    #[test]
    fn slash_popup_sits_above_input_box() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        app.slash_popup = Some(SlashPopup::filtered(""));
        let mut input = ratatui_textarea::TextArea::default();
        input.insert_char('/');
        terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        // 60x20：标题 1 + 对话区 + 输入框 + 状态栏；输入框顶行之下不应出现弹窗内容。
        let popup_row = (0u16..20).find(|&y| row_text(&terminal, y).contains("/help")).expect("弹窗行存在");
        let input_top = (0u16..20)
            .find(|&y| row_text(&terminal, y).contains('╭') && y > 8)
            .expect("输入框顶边框存在");
        assert!(popup_row < input_top, "弹窗行 {popup_row} 应在输入框顶 {input_top} 之上");
    }
```

**注意**：`slash_popup_sits_above_input_box` 中 `y > 8` 是跳过对话卡片顶边框的启发式；若测试在小尺寸渲染下不稳定，允许实现者改为断言"弹窗行 y < 14"（60x20 下输入框顶边 y=16，弹窗底部不越过它即可），但"弹窗在输入框上方"的语义必须保留。

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test --bin parrot slash_popup`
Expected: `slash_popup_renders_when_active` FAIL（弹窗未绘制）

- [ ] **Step 4: 实现**

`src/tui/ui.rs`：

（a）顶部 use 区加入：

```rust
use unicode_width::UnicodeWidthStr;

use crate::tui::slash::SlashPopup;
```

（b）`draw` 中，`draw_status(f, chunks[3], app);` 之后、confirm modal 块之前加入：

```rust
    // 斜杠补全弹窗：浮在输入卡片上方（仅 Normal 模式；确认模态优先）。
    if app.mode == Mode::Normal {
        if let Some(popup) = app.slash_popup.as_ref() {
            if !popup.is_empty() {
                draw_slash_popup(f, area, chunks[2], popup);
            }
        }
    }
```

（c）在 `draw_input` 之后新增函数：

```rust
const POPUP_MAX_VISIBLE: usize = 8;
const POPUP_MIN_WIDTH: u16 = 40;

/// 斜杠命令补全弹窗：贴输入卡片上方、左对齐输入卡片，最多显示
/// `POPUP_MAX_VISIBLE` 项，超出时滚动窗口保证选中项可见。
fn draw_slash_popup(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    input_area: Rect,
    popup: &SlashPopup,
) {
    let items = popup.items();
    let total = items.len();
    let visible = total.min(POPUP_MAX_VISIBLE);
    let sel = popup.selected();
    // 滚动窗口起点：sel 减一屏可保证选中项在窗口内，再夹到合法范围。
    let start = sel
        .checked_sub(visible - 1)
        .unwrap_or(0)
        .min(total - visible);

    // 宽度：最长行内容宽 + 前缀与 padding，且不低于最小宽度。
    let content_w = items
        .iter()
        .map(|c| format!("/{}  {}", c.name, c.description).width() as u16)
        .max()
        .unwrap_or(0)
        + 4; // ❯+空格 前缀 2 列 + 左右 padding 2 列
    let total_w = content_w
        .max(POPUP_MIN_WIDTH)
        .min(area.width.saturating_sub(2))
        .max(3);
    let height = (visible as u16 + 2).min(input_area.y.max(3)); // 含上下边框

    let popup_area = Rect::new(input_area.x, input_area.y - height, total_w, height);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::DIM))
        .title(Span::styled(
            " 斜杠命令 ",
            Style::default().fg(palette::DIM),
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, c) in items[start..start + visible].iter().enumerate() {
        let idx = start + i;
        if idx == sel {
            let row_w = 2 + 1 + c.name.len() + 2 + c.description.width();
            let pad = (inner.width as usize).saturating_sub(row_w);
            let hi = Style::default().bg(palette::TITLE_BG).fg(palette::TITLE_FG);
            lines.push(Line::from(vec![
                Span::styled("❯ ".to_string(), hi),
                Span::styled(format!("/{}", c.name), hi.add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}", c.description), hi),
                Span::styled(" ".repeat(pad), hi),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("/{}", c.name), Style::default().fg(palette::USER_FG)),
                Span::styled(
                    format!("  {}", c.description),
                    Style::default().fg(palette::DIM),
                ),
            ]));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --bin parrot`
Expected: 全部 PASS

- [ ] **Step 6: lint 并提交**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add Cargo.toml Cargo.lock src/tui/ui.rs
git commit -m "feat(tui): 斜杠命令补全弹窗渲染"
```

---

### Task 7: 全量验证

**Files:** 无新改动（验证任务）

- [ ] **Step 1: 全 workspace 测试**

Run: `cargo test --workspace`
Expected: 全部 PASS（含 e2e/phase15/cassette 集成测试）

- [ ] **Step 2: clippy + fmt**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 无输出（0 warning / 格式已整）

- [ ] **Step 3: 手动冒烟（可选，需要 ANTHROPIC_API_KEY）**

```bash
export ANTHROPIC_API_KEY=sk-...
cargo run --bin parrotd &
cargo run --bin parrot
```

Expected: 输入 `/` 弹窗显示 4 条命令；输入 `/us` 过滤出 `/usage`；↑/↓ 移动选中；Enter 执行；Esc 关闭弹窗且输入保留；删除 `/` 弹窗消失；Shift+↑/↓ 仍滚动聊天区。

- [ ] **Step 4: 完成报告**

无代码改动则无需提交；如有 fmt 产生的修正：

```bash
git add -A
git commit -m "style: cargo fmt"
```
