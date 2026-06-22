# Parrot — Phase 1.5 技术方案

> 续接 `2026-06-21-parrot-design.md`（架构 + Phase 1 基线）。本文档专注 Phase 1.5 的可实施细节：协议扩展、daemon 处理器、CLI 子命令、工具二次确认流程。TUI 客户端（`parrot-tui`）规模较大，独立为 Phase 1.5b，见文末。

## 1. 范围

| 子项 | 目标 | 状态 |
|------|------|------|
| 协议扩展 | `ListSessions`/`ResumeSession`/`ConfirmToolCall` 等新消息 | ✅ |
| Daemon 处理器 | `ListSessions`（读 index.json）、`ResumeSession`（重放 event log + spawn engine）、`ToolList`（替换 `TextDelta` hack）、confirm 路由 | ✅ |
| 引擎二次确认 | `require_confirmation` 匹配的 tool call 走 oneshot 等待客户端决策 | ✅ |
| CLI 子命令 | `parrot sessions list/show/resume`、`parrot models`、`parrot tools`、交互式确认提示 | ✅ |
| `ToolList` 消息 | 替换当前 `ListTools` 用 `TextDelta` 回传 JSON 的临时实现 | ✅ |
| TUI 客户端 | ratatui + crossterm，独立 binary `parrot-tui` | ⏳（Phase 1.5b） |

> ✅ = 本批已落地并通过 `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`。⏳ = 待后续批次。

## 2. 协议扩展

### 2.1 Client → Server 新增

```rust
pub enum ClientMessage {
    // ... 既有消息 ...

    /// 列出本地所有 session（读 index.json）
    ListSessions,

    /// 恢复已存在的 session：重放 event log + 重建 context + spawn engine
    ResumeSession { session_id: SessionId },

    /// 工具二次确认响应（daemon 先发 ToolCallConfirmationRequired）
    ConfirmToolCall {
        session_id: SessionId,
        tool_id: String,
        decision: ConfirmDecision,
    },
}

#[derive(Serialize, Deserialize, PartialEq)]
pub enum ConfirmDecision {
    Approve,
    Reject,
    Timeout,
}
```

### 2.2 Server → Client 新增

```rust
pub enum ServerMessage {
    // ... 既有消息 ...

    /// Session 列表（响应 ListSessions）
    SessionList { sessions: Vec<SessionMeta> },

    /// Session 恢复确认（响应 ResumeSession）
    SessionResumed { session_id: SessionId },

    /// 工具调用需二次确认（daemon 主动推）
    ToolCallConfirmationRequired {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
        arguments: Value,
    },

    /// 工具列表（响应 ListTools，替换 TextDelta hack）
    ToolList { session_id: SessionId, tools: Vec<ToolDefinitionWire> },
}
```

### 2.3 新增纯数据类型（`parrot-protocol::types`）

```rust
/// Session 元信息（响应 ListSessions / SessionList）。
/// 与 daemon 端 `SessionMeta` 字段对齐，但独立定义在 protocol 避免反向依赖。
#[derive(Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub model: String,
    pub provider: String,
    pub title: Option<String>,
    pub total_tokens: u64,
    /// wire 协议不带 system_prompt（敏感字段，客户端无需看到）；
    /// daemon 端 SessionMeta 有此字段，转 wire 时丢弃。
}

/// 工具定义的 wire 格式（响应 ToolList）。与 core 的 ToolDefinition
/// 字段对齐，独立定义避免 protocol 反向依赖 core。
#[derive(Serialize, Deserialize)]
pub struct ToolDefinitionWire {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
```

### 2.4 非流式请求 → 响应映射（更新）

