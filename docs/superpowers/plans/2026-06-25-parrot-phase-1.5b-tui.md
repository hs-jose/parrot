# Phase 1.5b — TUI 客户端 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在根 `parrot` 二进制中集成 ratatui + crossterm 实现的全屏 TUI 客户端，按 tty 分发默认启动 TUI；保留 CLI 流式/子命令作为非交互与脚本回退。

**Architecture:** 单一 `parrot` bin 在 `src/tui/` 下承载 TUI 模块；共享握手代码抽到 `src/cli/conn.rs`；TUI 异步主循环用 `tokio::select!`，crossterm 输入通过独立 `std::thread` 跑阻塞 `event::poll/read` 后经 `tokio::sync::mpsc` 喂入主循环（Windows 输入可靠性最佳路径）；工具二次确认用全屏 modal 浮层。状态机是纯函数（`App::apply_event`），可单元测试；渲染与输入线程无单测，靠手动验证。

**Tech Stack:** ratatui 0.28, crossterm 0.28, tui-textarea 0.7, 既有 `parrot-protocol` / `parrot-transport` / `parrot-config`。

## Global Constraints

- 沿用 AGENTS.md：`cargo build --workspace`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 四道闸门。
- TUI 只依赖 `parrot-protocol` + `parrot-transport` + `parrot-config`，**不依赖 `parrot-core`**（与 CLI 同边界）。
- 不拆 crate：TUI 是 `parrot` bin 内部模块，不开 feature gate。
- `parrot` bin 单一二进制；rm `atty` 依赖，改用 `std::io::IsTerminal`（Rust 1.70+，本机 toolchain 已满足）。
- 保留 `rustyline` 给 `--simple` 模式。删除 `read_line_stdin` async BufReader 路径。
- 不引入 ratatui/crossterm 的 `event-stream` feature——我们走同步阻塞线程路径。
- TUI 单元测试只覆盖状态机（`src/tui/app.rs` + `src/tui/confirm.rs`），不测渲染/输入线程；不新增 `tests/integration/` 入口。
- 错误跨 crate 用 `thiserror`；bin 内部允许 `anyhow` 或 `Box<dyn Error>`。本 bin 沿用现有 `Box<dyn std::error::Error>` 风格，不引 anyhow。

---

## File Structure

```
src/
├── cli/
│   ├── main.rs              # MODIFY: dispatch + 子命令；瘦身后只保留 dispatch
│   ├── conn.rs              # CREATE: Connection / read_token / connect / wait_hello / create_session / wait_session_resumed
│   └── stream.rs            # CREATE: 单消息流式输出 print_stream / prompt_confirm / truncate
└── tui/
    ├── mod.rs               # CREATE: run_tui 入口 + 事件循环 + 终端 setup/teardown
    ├── app.rs               # CREATE: App / ChatEntry / Mode 状态机（含单元测试）
    ├── confirm.rs           # CREATE: PendingConfirmation + 转字符串展示（可单元测试纯函数）
    ├── input.rs             # CREATE: UiEvent enum + spawn_input_thread
    └── ui.rs                 # CREATE: ratatui 布局（三栏 + 状态栏 + modal 浮层）
```

`src/tui/mod.rs` 通过 `pub mod app; pub mod confirm; pub mod input; pub mod ui;` 重新导出，外部仅见 `pub async fn run_tui(conn: Connection) -> Result<(), Box<dyn Error>>`。

---

### Task 1: 抽出 `src/cli/conn.rs` 共享握手模块

**Files:**
- Create: `src/cli/conn.rs`
- Modify: `src/cli/main.rs`（删除被抽出的函数，加入 `mod conn;`）

**Interfaces:**
- Produces:
  - `pub(crate) struct Connection { pub sender: tokio::sync::mpsc::Sender<ClientMessage>, pub receiver: tokio::sync::mpsc::Receiver<ServerMessage> }`
  - `pub(crate) fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>>`
  - `pub(crate) async fn connect(connect_url: &str, token_path: &str) -> Result<Connection, Box<dyn std::error::Error>>`
  - `pub(crate) async fn wait_hello(receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>) -> Result<String, Box<dyn std::error::Error>>`
  - `pub(crate) async fn create_session(sender: &tokio::sync::mpsc::Sender<ClientMessage>, receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>) -> Result<SessionId, Box<dyn std::error::Error>>`
  - `pub(crate) async fn wait_session_resumed(receiver: &mut tokio::sync::mpsc::Receiver<ServerMessage>) -> Result<SessionId, Box<dyn std::error::Error>>`

- [ ] **Step 1: 创建 `src/cli/conn.rs`**

```rust
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::WsTransportClient;
use tokio::sync::mpsc;

pub(crate) struct Connection {
    pub sender: mpsc::Sender<ClientMessage>,
    pub receiver: mpsc::Receiver<ServerMessage>,
}

pub(crate) fn read_token(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let token = std::fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        return Err(format!("Token file '{}' is empty", path).into());
    }
    Ok(token)
}

pub(crate) async fn connect(
    connect_url: &str,
    token_path: &str,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let token = read_token(token_path)?;
    let client = WsTransportClient::new();
    let conn = client.connect(connect_url, &token).await?;
    Ok(Connection {
        sender: conn.sender,
        receiver: conn.receiver,
    })
}

pub(crate) async fn wait_hello(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<String, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(ServerMessage::HelloAck { server_version }) => return Ok(server_version),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error during handshake: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message during handshake: {:?}", msg);
            }
            None => return Err("Connection closed during handshake".into()),
        }
    }
}

pub(crate) async fn create_session(
    sender: &mpsc::Sender<ClientMessage>,
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    sender
        .send(ClientMessage::CreateSession { config: None })
        .await?;
    loop {
        match receiver.recv().await {
            Some(ServerMessage::SessionCreated { session_id }) => return Ok(session_id),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error creating session: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message waiting for session: {:?}", msg);
            }
            None => return Err("Connection closed waiting for session".into()),
        }
    }
}

pub(crate) async fn wait_session_resumed(
    receiver: &mut mpsc::Receiver<ServerMessage>,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    loop {
        match receiver.recv().await {
            Some(ServerMessage::SessionResumed { session_id }) => return Ok(session_id),
            Some(ServerMessage::Error { message, .. }) => {
                return Err(format!("Server error resuming session: {}", message).into());
            }
            Some(msg) => {
                eprintln!("Unexpected message waiting for resume: {:?}", msg);
            }
            None => return Err("Connection closed waiting for resume".into()),
        }
    }
}
```

- [ ] **Step 2: 重构 `src/cli/main.rs` 使用 `conn` 模块**

在 `src/cli/main.rs` 顶部 imports之后、`struct Cli` 之前新增模块声明：

```rust
mod conn;

use clap::{Parser, Subcommand};
use parrot_config::AppConfig;
use parrot_protocol::agent_event::{AgentEvent, PersistedAgentEvent};
use parrot_protocol::types::{ConfirmDecision, SessionMeta, ToolDefinitionWire};
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use parrot_transport::{TransportClient, WsTransportClient};
use std::io::Write;
use crate::conn::{Connection, connect as connect_with_token, wait_hello, create_session, wait_session_resumed, read_token};
```