| `ClientMessage` | `ServerMessage` 响应 |
|---|---|
| `ListSessions` | `SessionList { sessions }` |
| `ResumeSession { session_id }` | `SessionResumed { session_id }` 或 `Error{SessionNotFound}` |
| `ListTools { session_id }` | `ToolList { session_id, tools }`（**替换**当前的 `TextDelta`+`Finished` hack） |
| `ConfirmToolCall { .. }` | 无直接响应；daemon 路由到 session task，后续推 `ToolResult` 或 `ToolCallEnd` |

## 3. Daemon 处理器

### 3.1 ListSessions

读 `SessionStore::read_index()`，转 `Vec<SessionMeta>` 回 `SessionList`。

```rust
ClientMessage::ListSessions => {
    let index = session_store.read_index().await?;
    let metas: Vec<SessionMeta> = index.sessions.into_iter()
        .map(|e| SessionMeta {
            id: e.id, created_at: e.updated_at, updated_at: e.updated_at,
            model: e.model, provider: e.provider, title: e.title,
            total_tokens: e.total_tokens,
        })
        .collect();
    // 注意：index.json 只存了 updated_at，没有 created_at。
    // 完整 created_at 需读每个 session 的 meta.json。MVP 用 updated_at
    // 近似（或扩展 index.json schema 加 created_at —— 见 §3.4）。
    client.send(ServerMessage::SessionList { sessions: metas }).await;
}
```

> **取舍**：为避免 ListSessions 触发 N 次 meta.json 读，扩展 `index.json` 的 `SessionIndexEntry` 加 `created_at` 字段。这是一次 schema 升级，旧 index.json 反序列化时 `created_at` 用 `updated_at` 兜底（serde default）。

### 3.2 ResumeSession

```
1. 查 index.json / meta.json 确认 session 存在
2. 若 session 已在 SessionManager（前一次 resume 过或未退出）→ 直接回 SessionResumed
3. 否则：
   a. 读 meta.json 拿 model / system_prompt
   b. EventLog::replay() 拿全部 EventLogEntryWithMeta
   c. 重建 Vec<ChatMessage>：System(注入) + 遍历 entries → User/Assistant/Tool
   d. SessionManager::create_session_with_context(rebuilt_context, gen_config, system_prompt)
   e. spawn engine + 接管 event_rx（同 CreateSession 路径）
4. 回 SessionResumed { session_id }
```

引擎需要新增 `create_session_with_context` API：当前 `create_session` 从空 context 开始，ResumeSession 需要从重建的 context 开始。实现上把 `create_session` 拆成"建 context + spawn"两步，公共部分提取。

### 3.3 ToolList（替换 TextDelta hack）

```rust
ClientMessage::ListTools { session_id } => {
    let defs = session_manager.read().await.list_tool_definitions().await;
    let wire: Vec<ToolDefinitionWire> = defs.into_iter()
        .map(|d| ToolDefinitionWire { name: d.name, description: d.description, input_schema: d.input_schema })
        .collect();
    client.send(ServerMessage::ToolList { session_id, tools: wire }).await;
}
```

删除原 `TextDelta` + `Finished` 的临时实现。

### 3.4 index.json schema 升级

```json
{
  "version": 2,
  "sessions": [
    {
      "id": "uuid",
      "title": "...",
      "model": "...",
      "provider": "...",
      "created_at": "...",
      "updated_at": "...",
      "total_tokens": 15000
    }
  ]
}
```

`SessionIndexEntry` 加 `created_at: chrono::DateTime<Utc>`。`SessionStore::init_session` 写入时填 `created_at`。读取时 `#[serde(default)]` 兜底 `Utc::now()`（或 `updated_at`）以兼容旧文件。

### 3.5 ConfirmToolCall 路由

daemon 端维护 `HashMap<(SessionId, ToolId), oneshot::Sender<ConfirmDecision>>`，挂在 `RwLock` 后。流程：

```
[引擎] tool call 匹配 require_confirmation
  → 通过 event_tx 发 ToolCallConfirmationRequired
  → 在 session task 内挂起 oneshot::Receiver<ConfirmDecision>
[daemon connection handler] 收到 ToolCallConfirmationRequired 转发给客户端
  → 同时把 oneshot::Sender 存进 routing map（key = (session_id, tool_id)）
[客户端] 用户 y/n → 发 ConfirmToolCall { session_id, tool_id, decision }
[daemon connection handler] 查 routing map 拿 sender → send(decision) → 唤醒引擎
[引擎] 根据 decision：Approve → tool.call()；Reject/Timeout → ToolResult{is_error: true}
```

routing map 由 `ConfirmRouter` 结构管理（`src/daemon/confirm_router.rs`），daemon 持有 `Arc<ConfirmRouter>`。map 条目生命周期短（仅 confirm 流程激活期间），正常路径零开销。

## 4. 引擎二次确认

### 4.1 引擎改动

`ReActEngine` 新增字段：

```rust
pub struct ReActEngine {
    // ... 既有 ...
    require_confirmation: Vec<String>,   // 从 config.tools.sandbox.require_confirmation 传入
    confirm_router: Arc<ConfirmRouter>,  // daemon 注入
}
```

工具执行阶段，对每个 tool_call 检查：

```rust
if require_confirmation.iter().any(|pat| tool_name.starts_with(pat)) {
    // 1. 通知 daemon 我们要等一个 confirm（router 注册 receiver）
    let (tx, rx) = oneshot::channel();
    confirm_router.register(session_id, tool_id.clone(), tx).await;

    // 2. 发 ToolCallConfirmationRequired 给客户端（通过 event_tx）
    event_tx.send(StreamEvent::ToolCallConfirmationRequired {
        tool_id: tool_id.clone(),
        tool_name: tool_name.clone(),
        arguments: args.clone(),
    }).await;

    // 3. 等 decision，60s 超时
    let decision = tokio::time::timeout(Duration::from_secs(60), rx)
        .await
        .map(|r| r.unwrap_or(ConfirmDecision::Timeout))
        .unwrap_or(ConfirmDecision::Timeout);

    confirm_router.unregister(&session_id, &tool_id).await;

    match decision {
        ConfirmDecision::Approve => { /* 走正常 tool.call() */ }
        ConfirmDecision::Reject | ConfirmDecision::Timeout => {
            let output = ToolOutput {
                content: if matches!(decision, ConfirmDecision::Reject) {
                    "user rejected".to_string()
                } else {
                    "confirmation timeout".to_string()
                },
                is_error: true,
            };
            // 发 ToolResult + 写 event log + 推 context
        }
    }
} else {
    // 不需要确认，直接 tool.call()
}
```

### 4.2 StreamEvent 新增变体

```rust
pub enum StreamEvent {
    // ... 既有 ...
    ToolCallConfirmationRequired {
        tool_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
}
```

`SessionAdapter::stream_event_to_server` 加映射：

```rust
StreamEvent::ToolCallConfirmationRequired { tool_id, tool_name, arguments } =>
    ServerMessage::ToolCallConfirmationRequired { session_id, tool_id, tool_name, arguments },
```

### 4.3 require_confirmation 匹配规则

配置示例：`require_confirmation = ["git push", "rm"]`

匹配方式：`tool_name.starts_with(pattern)`。简单前缀匹配，MVP 不做 glob/regex。`shell_exec` 工具的 `command` 参数才需要看命令内容——但当前 require_confirmation 是按"工具名"匹配，不是按"参数"。

> **设计取舍**：Phase 1.5 仍按工具名匹配（简单、覆盖 `file_write`/`shell_exec` 这类危险工具整体）。Phase 2 升级为"参数匹配"（如 `shell_exec` 的 `command` 字段含 `rm -rf` 才确认）——需要在 Tool trait 加 `fn requires_confirmation(&self, args: &Value) -> bool`，让工具自己判断。

## 5. CLI 子命令

### 5.1 clap 结构