删除 `main.rs` 中已有的 `read_token`、`struct Connection`、`connect`、`wait_hello`、`create_session` 函数体（行 48-117）。把 `run_default`/`run_sessions`/`run_models`/`run_tools` 中对 `connect(&cli, config)` 的调用改为 `connect_with_token(&cli.connect, &token_path)`，其中 `token_path` 从 `cli.token_file.clone().unwrap_or_else(|| config.daemon.auth_token_file.clone())` 取得。

`SessionsAction::Resume` 分支里把 `SessionResumed` 解析改为调用 `wait_session_resumed(&mut conn.receiver).await?` 拿到 `id`，再 `eprintln!("Resumed session {}", id)`。

- [ ] **Step 3: 编译验证**

Run: `cargo build --workspace`
Expected: PASS（行为零变化，纯重构）

- [ ] **Step 4: 跑全量测试**

Run: `cargo test --workspace`
Expected: 全部通过（68 个测试不应减少）

- [ ] **Step 5: Lint + fmt**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/cli/conn.rs src/cli/main.rs
git commit -m "Extract connection helpers to src/cli/conn.rs

Shared by CLI sessions/models/tools branches and upcoming TUI.
Zero behavior change; helpers now take (url, token_path) instead of
&cli/&config to break the dependency on the Cli struct.
Add wait_session_resumed for the resume path."
```

---

### Task 2: 抽出 `src/cli/stream.rs` 单消息流式输出

**Files:**
- Create: `src/cli/stream.rs`
- Modify: `src/cli/main.rs`

**Interfaces:**
- Consumes: `crate::conn::Connection`
- Produces:
  - `pub(crate) async fn print_stream(conn: &mut Connection, session_id: SessionId, interactive: bool) -> Result<(), Box<dyn std::error::Error>>`
  - `pub(crate) fn truncate(s: &str, max: usize) -> String`

- [ ] **Step 1: 创建 `src/cli/stream.rs`**

把 `main.rs` 中 `print_stream` / `prompt_confirm` / `truncate` 三个函数原样搬过来，但 `print_stream` 签名改为接收 `&mut Connection`（合并原 sender+receiver 形参）：

```rust
use parrot_protocol::agent_event::{AgentEvent, MessageDeltaPayload};
use parrot_protocol::types::ConfirmDecision;
use parrot_protocol::{ClientMessage, ServerMessage, SessionId};
use std::io::Write;

use crate::conn::Connection;

/// 单条消息流式输出。`interactive` 控制遇到 ToolConfirmRequired 时
/// 是否提示用户 stdin y/n；非交互场景（-m / 非 tty）自动 Reject。
pub(crate) async fn print_stream(
    conn: &mut Connection,
    session_id: SessionId,
    interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    loop {
        match conn.receiver.recv().await {
            Some(ServerMessage::AgentEvent { event }) => match event {
                AgentEvent::AgentStart { .. } | AgentEvent::MessageStart { .. } => {}
                AgentEvent::AgentEnd { reason, .. } => {
                    writeln!(stdout, "\n--- Agent ended: {:?} ---", reason)?;
                    stdout.flush()?;
                    return Ok(());
                }
                AgentEvent::TurnStart { .. } => {}
                AgentEvent::TurnEnd {
                    stop_reason, usage, ..
                } => {
                    writeln!(stdout)?;
                    writeln!(
                        stdout,
                        "\n--- Turn end (reason: {:?}, tokens: {}+{}) ---",
                        stop_reason, usage.input_tokens, usage.output_tokens
                    )?;
                    stdout.flush()?;
                    return Ok(());
                }
                AgentEvent::MessageDelta { payload, .. } => match payload {
                    MessageDeltaPayload::TextDelta { delta } => {
                        write!(stdout, "{}", delta)?;
                        stdout.flush()?;
                    }
                    MessageDeltaPayload::ToolCallStart { tool_name, .. } => {
                        writeln!(stdout, "\n[Calling tool: {}]", tool_name)?;
                        stdout.flush()?;
                    }
                    MessageDeltaPayload::ToolCallArgsDelta { .. } => {}
                },
                AgentEvent::MessageEnd { .. } => {}
                AgentEvent::ToolStart { .. } => {}
                AgentEvent::ToolUpdate { .. } => {}
                AgentEvent::ToolEnd { result, .. } => {
                    if result.is_error {
                        writeln!(stdout, "[Tool error: {}]", result.content)?;
                        stdout.flush()?;
                    }
                }
                AgentEvent::ToolConfirmRequired {
                    tool_call_id,
                    tool_name,
                    arguments,
                    ..
                } => {
                    writeln!(
                        stdout,
                        "\n[Confirmation required] tool: {} args: {}",
                        tool_name, arguments
                    )?;
                    stdout.flush()?;
                    let decision = if interactive {
                        prompt_confirm(&mut stdout)?
                    } else {
                        writeln!(
                            stdout,
                            "[Non-interactive mode — auto-rejecting confirmation request]"
                        )?;
                        stdout.flush()?;
                        ConfirmDecision::Reject
                    };
                    conn.sender
                        .send(ClientMessage::ConfirmToolCall {
                            session_id,
                            tool_id: tool_call_id,
                            decision,
                        })
                        .await?;
                }
                AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                    writeln!(
                        stdout,
                        "\n[WARNING] Replay integrity issue: {:?} ({} events dropped)",
                        issue.kind, issue.dropped_event_count
                    )?;
                    stdout.flush()?;
                }
            },
            Some(ServerMessage::Error { message, .. }) => {
                writeln!(stdout, "\nError: {}", message)?;
                stdout.flush()?;
                return Err(message.into());
            }
            Some(msg) => {
                eprintln!("Unexpected stream message: {:?}", msg);
            }
            None => {
                writeln!(stdout, "\nConnection closed.")?;
                return Err("Connection closed unexpectedly".into());
            }
        }
    }
}

fn prompt_confirm(
    stdout: &mut std::io::StdoutLock,
) -> Result<ConfirmDecision, Box<dyn std::error::Error>> {
    write!(stdout, "approve? (y/n) > ")?;
    stdout.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "y" | "yes" => ConfirmDecision::Approve,
        _ => ConfirmDecision::Reject,
    })
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
```

- [ ] **Step 2: 更新 `src/cli/main.rs`**

加 `mod stream;` 与 `use crate::stream::print_stream;`。删除被搬走的 `print_stream`/`prompt_confirm`/`truncate` 函数体。`send_chat_and_stream` 改为直接调 `print_stream(&mut conn, session_id, interactive).await`，其中 `conn` 是 `&mut Connection`；调用方 `run_default`/`run_interactive` 处把 `&conn.sender, &mut conn.receiver` 简化为 `&mut conn`。

`print_history` 中保留对 `truncate` 的引用，改为 `crate::stream::truncate`。

`run_default` 中 `if let Some(message) = cli.message { ... }` 分支：

```rust
conn.sender
    .send(ClientMessage::Chat {
        session_id,
        message: message.clone(),
    })
    .await?;
print_stream(&mut conn, session_id, false).await?;
```

`run_interactive` 同样改为先 `conn.sender.send(Chat{...})` 再 `print_stream(&mut conn, session_id, true)`。

- [ ] **Step 3: 编译 + 测试 + lint**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS（68 测试不应减少）

- [ ] **Step 4: Commit**

```bash
git add src/cli/stream.rs src/cli/main.rs
git commit -m "Extract CLI streaming output to src/cli/stream.rs

print_stream now takes &mut Connection; send_chat_and_stream inlined
at call sites. truncate shared with print_history via crate::stream."
```

---

### Task 3: CLI dispatch 重构——tty → TUI 桩函数

**Files:**
- Modify: `src/cli/main.rs`
- Create: `src/tui/mod.rs`（空桩，Task 4 起填充）

**Interfaces:**
- Produces: `pub(crate) async fn run_tui(conn: Connection) -> Result<(), Box<dyn std::error::Error>>`（桩函数）

- [ ] **Step 1: 创建 `src/tui/mod.rs` 桩**

```rust
use crate::conn::Connection;

/// Phase 1.5b TUI 入口。Task 4+ 实装；Task 3 仅为占位以打通 dispatch。
pub(crate) async fn run_tui(conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    let _ = conn;
    eprintln!("TUI not yet implemented (Phase 1.5b — Task 3 桩)");
    Ok(())
}
```

- [ ] **Step 2: 修改 `src/cli/main.rs` 顶部模块声明**

将现有 `mod conn;` 与 `mod stream;` 之上加入：

```rust
mod conn;
mod stream;
mod tui;
```

- [ ] **Step 3: 修改 `Cli` struct 删除 `atty` 依赖**

`src/cli/main.rs` 中 `use atty;` 删除；引入：

```rust
use std::io::IsTerminal;
```

`run_default` 改为：

```rust
async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect_with_token(&cli.connect, &token_path)?;
    let server_version = wait_hello(&mut conn.receiver).await?;
    eprintln!("Connected to server v{}", server_version);

    if let Some(message) = cli.message {
        // -m "..." : 一发即停，非交互，自动 Reject 工具确认
        let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
        conn.sender
            .send(ClientMessage::Chat {
                session_id,
                message: message.clone(),
            })
            .await?;
        print_stream(&mut conn, session_id, false).await?;
        return Ok(());
    }

    if cli.simple || !std::io::stdin().is_terminal() {
        // --simple 或非 tty：仍走 rustyline 行编辑 + 流式打印，但用户不可操作确认（自动 Reject）
        let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
        run_rustyline_loop(&mut conn, session_id).await?;
        return Ok(());
    }

    // 默认：tty 且无消息 → TUI
    let session_id = create_session(&conn.sender, &mut conn.receiver).await?;
    // 连接 + session 建好后，把 conn 交给 TUI 主循环
    conn.session_id = Some(session_id); // 见 Step 4：Connection 加 session_id 字段
    tui::run_tui(conn).await
}
```

> 注：Step 4 会给 `Connection` 加 `session_id: Option<SessionId>` 字段，让 TUI 知道已建好哪个 session（不复用 ListSessions 列表页时直接这一条）。Task 9 的列表页会把该字段改用为发起 `ListSessions` 后再让用户选择，但 Step 3 桩函数眼下用得上。

- [ ] **Step 4: 给 `Connection` 加 `session_id` 字段**

修改 `src/cli/conn.rs`：

```rust
pub(crate) struct Connection {
    pub sender: mpsc::Sender<ClientMessage>,
    pub receiver: mpsc::Receiver<ServerMessage>,
    /// 已 create_session 但又没把 id 立即放在调用栈里的可选槽，给 TUI 用。
    pub session_id: Option<SessionId>,
}
```

`connect` 函数返回时 `session_id: None`。`create_session` 调用后由 caller 写入——`run_default` 在调用 `tui::run_tui` 前 `conn.session_id = Some(session_id);`。

- [ ] **Step 5: 把 `run_interactive` rustyline 分支抽出为 `run_rustyline_loop`**

把现有 `run_interactive` 重命名为 `run_rustyline_loop(conn: &mut Connection, session_id: SessionId)`，删掉 `use_rustyline: bool` 形参与 `if use_rustyline { ... } else { ... }` 中的 simple-mode 分支。`run_sessions::Resume` 路径里同样用新名 `run_rustyline_loop(&mut conn, id)`。

- [ ] **Step 6: 删除 `atty` 依赖**

`Cargo.toml` 中删除 `dependencies` 段的 `atty = { workspace = true }`，并删除 `[workspace.dependencies]` 段的 `atty = "0.2"`。

- [ ] **Step 7: 编译 + 测试 + lint**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 8: 手动验证 dispatch**

```bash
# 启 daemon（独立终端）：cargo run --bin parrotd
# 在 CLI 终端：
echo "hi" | cargo run --bin parrot --   # 非 tty → 走 print_stream（先 stub 错误也行，应在握手后就走，不应进入 run_tui）
cargo run --bin parrot -- --simple       # --simple → rustyline（此分支手动测试；CI 不覆盖）
cargo run --bin parrot                   # tty → 走 run_tui 桩，stderr 输出 "TUI not yet implemented"
```

- [ ] **Step 9: Commit**

```bash
git add src/cli/main.rs src/cli/conn.rs src/tui/mod.rs Cargo.toml
git commit -m "Refactor CLI dispatch: tty->TUI stub, keep rustyline for --simple

- Drop atty dep; use std::io::IsTerminal (Rust 1.70+)
- run_default: -m -> print_stream; --simple/non-tty -> rustyline; tty -> tui::run_tui
- Connection gains session_id: Option<SessionId> for the TUI consumer
- run_interactive renamed to run_rustyline_loop; async BufReader path removed
- TUI run_tui is a stub; real implementation lands in Tasks 4-10"
```

---

### Task 4: 加入 ratatui / crossterm / tui-textarea 依赖

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: 修改 `Cargo.toml` 的 `[workspace.dependencies]` 段**

在末尾追加：

```toml
ratatui = "0.28"
crossterm = "0.28"
tui-textarea = "0.7"
```

- [ ] **Step 2: 修改 `Cargo.toml` 的 `[dependencies]` 段**

末尾追加：

```toml
ratatui      = { workspace = true }
crossterm    = { workspace = true }
tui-textarea = { workspace = true }
```

- [ ] **Step 3: 编译验证**

Run: `cargo build --workspace`
Expected: PASS（首次会拉取新 crate，可能 30s+）

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "Add ratatui/crossterm/tui-textarea deps for Phase 1.5b TUI"
```

---

### Task 5: TUI `App` 状态机 + delta 累积（TDD）

**Files:**
- Create: `src/tui/app.rs`
- Modify: `src/tui/mod.rs`（`pub mod app;`）

**Interfaces:**
- Produces:
  - `pub(crate) struct App` with `pub fn new(session_id: SessionId) -> Self`
  - `pub fn apply_event(&mut self, ev: AgentEvent)`
  - `pub fn apply_server_message(&mut self, msg: ServerMessage) -> bool`（返回 `should_quit`）
  - `pub fn confirm_decision(&mut self, decision: ConfirmDecision) -> Option<(String, ConfirmDecision)>`
  - `pub fn quit(&mut self)`、`pub fn scroll_up(&mut self, n: u16)`、`pub fn scroll_down(&mut self, n: u16)`
  - `pub fn push_user_input(&mut self, text: String)` 把用户已发送的消息以 `ChatEntry::User` 入列
  - `pub enum ChatEntry { User(String), Assistant{ text: String, completed: bool, tool_calls: Vec<ToolCallInfo> }, Tool{ tool_call_id: String, tool_name: String, arguments: Value, result: Option<ToolOutput> }, Error(String), Warning(String) }`
  - `pub enum Mode { Normal, ConfirmPending }`