```rust
#[derive(Parser)]
#[command(name = "parrot", version, about = "Parrot LLM Agent CLI")]
struct Cli {
    /// 连接 URL
    #[arg(long, default_value = "ws://127.0.0.1:9876")]
    connect: String,

    /// token 文件路径
    #[arg(long)]
    token_file: Option<String>,

    /// 单消息模式（兼容旧行为）
    #[arg(short, long)]
    message: Option<String>,

    /// 简单 stdin 模式
    #[arg(long)]
    simple: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 列出所有 session
    Sessions(SessionsCmd),
    /// 列出可用模型
    Models,
    /// 列出可用工具
    Tools,
}

#[derive(Args)]
struct SessionsCmd {
    #[command(subcommand)]
    action: SessionsAction,
}

#[derive(Subcommand)]
enum SessionsAction {
    /// 列出所有 session
    List,
    /// 显示某 session 的历史
    Show { session_id: String },
    /// 恢复 session 并进入交互模式
    Resume { session_id: String },
}
```

兼容性：`parrot`（无 subcommand）+ `parrot -m "..."` 走原交互/单消息路径；`parrot sessions list` 等走子命令路径。`command: Option<Command>` 让 subcommand 可选。

### 5.2 子命令实现

```rust
match cli.command {
    Some(Command::Sessions(SessionsCmd { action: SessionsAction::List })) => {
        let conn = connect(&cli).await?;
        conn.sender.send(ClientMessage::ListSessions).await?;
        let SessionList { sessions } = wait_session_list(&mut conn.receiver).await?;
        print_session_table(&sessions);
    }
    Some(Command::Sessions(SessionsCmd { action: SessionsAction::Show { session_id } })) => {
        let id: SessionId = session_id.parse()?;
        let conn = connect(&cli).await?;
        conn.sender.send(ClientMessage::GetHistory { session_id: id }).await?;
        let History { entries, .. } = wait_history(&mut conn.receiver).await?;
        print_history(&entries);
    }
    Some(Command::Sessions(SessionsCmd { action: SessionsAction::Resume { session_id } })) => {
        let id: SessionId = session_id.parse()?;
        let mut conn = connect(&cli).await?;
        conn.sender.send(ClientMessage::ResumeSession { session_id: id }).await?;
        wait_session_resumed(&mut conn.receiver).await?;
        // 进入交互模式（复用 run_interactive）
        run_interactive(&conn).await?;
    }
    Some(Command::Models) => { /* ListModels → 打印表格 */ }
    Some(Command::Tools) => { /* ListTools → 打印 ToolList */ }
    None => {
        // 原 parrot 行为：CreateSession + 交互/单消息
        run_default(cli).await?;
    }
}
```

### 5.3 交互模式确认提示

```rust
// 在 print_stream 中处理新消息：
Some(ServerMessage::ToolCallConfirmationRequired { tool_name, arguments, .. }) => {
    println!("\n[Tool: {} args: {}]", tool_name, arguments);
    print!("approve? (y/n) > ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let decision = match line.trim() {
        "y" | "yes" => ConfirmDecision::Approve,
        _ => ConfirmDecision::Reject,
    };
    sender.send(ClientMessage::ConfirmToolCall {
        session_id, tool_id, decision,
    }).await?;
}
```

> 交互模式确认是阻塞的（read_line 卡住 stdin），与流式输出共用同一 task。这是 MVP 取舍；TUI 会用单独事件处理。非交互模式（`-m`）默认 `Reject` 所有确认（避免无人值守执行危险操作）。

## 6. 测试