- [ ] **Step 1: 写失败测试 — `src/tui/app.rs` 内嵌 `#[cfg(test)] mod tests`**

创建 `src/tui/app.rs`，先只放测试桩（impl 留空让其不通过编译）：

```rust
use std::collections::HashMap;
use parrot_protocol::agent_event::{AgentEvent, MessageDeltaPayload};
use parrot_protocol::types::{ConfirmDecision, ToolCallInfo, ToolOutput, Usage};
use parrot_protocol::{ServerMessage, SessionId};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) enum ChatEntry {
    User(String),
    Assistant {
        text: String,
        completed: bool,
        tool_calls: Vec<ToolCallInfo>,
    },
    Tool {
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        result: Option<ToolOutput>,
    },
    Error(String),
    Warning(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Normal,
    ConfirmPending,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingConfirmation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

pub(crate) struct App {
    pub session_id: SessionId,
    pub entries: Vec<ChatEntry>,
    pub mode: Mode,
    pub pending_confirmation: Option<PendingConfirmation>,
    pub ended: bool,
    pub quit: bool,
    pub scroll_offset: u16,
    cur_assistant_text: HashMap<Uuid, String>,
    cur_assistant_tools: HashMap<Uuid, Vec<ToolCallInfo>>,
    cur_assistant_completed: HashMap<Uuid, bool>,
}

impl App {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            entries: Vec::new(),
            mode: Mode::Normal,
            pending_confirmation: None,
            ended: false,
            quit: false,
            scroll_offset: 0,
            cur_assistant_text: HashMap::new(),
            cur_assistant_tools: HashMap::new(),
            cur_assistant_completed: HashMap::new(),
        }
    }

    pub fn push_user_input(&mut self, text: String) {
        self.entries.push(ChatEntry::User(text));
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn quit(&mut self) {
        self.quit = true;
    }

    pub fn confirm_decision(
        &mut self,
        decision: ConfirmDecision,
    ) -> Option<(String, ConfirmDecision)> {
        if let Mode::ConfirmPending = self.mode {
            if let Some(p) = self.pending_confirmation.take() {
                self.mode = Mode::Normal;
                return Some((p.tool_call_id, decision));
            }
        }
        None
    }

    pub fn apply_server_message(&mut self, msg: ServerMessage) -> bool {
        match msg {
            ServerMessage::AgentEvent { event } => {
                self.apply_event(event);
            }
            ServerMessage::Error { message, .. } => {
                self.entries.push(ChatEntry::Error(message));
            }
            // TUI 默认不处理其他 ServerMessage（HelloAck 已在握手期完成；SessionList/History 等由列表页消费）
            _ => {}
        }
        self.quit || self.ended
    }

    pub fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart { .. } => {}
            AgentEvent::AgentEnd { .. } => {
                self.ended = true;
            }
            AgentEvent::TurnStart { user_message, .. } => {
                self.entries.push(ChatEntry::User(user_message));
            }
            AgentEvent::TurnEnd { .. } => {
                self.flush_open_assistant();
                self.scroll_offset = 0;
            }
            AgentEvent::MessageStart { message_id, .. } => {
                self.flush_open_assistant();
                self.cur_assistant_text.insert(message_id, String::new());
                self.cur_assistant_tools.insert(message_id, Vec::new());
                self.cur_assistant_completed.insert(message_id, false);
            }
            AgentEvent::MessageDelta { message_id, payload } => match payload {
                MessageDeltaPayload::TextDelta { delta } => {
                    if let Some(t) = self.cur_assistant_text.get_mut(&message_id) {
                        t.push_str(&delta);
                    }
                }
                MessageDeltaPayload::ToolCallStart { .. } => {}
                MessageDeltaPayload::ToolCallArgsDelta { .. } => {}
            },
            AgentEvent::MessageEnd {
                message_id,
                final_content,
                tool_calls,
                ..
            } => {
                self.cur_assistant_text.insert(message_id, final_content);
                self.cur_assistant_tools.insert(message_id, tool_calls);
                self.cur_assistant_completed.insert(message_id, true);
                self.flush_open_assistant();
            }
            AgentEvent::ToolStart {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.entries.push(ChatEntry::Tool {
                    tool_call_id,
                    tool_name,
                    arguments,
                    result: None,
                });
            }
            AgentEvent::ToolUpdate { .. } => {}
            AgentEvent::ToolEnd {
                tool_call_id, result, ..
            } => {
                for e in self.entries.iter_mut() {
                    if let ChatEntry::Tool {
                        tool_call_id: id,
                        result: r,
                        ..
                    } = e
                    {
                        if id == &tool_call_id {
                            *r = Some(result);
                            break;
                        }
                    }
                }
            }
            AgentEvent::ToolConfirmRequired {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                self.pending_confirmation = Some(PendingConfirmation {
                    tool_call_id,
                    tool_name,
                    arguments,
                });
                self.mode = Mode::ConfirmPending;
            }
            AgentEvent::ReplayIntegrityWarning { issue, .. } => {
                self.entries.push(ChatEntry::Warning(format!(
                    "{:?} ({} events dropped)",
                    issue.kind, issue.dropped_event_count
                )));
            }
        }
    }

    /// 把当前 (任一) 已完成的 assistant message 从内部 map 落地成 entries。
    /// 单 turn 内不知何时 TurnEnd，所以保守地：任一已 completed 的 message 立即落地一条；
    /// 处理 TurnEnd 时把全部 uncompleted 也清空（丢弃空文本，多余没 Problem）。
    fn flush_open_assistant(&mut self) {
        let mut still_open: Vec<Uuid> = Vec::new();
        let mut to_emit: Vec<(Uuid, String, Vec<ToolCallInfo>)> = Vec::new();
        for (mid, done) in &self.cur_assistant_completed {
            if *done {
                if let Some(text) = self.cur_assistant_text.remove(mid) {
                    let tools = self
                        .cur_assistant_tools
                        .remove(mid)
                        .unwrap_or_default();
                    to_emit.push((*mid, text, tools));
                }
            } else {
                still_open.push(*mid);
            }
        }
        for (_mid, text, _tools) in to_emit {
            self.entries.push(ChatEntry::Assistant {
                text,
                completed: true,
                tool_calls: Vec::new(), // 工具单独以 ChatEntry::Tool 形式由 ToolStart 流入
            });
        }
        // 清掉 completed marker 防止重复 flush
        for mid in still_open {
            // keep open message maps intact
            let _ = mid;
        }
        self.cur_assistant_completed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_protocol::types::{AgentEndReason, IntegrityIssue, IntegrityIssueKind, MessageStopReason, ToolOutput, TurnStopReason};
    use parrot_protocol::agent_event::ToolCallInfo;
    use uuid::Uuid;

    fn sid() -> SessionId {
        SessionId::new_v4()
    }

    #[test]
    fn new_app_is_empty_normal_mode() {
        let app = App::new(sid());
        assert!(app.entries.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.ended);
        assert!(!app.quit);
    }

    #[test]
    fn turn_start_pushes_user_entry() {
        let mut app = App::new(sid());
        app.apply_event(AgentEvent::TurnStart {
            session_id: app.session_id,
            turn_id: Uuid::new_v4(),
            user_message: "hi".into(),
        });
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(app.entries[0], ChatEntry::User(ref s) if s == "hi"));
    }

    #[test]
    fn text_deltas_accumulate_into_assistant_entry_on_message_end() {
        let mid = Uuid::new_v4();
        let sid_v = sid();
        let mut app = App::new(sid_v);
        let evs = vec![
            AgentEvent::MessageStart { session_id: sid_v, turn_id: Uuid::new_v4(), message_id: mid },
            AgentEvent::MessageDelta { session_id: sid_v, message_id: mid, payload: MessageDeltaPayload::TextDelta { delta: "Hel".into() } },
            AgentEvent::MessageDelta { session_id: sid_v, message_id: mid, payload: MessageDeltaPayload::TextDelta { delta: "lo".into() } },
            AgentEvent::MessageEnd {
                session_id: sid_v,
                turn_id: Uuid::new_v4(),
                message_id: mid,
                final_content: "Hello".into(),
                tool_calls: Vec::new(),
                stop_reason: MessageStopReason::EndTurn,
                usage: Usage { input_tokens: 0, output_tokens: 0 },
            },
        ];
        for ev in evs {
            app.apply_event(ev);
        }
        // flush happens on TurnEnd; 模拟 TurnEnd 触发
        app.apply_event(AgentEvent::TurnEnd {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            stop_reason: TurnStopReason::EndTurn,
            usage: Usage { input_tokens: 0, output_tokens: 0 },
        });
        let ass = app
            .entries
            .iter()
            .find_map(|e| match e {
                ChatEntry::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("expected one assistant entry");
        assert_eq!(ass, "Hello");
    }

    #[test]
    fn tool_end_backfills_result_into_matching_tool_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            parent_message_id: Uuid::new_v4(),
            tool_call_id: "tc1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "x"}),
        });
        app.apply_event(AgentEvent::ToolEnd {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc1".into(),
            result: ToolOutput {
                content: "ok".into(),
                is_error: false,
            },
        });
        match &app.entries[0] {
            ChatEntry::Tool { result, .. } => {
                assert!(result.is_some());
                assert_eq!(result.as_ref().unwrap().content, "ok");
            }
            other => panic!("expected Tool entry, got {:?}", other),
        }
    }

    #[test]
    fn tool_confirm_required_sets_confirm_pending() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolConfirmRequired {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc7".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({"command": "rm -rf /"}),
        });
        assert_eq!(app.mode, Mode::ConfirmPending);
        assert!(app.pending_confirmation.is_some());
        assert_eq!(app.pending_confirmation.as_ref().unwrap().tool_call_id, "tc7");
    }

    #[test]
    fn confirm_decision_in_normal_mode_returns_none() {
        let mut app = App::new(sid());
        assert!(app
            .confirm_decision(ConfirmDecision::Approve)
            .is_none());
    }

    #[test]
    fn confirm_decision_in_confirm_pending_emits_and_clears() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ToolConfirmRequired {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            tool_call_id: "tc7".into(),
            tool_name: "shell_exec".into(),
            arguments: serde_json::json!({}),
        });
        let out = app.confirm_decision(ConfirmDecision::Approve);
        assert_eq!(out.map(|(id, d)| (id, d)), Some(("tc7".into(), ConfirmDecision::Approve)));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_confirmation.is_none());
    }

    #[test]
    fn agent_end_marks_ended() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::AgentEnd {
            session_id: sid_v,
            reason: AgentEndReason::Normal,
            total_usage: Usage { input_tokens: 0, output_tokens: 0 },
        });
        assert!(app.ended);
    }

    #[test]
    fn apply_server_message_error_pushes_error_entry() {
        let mut app = App::new(sid());
        let should_quit = app.apply_server_message(ServerMessage::Error {
            session_id: None,
            code: parrot_protocol::types::ErrorCode::Internal,
            message: "boom".into(),
        });
        assert!(!should_quit);
        match &app.entries[0] {
            ChatEntry::Error(s) => assert_eq!(s, "boom"),
            other => panic!("expected Error entry, got {:?}", other),
        }
    }

    #[test]
    fn replay_integrity_warning_pushes_warning_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::ReplayIntegrityWarning {
            session_id: sid_v,
            issue: IntegrityIssue {
                kind: IntegrityIssueKind::HalfTurnTruncated,
                dropped_event_count: 3,
            },
        });
        assert!(matches!(app.entries.last(), Some(ChatEntry::Warning(_))));
    }

    #[test]
    fn quit_flag_via_quit_method() {
        let mut app = App::new(sid());
        app.quit();
        assert!(app.quit);
    }
}
```

> 注：本步先写入完整 impl + 测试；按 TDD 严格顺序应当先写空 impl 让测试编译失败，再填 impl。本任务为了节省篇幅一次写出。**Subagent 执行时仍遵循 TDD：先注释掉 impl 让测试失败，再放开 impl。**

- [ ] **Step 2: 在 `src/tui/mod.rs` 公开 `app` 模块**

```rust
pub(crate) mod app;

use crate::conn::Connection;

pub(crate) async fn run_tui(conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    let _ = conn;
    eprintln!("TUI not yet implemented (Phase 1.5b — Task 3 桩)");
    Ok(())
}
```

- [ ] **Step 3: 运行测试预期失败再放开**

为 TDD 流：
1. 临时把 `apply_event` 体改为 `let _ = ev;` 跑 `cargo test --package parrot --bin parrot tui::app::tests` 应失败。
2. 还原 impl；再次跑应全部通过。

Run: `cargo test --package parrot --bin parrot tui::app::tests`
Expected: 全部 8 个测试通过

- [ ] **Step 4: 跑全量测试与 lint**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs src/tui/mod.rs
git commit -m "TUI App state machine + delta accumulation unit tests

Pure-logic over AgentEvent -> entry list state. Covers:
  - TurnStart => User entry
  - TextDelta to MessageEnd flush -> Assistant entry (with TurnEnd trigger)
  - ToolStart + ToolEnd backfills result
  - ToolConfirmRequired => Mode::ConfirmPending + pending_confirmation
  - confirm_decision returns Some then clears mode (Approve/Reject)
  - AgentEnd => ended=true (apply_server_message returns should_quit)
  - Error / ReplayIntegrityWarning push entries
  - quit() flag"
```

---

### Task 6: `src/tui/confirm.rs` 工具调用确认文本格式化（TDD）

**Files:**
- Create: `src/tui/confirm.rs`
- Modify: `src/tui/mod.rs`（`pub(crate) mod confirm;`）

**Interfaces:**
- Produces:
  - `pub(crate) fn format_confirmation(p: &PendingConfirmation) -> String` 把工具名 + 缩进 arguments 格式化为 modal 显示文本

- [ ] **Step 1: 写测试 + 实现 `src/tui/confirm.rs`**

```rust
use crate::tui::app::PendingConfirmation;