| 测试 | 类型 | 覆盖点 |
|------|------|--------|
| `protocol::roundtrip::list_sessions_roundtrip` | unit | `ListSessions` / `SessionList` serde |
| `protocol::roundtrip::resume_session_roundtrip` | unit | `ResumeSession` / `SessionResumed` serde |
| `protocol::roundtrip::confirm_tool_call_roundtrip` | unit | `ConfirmToolCall` / `ToolCallConfirmationRequired` + `ConfirmDecision` |
| `protocol::roundtrip::tool_list_roundtrip` | unit | `ToolList` + `ToolDefinitionWire` |
| `e2e::e2e_list_sessions_returns_created_session` | integration | CreateSession → ListSessions 看到该 session |
| `e2e::e2e_resume_session_replays_event_log` | integration | CreateSession → Chat → ResumeSession → 接续 Chat，context 保留 |
| `e2e::e2e_tool_list_replaces_text_delta_hack` | integration | ListTools → 收到 `ToolList` 而非 `TextDelta` |
| `e2e::e2e_confirm_tool_call_approve_path` | integration | require_confirmation 匹配 → 收到 `ToolCallConfirmationRequired` → 发 Approve → 收到 `ToolResult` |
| `e2e::e2e_confirm_tool_call_reject_path` | integration | 同上但发 Reject → `ToolResult{is_error: true}` |
| `e2e::e2e_confirm_tool_call_timeout` | integration | 不发 ConfirmToolCall → 60s 后收到 `ToolResult{is_error, "confirmation timeout"}` |

> timeout 测试用 60s 太慢。`ReActEngine` 的 confirm 超时改用 `Duration::from_millis(confirm_timeout_ms)`，从 config 注入；测试用 200ms。生产默认 60000。

## 7. 实施顺序

1. 协议扩展 + roundtrip 测试
2. daemon：`ToolList`（替换 hack，最小改动）+ `ListSessions` + index.json schema 升级
3. daemon：`ResumeSession`（含 `create_session_with_context` 引擎 API）
4. 引擎 + daemon：`ConfirmToolCall` 流程（`ConfirmRouter` + `StreamEvent::ToolCallConfirmationRequired` + 超时）
5. CLI：clap 子命令重构 + 交互式确认提示
6. E2E 测试覆盖上述每条
7. 文档回填 ✅，标记 TUI 为 Phase 1.5b

## 8. Phase 1.5b — TUI 客户端（独立批次）

> 单独成批，因为 ratatui + crossterm 是一整套新代码，且依赖 Phase 1.5a 的协议扩展全部就绪。

布局（ratatui 三栏）：

```
┌────────────────────────────────────────────┐
│ Parrot TUI  | session: <uuid>  model: ...  │  ← 状态栏 (顶部)
├────────────────────────────────────────────┤
│ user: 帮我读 src/lib.rs                    │
│ assistant: 好的，我先看看 [tool: file_read] │
│   → src/lib.rs (3.2KB)                     │
│ assistant: 这个文件定义了...               │  ← 消息流 (滚动区)
│                                            │
├────────────────────────────────────────────┤
│ > _                                        │  ← 输入框 (底部，多行)
│ [Enter 发送 / Ctrl+C 退出 / Ctrl+R 滚动]   │
└────────────────────────────────────────────┘
```

事件循环：

```rust
// src/tui/main.rs
#[tokio::main]
async fn main() -> Result<()> {
    let conn = WsTransportClient::new().connect(..).await?;
    terminal_init()?;            // enter raw mode
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(64);
    tokio::spawn(async move { ws_to_ui(conn.receiver, ui_tx).await; });
    run_app(terminal, conn.sender, ui_rx).await
}
```

依赖：`ratatui`, `crossterm`。TUI 只依赖 protocol + transport，不依赖 core。`Cargo.toml` 新增 `[[bin]] name = "parrot-tui"`。

确认提示在 TUI 用模态浮层（输入框上方临时条 + y/n 快捷键），不阻塞渲染。

## 9. 与主设计文档的关系

本文档是 `2026-06-21-parrot-design.md` 的 Phase 1.5 细化补充。主文档的 §11 实施路线保留高层概览，本文档提供可实施细节。Phase 2 及以后如有需要同样拆分独立文档（`2026-xx-xx-parrot-phase-2.md`）。