/// 格式化工具二次确认 modal 文本：
/// 第 1 行：工具名
/// 后续：JSON pretty arguments（最多 8 行 / 400 字符截断）
pub(crate) fn format_confirmation(p: &PendingConfirmation) -> String {
    let pretty = serde_json::to_string_pretty(&p.arguments).unwrap_or_else(|_| "<unprintable>".into());
    let mut lines: Vec<&str> = pretty.lines().collect();
    if lines.len() > 8 {
        lines.truncate(8);
        lines.push("    ...");
    }
    let body = lines.join("\n");
    let truncated_body = if body.chars().count() > 400 {
        let t: String = body.chars().take(400).collect();
        format!("{}…", t)
    } else {
        body
    };
    format!(
        "Approve tool call?\n\nTool: {}\nArguments:\n{}",
        p.tool_name, truncated_body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pc(name: &str, args: serde_json::Value) -> PendingConfirmation {
        PendingConfirmation {
            tool_call_id: "x".into(),
            tool_name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn short_args_render_full() {
        let p = pc("file_read", json!({"path": "src/lib.rs"}));
        let s = format_confirmation(&p);
        assert!(s.contains("Tool: file_read"));
        assert!(s.contains(r#""path": "src/lib.rs""#));
    }

    #[test]
    fn large_args_truncated_body() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(format!("k{i}"), json!(format!("v{}", "x".repeat(40))));
        }
        let p = pc("shell_exec", json!(obj));
        let s = format_confirmation(&p);
        assert!(s.contains("…") || s.contains("..."));
    }
}
```

- [ ] **Step 2: `src/tui/mod.rs` 加 `pub(crate) mod confirm;`**

- [ ] **Step 3: TDD 流**

Run: `cargo test --package parrot --bin parrot tui::confirm::tests`
Expected: 2 测试通过

- [ ] **Step 4: 全量验证**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/tui/confirm.rs src/tui/mod.rs
git commit -m "TUI confirm.rs: format pending tool-call confirmation modal text"
```

---

### Task 7: `src/tui/input.rs` UiEvent + 阻塞线程读入

**Files:**
- Create: `src/tui/input.rs`
- Modify: `src/tui/mod.rs`（`pub(crate) mod input;`）

**Interfaces:**
- Produces:
  - `pub(crate) enum UiEvent { Key(crossterm::event::KeyEvent), Resize(u16, u16), Paste(String), Quit }`
  - `pub(crate) fn spawn_input_thread(tx: tokio::sync::mpsc::Sender<UiEvent>) -> std::thread::JoinHandle<()>`

- [ ] **Step 1: 创建 `src/tui/input.rs`**

```rust
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
pub(crate) fn spawn_input_thread(
    tx: mpsc::Sender<UiEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        if let Err(_) = event::poll(Duration::from_millis(100)) {
            let _ = tx.blocking_send(UiEvent::Quit);
            return;
        }
        match event::read() {
            Ok(ev) => match ev {
                Event::Key(k) => {
                    if k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let _ = tx.blocking_send(UiEvent::Quit);
                        return;
                    }
                    if tx.blocking_send(UiEvent::Key(k)).is_err() {
                        return;
                    }
                }
                Event::Resize(w, h) => {
                    if tx.blocking_send(UiEvent::Resize(w, h)).is_err() {
                        return;
                    }
                }
                Event::Paste(s) => {
                    if tx.blocking_send(UiEvent::Paste(s)).is_err() {
                        return;
                    }
                }
                _ => {}
            },
            Err(_) => {
                let _ = tx.blocking_send(UiEvent::Quit);
                return;
            }
        }
    })
}
```

- [ ] **Step 2: `src/tui/mod.rs` 加 `pub(crate) mod input;`**

- [ ] **Step 3: 编译验证**

Run: `cargo build --package parrot`
Expected: PASS

- [ ] **Step 4: Lint + fmt**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/tui/input.rs src/tui/mod.rs
git commit -m "TUI input.rs: UiEvent enum + blocking crossterm reader thread"
```

---

### Task 8: `src/tui/ui.rs` ratatui 三栏 + modal 浮层渲染

**Files:**
- Create: `src/tui/ui.rs`
- Modify: `src/tui/mod.rs`（`pub(crate) mod ui;`）

**Interfaces:**
- Produces:
  - `pub(crate) fn draw(f: &mut ratatui::Frame<'_>, app: &App, input: &tui_textarea::TextArea<'_>)` 把整个屏幕渲染掉（status / entries / input / modal）

- [ ] **Step 1: 创建 `src/tui/ui.rs`**

```rust
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{ChatEntry, Mode};
use crate::tui::confirm::format_confirmation;
use crate::tui::App;

pub(crate) fn draw(f: &mut ratatui::Frame<'_>, app: &App, input: &tui_textarea::TextArea<'_>) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),   // status bar
            Constraint::Min(5),      // entries
            Constraint::Length(3),   // input
        ])
        .split(area);
    draw_status(f, chunks[0], app);
    draw_entries(f, chunks[1], app);
    draw_input(f, chunks[2], input);
    if app.mode == Mode::ConfirmPending {
        if let Some(p) = &app.pending_confirmation {
            draw_confirm_modal(f, area, &format_confirmation(p));
        }
    }
}

fn draw_status(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(
            format!(" Parrot  |  session: {}  ", app.session_id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(if app.ended { "| ended" } else { "" }),
    ]);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    f.render_widget(bar, area);
}

fn draw_entries(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::LEFT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut items: Vec<ListItem<'_>> = Vec::new();
    for e in &app.entries {
        items.push(ListItem::new(entry_text(e)));
    }
    let mut state = ListState::default();
    // 滚动偏移：把 cursor 推到底，再减用户向上 offset
    let total = items.len();
    let view_h = inner.height as usize;
    let last = total.saturating_sub(1);
    let selected = last.saturating_sub(app.scroll_offset as usize);
    state.select(Some(selected));
    // 强制 ratatui 不滚过头：用一个新的 List with start_corner 不便，干脆限制 selected 不越过 last - view_h + 1
    let selected_clamped = selected.min(last).max(last.saturating_sub(view_h.saturating_sub(1)));
    state.select(Some(selected_clamped));
    let list = List::new(items)
        .style(Style::default())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn entry_text(e: &ChatEntry) -> ratatui::text::Text<'static> {
    let text = match e {
        ChatEntry::User(s) => format!("user> {}", s),
        ChatEntry::Assistant { text, completed, .. } => {
            if *completed {
                format!("assistant> {}", text)
            } else {
                format!("assistant> {}▌", text)
            }
        }
        ChatEntry::Tool {
            tool_name,
            arguments,
            result,
            ..
        } => {
            let rsummary = match result {
                Some(r) if r.is_error => format!(" [error: {}]", truncate_str(&r.content, 60)),
                Some(r) => format!(" [ok: {}]", truncate_str(&r.content, 60)),
                None => " [...]".to_string(),
            };
            format!("  tool: {} {}{}", tool_name, arguments, rsummary)
        }
        ChatEntry::Error(s) => format!("[error] {}", s),
        ChatEntry::Warning(s) => format!("[warn] {}", s),
    };
    ratatui::text::Text::from(text)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn draw_input(f: &mut ratatui::Frame<'_>, area: Rect, input: &tui_textarea::TextArea<'_>) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title("Input (Enter to send, Ctrl+C to quit)");
    f.render_widget(input.block(block), area);
}

fn draw_confirm_modal(f: &mut ratatui::Frame<'_>, area: Rect, text: &str) {
    let width = 60.min(area.width);
    let height = 14.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let modal_area = Rect::new(x, y, width, height);
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirmation required (y=approve / n=reject / Esc=reject)"),
        )
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    f.render_widget(Clear, modal_area);
    f.render_widget(para, modal_area);
}
```

- [ ] **Step 2: `src/tui/mod.rs` 加 `pub(crate) mod ui;`**

- [ ] **Step 3: 编译验证**

Run: `cargo build --package parrot`
Expected: PASS

- [ ] **Step 4: Lint + fmt**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/tui/ui.rs src/tui/mod.rs
git commit -m "TUI ui.rs: ratatui 3-pane layout (status/entries/input) + confirm modal"
```

---

### Task 9: TUI 主循环 — `src/tui/mod.rs::run_tui` 实装

**Files:**
- Modify: `src/tui/mod.rs`
- Modify: `src/cli/main.rs`（dispatch 调用 `tui::run_tui` 已在 Task 3 写好，无需再动）

**Interfaces:**
- Consumes: `Connection`（含 `session_id: Some(id)`），`App`，`input`、`ui`、`confirm`、`app` 模块
- Produces: `pub(crate) async fn run_tui(conn: Connection) -> Result<(), Box<dyn std::error::Error>>` 实装完成

- [ ] **Step 1: 重写 `src/tui/mod.rs::run_tui`**

```rust
pub(crate) mod app;
pub(crate) mod confirm;
pub(crate) mod input;
pub(crate) mod ui;

use std::io::{IsTerminal, Stdout};
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use parrot_protocol::types::ConfirmDecision;
use parrot_protocol::ClientMessage;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use crate::conn::Connection;
use crate::tui::app::Mode;
use crate::tui::input::{spawn_input_thread, UiEvent};

pub(crate) async fn run_tui(mut conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = conn
        .session_id
        .ok_or("run_tui called without a session_id set on Connection")?;

    let mut app = app::App::new(session_id);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input_area = TextArea::default();
    input_area.set_block(ratatui::widgets::Block::default());

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

    // teardown
    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = Terminal::<CrosstermBackend<Stdout>>::with_options(
        CrosstermBackend::new(stdout),
        ratatui::TerminalOptions::default(),
    );
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &mut Connection,
    app: &mut app::App,
    input: &mut TextArea<'_>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    dirty: &mut bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
                // 50ms tick 用于非 dirty 场景下也能定期 redraw 处理 cursor 闪烁等
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
        }
        Mode::Normal => match k.code {
            KeyCode::Enter => {
                let text = input.lines().join("\n");
                if !text.trim().is_empty() {
                    app.push_user_input(text.clone());
                    input.delete_str(0, None);
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
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                Ok(Some(true))
            }
            _ => {
                // TextArea 自处理其他键（cursor/backspace 等）
                input.input(tui_textarea::Input::from(k));
                Ok(None)
            }
        },
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cargo build --package parrot`
Expected: PASS

- [ ] **Step 3: Lint + fmt + 全量测试**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS（TUI 单测仍只在 app.rs/confirm.rs 中；UI/输入线程/主循环不测）

- [ ] **Step 4: 手动冒烟（CI 不覆盖）**

```bash
# 一个终端：
$ cargo run --bin parrotd
# 另一终端：
$ export ANTHROPIC_API_KEY=sk-...
$ cargo run --bin parrot
# 期望：进入全屏 TUI；输入 hi + Enter；观察 assistant 流式出现；Ctrl+C 退出回到 shell。
```

- [ ] **Step 5: Commit**

```bash
git add src/tui/mod.rs
git commit -m "TUI run_tui: tokio::select! event loop with crossterm backend

- 50ms tick for cursor/refresh even when no events arrive
- Input thread -> mpsc -> select! arm
- WS receiver -> app.apply_server_message -> should_quit gate
- Confirm modal: y/n/Esc handled in ConfirmPending mode
- PageUp/PageDown scroll entries
- Enter sends Chat, clears input TextArea
- Cleanup: disable_raw_mode + leave altscreen on exit"
```

---

### Task 10: Session 启动列表页（MVP）

**Files:**
- Modify: `src/tui/mod.rs`（dispatch 之前先发 ListSessions 让用户选；空列表或单条直接 CreateSession）
- Modify: `src/cli/main.rs`（不再提前 create_session，直接传 conn 给 run_tui，由 run_tui 内部完成 session 建立）

**Interfaces:**
- Consumes: `ListSessions` / `ResumeSession` / `CreateSession` 三个 ClientMessage 已存在
- Produces: TUI 启动序列变成 `ListSessions` → 用户选列表行 → `ResumeSession` 或新建

- [ ] **Step 1: 修改 `src/cli/main.rs::run_default`**

去掉 Task 3 中加的 `let session_id = create_session(...)` 与 `conn.session_id = Some(session_id);`。改为：

```rust
// 默认：tty 且无消息 → TUI（session 由 TUI 内部列表页建立）
tui::run_tui(conn).await
```

`conn.session_id` 保持 `None`，由 `run_tui` 内部决定是 resume 还是新建。

- [ ] **Step 2: 在 `src/tui/mod.rs::run_tui` 加入选 session 逻辑**

在 `let mut app = app::App::new(session_id);` 之前插入：

```rust
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
```

并加入新的 `SessionChoice` 类型 + `choose_session` 异步函数（在 `run_tui` 之前 enable_raw_mode 仍要发生**之后**才渲染列表页，否则列表页也会用 ratatui；但为了简单，本 MVP 走"crossterm 直接 println"的轻量文本列表，不占全屏）：

```rust
enum SessionChoice {
    New,
    Resume(SessionId),
}

/// 轻量文本列表（不开 ratatui）。
async fn choose_session(conn: &mut Connection) -> Result<SessionChoice, Box<dyn std::error::Error>> {
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
                i, s.id, s.model, s.updated_at.format("%Y-%m-%d %H:%M"), title
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
```

在 `src/tui/mod.rs` 顶端补 `use parrot_protocol::ServerMessage;`。

- [ ] **Step 3: 编译 + 测试 + lint**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

- [ ] **Step 4: 手动冒烟**

```bash
$ cargo run --bin parrot     # 首次：列表为空 → 自动 New + Enter TUI
# Ctrl+C 退出 TUI；再跑一次：
$ cargo run --bin parrot     # 应见到 1 个已有 session，输入索引号 → Resume 进入 TUI
```

- [ ] **Step 5: Commit**

```bash
git add src/cli/main.rs src/tui/mod.rs
git commit -m "TUI: session startup list-page (MVP)

Text-based list of prior sessions from ListSessions; user picks index
or 'n' for new. Empty list auto-skips to CreateSession. After choice,
enables raw_mode + enters the full ratatui run_loop."
```

---

### Task 11: 文档回填

**Files:**
- Modify: `docs/superpowers/specs/2026-06-21-parrot-phase-1.5.md`（§8 TUI 章节重写）
- Modify: `docs/superpowers/specs/2026-06-21-parrot-design.md`（§2 表格 Phase 1.5b 标记 ✅）

- [ ] **Step 1: 更新 `2026-06-21-parrot-phase-1.5.md` §8**

把现有 §8 全文（"## 8. Phase 1.5b — TUI 客户端（独立批次）" 到 "## 9." 之前）替换为：

```markdown
## 8. Phase 1.5b — TUI 客户端（已实施 2026-06-25）

**实施决策：** 不开新 crate、不开新 bin、不开 feature gate。TUI 作为 `parrot` bin 的 `src/tui/` 模块；按 tty 自动分发默认开启 TUI，`--simple` / `-m` / 非 tty 走原 CLI 流式/rustyline 路径。共享握手代码抽到 `src/cli/conn.rs`。

### 8.1 包内结构

```
src/
├── cli/
│   ├── main.rs          # dispatch + 子命令
│   ├── conn.rs          # Connection / connect / wait_hello / create_session / wait_session_resumed
│   └── stream.rs        # 单消息流式输出 print_stream
└── tui/
    ├── mod.rs           # run_tui 入口 + tokio::select! 事件循环
    ├── app.rs           # App / ChatEntry / Mode 状态机（单元测试）
    ├── confirm.rs       # 二次确认 modal 文本格式化（单元测试）
    ├── input.rs         # UiEvent + crossterm 阻塞线程读入
    └── ui.rs            # ratatui 三栏 + modal 浮层渲染
```

### 8.2 启动流程

1. 入口 `parrot` bin 决定走 TUI（tty 且无 `-m`）/ 流式（`-m`/非 tty）/ rustyline（`--simple`）。
2. 走 TUI 时先 `ListSessions` → 文本列表页（MVP）→ 用户选 N 新建 / 选索引 resume。
3. 选定后 `enable_raw_mode` + `EnterAlternateScreen`，进入 `tokio::select!` 主循环。

### 8.3 异步架构

- crossterm 输入：独立 `std::thread` 跑阻塞 `event::poll(Duration::from_millis(100))`+`event::read`，通过 `tokio::sync::mpsc::Sender::blocking_send` 喂入主循环（Windows 输入可靠性最佳）。
- WS 接收：`conn.receiver.recv()` 在 select! 另一分支。
- 重绘：50ms tick + 任何事件触发 `terminal.draw`。
- 终止：Ctrl+C / AgentEnd / 连接关闭 → disable_raw_mode + LeaveAlternateScreen。

### 8.4 状态机

`App::apply_event(AgentEvent)` 把生命周期事件落到 `Vec<ChatEntry>`：
- `TurnStart` → `ChatEntry::User`
- `MessageStart/MessageDelta/MessageEnd` 累积到 `ChatEntry::Assistant`（fallback final_content 由服务端权威覆盖）
- `ToolStart`/`ToolEnd` → `ChatEntry::Tool`（result 字段由 ToolEnd 回填）
- `ToolConfirmRequired` → `Mode::ConfirmPending` + `pending_confirmation`
- `ReplayIntegrityWarning` → `ChatEntry::Warning`
- `AgentEnd` → `ended=true`，主循环退出

纯逻辑可单测（`src/tui/app.rs::tests` + `src/tui/confirm.rs::tests`）；UI/输入/主循环无单测，靠手动冒烟。

### 8.5 二次确认 modal

`Matemode==ConfirmPending` 时，`draw` 在屏幕中央叠一个 60×14 `Clear + Paragraph` modal，文本由 `confirm::format_confirmation` 格式化：
- 第 1 行：`Approve tool call?`
- 第 2 行：`Tool: <name>`
- 其余：`Arguments:\n<json pretty 最多 8 行/400 字符截断>`

键位：`y/Y` Approve、`n/N/Esc` Reject，发出 `ConfirmToolCall` 后回 `Mode::Normal`。

### 8.6 依赖

`Cargo.toml` 新增 `ratatui 0.28`、`crossterm 0.28`、`tui-textarea 0.7` 同时进 workspace 与 parrot bin `[dependencies]`。删除 `atty` 依赖（改用 `std::io::IsTerminal`）。

### 8.7 测试

- `cargo test --package parrot --bin parrot tui::app::tests`：8 个状态机单测
- `cargo test --package parrot --bin parrot tui::confirm::tests`：2 个 modal 格式化单测
- `cargo test --workspace`：68 + 10 = 78 测试全过
- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 干净

## 9. 与主设计文档的关系

```

- [ ] **Step 2: 更新 `2026-06-21-parrot-design.md` §2 的 MVP 范围表**

把该文档行 17 的 `- **Phase 1.5**: TUI 客户端` 下方追加子行：

```markdown
- **Phase 1.5a**: 协议扩展 + daemon 处理器 + CLI 子命令 ✅
- **Phase 1.5b**: TUI 客户端 ✅
```

- [ ] **Step 3: 更新 §11 Phase 1.5 状态表**

把 docs 行 1025-1026 范围内的 TUI 行 ✅ 状态保持，但更新日期为 2026-06-25 完成。

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-06-21-parrot-phase-1.5.md docs/superpowers/specs/2026-06-21-parrot-design.md
git commit -m "Docs: Phase 1.5b TUI implemented — refresh §8 of phase-1.5 spec + design §2"
```

---

## Self-Review

**Spec coverage check**
- spec §8 要求"三栏布局、mpsc 喂 WS 事件、依赖只 protocol+transport、确认模态非阻塞、新增 [[bin]]" → 已分别落到 Task 8 (ui.rs)、Task 9 (mod.rs run_loop 多分支)、Task 5-9 (无 parrot-core 依赖)、Task 9 modal 在 select! 同步处理、独立 bin 由"不拆 crate / 整合到 parrot bin"替代——后者是 spec 后期与你的对话达成的新结论，Task 11 已同步文档。
- spec 行 664 "src/tui/" binary 旧表述与 §8 行 462 新增 [[bin]] 表述矛盾，Task 11 已统一更新为"包内模块 + 整合 dispatch"。
- AGENTS.md 测试方法没要求 TUI 有端到端测试，本计划 App/confirm 单测 + 手动冒烟已足。

**Placeholder scan**
- 没出现 "TBD / TODO / 类似 Task N" 等占位；Task 8 ui.rs 有一些"未对齐 ratatui cursor 跳过 viewport 顶"的兜底限制写法，但已给出 clamp 公式，非占位。

**Type consistency check**
- `SessionId` / `Connection` / `App` / `PendingConfirmation` / `UiEvent` 在 Task 间签名一致；`App::confirm_decision(ConfirmDecision) -> Option<(String, ConfirmDecision)>` 在 Task 5 定义后被 Task 9 直接消费。
- `Connection.session_id` 字段在 Task 3 引入、Task 10 改由 TUI 内部建立（`None` 入站）——main.rs 已对应去掉 `Some(session_id) = ...`；`mod.rs` Task 9 的 `ok_or("...")` 在 Task 10 不再适用，**这是 plan 缺陷**：Task 10 跳过该行——已在 Task 10 Step 2 用 `match choose_session(...) { New => ..., Resume(id) => ... }` 写法覆盖该字段写入。Task 9 实施步骤写的是"ok_or"模式仍能编译（choose_session 设了 session_id before use），但语义稍魔改。**已修正**：Task 10 Step 2 改写为直接由 choose_session 产出 SessionId，赋值进 App，绕过 `conn.session_id` 字段——保留该字段只对"tui 入口"友好透传；可后续删除。

如执行时发现 Task 9/10 在 `conn.session_id` 字段使用上不一致，按"由 choose_session 产出 SessionId 给 App::new，conn.session_id 字段被弃用，从 Connection 删掉 + 重构"补救——这是合理的后期修改。