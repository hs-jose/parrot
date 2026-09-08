# 代码简洁性重构计划（阶段①死代码 + 阶段②去重 + 注释中文化）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除全部已验证的死代码，提取公共 helper 消除重复模式，并将本次涉及文件的注释统一为中文。

**Architecture:** 纯行为保持型重构（不改任何对外协议/序列化格式/语义）。每个任务结束时对应 crate 的测试必须全绿。执行顺序按依赖排列：core → daemon → providers → tools → hooks → cli → tui。

**Tech Stack:** Rust workspace（parrot-core / protocol / transport / config / providers / tools / hooks / daemon / cli / tui）。

## Global Constraints

- `parrot-core` 保持零 IO 约束（现状的 `EventLog`/`SessionManager` 文件操作不动）
- 错误跨 crate 用 `thiserror`；`anyhow` 仅允许出现在 binary
- 本次不修改 `parrot-protocol` 的消息类型（不动 roundtrip 语义）
- 不碰 `src/daemon/auth.rs`（token 文件 0600 语义保持）
- 每个任务收尾：`cargo test -p <crate>`；最后全量 `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
- **不执行 git commit**（用户未要求；全部改动留在工作区供审查）
- 被改文件中残留英文注释（`//` 与 `///`）一律译为中文；已有的中文注释不动
- 不新增任何功能、不重排无关代码

## 死代码清单（已用全仓 grep 验证 0 调用方）

| 位置 | 项 |
|---|---|
| `crates/parrot-core/src/session.rs:162` | `create_session_with_context` |
| `crates/parrot-core/src/session.rs:299` | `get_handle_mut` |
| `crates/parrot-core/src/session.rs:316` | `session_ids` |
| `crates/parrot-core/src/session.rs:331` | `is_healthy` |
| `crates/parrot-core/src/session.rs:341` | `remove` |
| `crates/parrot-core/src/tool.rs:13` | `ToolResult` 结构体 |
| `crates/parrot-core/src/tool.rs:50` | `impl Default for ToolRegistry` |
| `crates/parrot-core/src/lib.rs:22` | `ToolResult` 导出 |
| `crates/parrot-core/src/event_log.rs:27` | `current_seq()` 方法 |
| `crates/parrot-core/src/event_log.rs:153` | `write_snapshot` 降私有（仅 `maybe_snapshot` 调用） |
| `crates/parrot-core/src/engine.rs:24` | `MAX_REACT_ITERATIONS` 降私有（仅 :487 使用） |
| `crates/parrot-core/src/engine.rs:1109` | `system_prompt_hash` 降私有（0 外部调用） |
| `crates/parrot-core/src/engine.rs:1063` | `fire_and_drop` 未用的 `_tx` 参数 |
| `crates/parrot-daemon/src/session_store.rs:30` | `last_snapshot_seq` 字段（:109/:144 初始化一并删） |
| `crates/parrot-providers/src/anthropic.rs:48-63` | `#[allow(dead_code)]` 字段 + `AnthropicUsage` |
| `crates/parrot-providers/src/anthropic.rs:35` | `AnthropicMessage` 未用的 `Clone` |
| `crates/parrot-providers/src/retry.rs:23` | `is_retryable` 降私有（同文件测试内使用） |
| `crates/parrot-tools/src/web_fetch.rs:10` | `impl Default for WebFetchTool` |
| `src/tui/app.rs:69` | `cur_assistant_tools` 字段（:97/:259/:282/:367/:385 一并删） |
| `src/tui/input.rs:8` | 过期的 `#[allow(dead_code)]` |
| `crates/parrot-hooks/src/external.rs:65` | `ExternalHook::new` 未用的 `_global_timeout` 参数（`hooks/lib.rs:49` 调用点同步） |

---

### Task 1: parrot-core 死代码清理

**Files:**
- Modify: `crates/parrot-core/src/session.rs`, `tool.rs`, `lib.rs`, `event_log.rs`, `engine.rs`, `provider.rs`

- [ ] 按上表删除/降级所有条目
- [ ] `engine.rs::new()`（79-87 行）改为调用 `system_prompt_hash(p)` 消除重复哈希逻辑：
```rust
let system_prompt_hash = system_prompt.as_deref().map(system_prompt_hash).unwrap_or_default();
```
- [ ] `provider.rs:39` 注释删去对已删除 `ToolResult` 类型的引用（保留"为何不出现在此 enum"的语义说明）
- [ ] 将上述文件中残留英文注释译为中文
- [ ] 验证: `cargo test -p parrot-core` 通过

### Task 2: `ChatMessage` 构造函数 + 全仓替换

**Files:**
- Modify: `crates/parrot-core/src/types.rs`、`engine.rs`、`event_log.rs`、`compaction.rs`

**Interfaces (Produces):** `ChatMessage::new(role: ChatRole, content: impl Into<String>) -> ChatMessage`

- [ ] `types.rs` 添加：
```rust
impl ChatMessage {
    /// 构造不带工具元数据的消息；工具字段用结构体更新语法按需补充，
    /// 例如 `ChatMessage { tool_call_id: Some(id), ..ChatMessage::new(ChatRole::Tool, text) }`。
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }
    }
}
```
- [ ] 替换全部 5 字段字面量（struct update 语法）：
  - `engine.rs:166`(system) `:445`(user) `:506`(assistant+tool_calls) `:753` `:874` `:1039`(tool)
  - `event_log.rs:266`(user) `:287`(assistant+tool_calls) `:304`(tool) `:323`(summary user)
  - `compaction.rs:137` `user_msg` 改为 `ChatMessage::new(ChatRole::User, content)` 一行；`:149`(system)
  - `compaction.rs` 测试 `msg()` helper（:183）同步简化
- [ ] `event_log.rs:292`、`engine.rs:511` 的 `if tcs.is_empty() { None } else { Some(tcs) }` 改为 `(!tcs.is_empty()).then_some(tcs)`
- [ ] 验证: `cargo test -p parrot-core` 通过

### Task 3: engine.rs 去重 helpers

**Files:**
- Modify: `crates/parrot-core/src/engine.rs`, `compaction.rs`, `types.rs`

**Interfaces (Produces):**
- `async fn emit_and_persist(event_tx: &mpsc::Sender<AgentEvent>, event_log: &mut EventLog, event: AgentEvent)`
- `async fn finish_tool_call(session_id: Uuid, turn_id: Uuid, tool_call_id: &str, tool_name: &str, result: ToolOutput, event_tx: &..., event_log: &mut EventLog, context: &mut Vec<ChatMessage>)`（吸收 `emit_aborted_tool_end`）
- `compaction::wrap_summary(summary: &str) -> String`
- `impl From<parrot_protocol::agent_event::ToolCallInfo> for crate::types::ToolCallInfo`

- [ ] 添加 `emit_and_persist`（先发送再落盘，落盘失败 warn）：
```rust
async fn emit_and_persist(
    event_tx: &mpsc::Sender<AgentEvent>,
    event_log: &mut EventLog,
    event: AgentEvent,
) {
    let _ = event_tx.send(event.clone()).await.ok();
    if let Err(e) = event_log.append(event) {
        tracing::warn!(error = ?e, "event persistence failed");
    }
}
```
- [ ] 替换 12 处"send+append"双行：engine.rs 190-193、215-218、266-267、277-289、324-325、424-427、580-581、685-686、722-723、751-752、871-872、1037-1038。**注意**：`fire_and_drop` 内 1084-1087 是"先落盘再发送"（注释明确说明 resume 依赖此顺序），**不改**；`maybe_compact` 内 387-393 的 `CompactionStart` 只发不落盘，**不改**。
- [ ] 添加 `finish_tool_call` 统一 3 处 ToolEnd 收尾（hook-blocked 745-759 / 正常 865-880 / `emit_aborted_tool_end` 1018-1046），删除原 `emit_aborted_tool_end`，aborted 调用点传入固定 `ToolOutput{content:"aborted before execution", is_error:true}`
- [ ] `compaction.rs` 添加 `pub fn wrap_summary(summary: &str) -> String { format!("{SUMMARY_MARKER}\n{summary}") }`；`engine.rs:420` 与 `compaction.rs:167` 改用
- [ ] `types.rs` 添加 `From<parrot_protocol::agent_event::ToolCallInfo>`（`tool_call_id→id, tool_name→name, arguments→arguments`）；`engine.rs:497-505`、`event_log.rs:279-286` 改用 `.map(CoreToolCallInfo::from)`
- [ ] `context_budget`（:30-35）简化为 `max_history_tokens.min(model_window.unwrap_or(u32::MAX))`
- [ ] 验证: `cargo test -p parrot-core` 通过

### Task 4: session.rs 合并 spawn 路径

**Files:**
- Modify: `crates/parrot-core/src/session.rs:123-293`

- [ ] `spawn_session` 增加 resume 参数，合并 `create_resumed_session` 与 `create_session` 两条路径：
```rust
async fn spawn_session(
    &mut self,
    id: Uuid,
    gen_config: GenerateConfig,
    system_prompt: Option<String>,
    initial_context: Vec<ChatMessage>,
    resume: Option<(u64, Option<parrot_protocol::agent_event::IntegrityIssue>)>,
) -> Result<(), crate::error::AgentError>
```
内部：`with_initial_context(initial_context)` 之后，`if let Some((seq, warning)) = resume { engine = engine.with_resumed_from(seq); if let Some(w) = warning { engine = engine.with_pending_integrity_warning(w); } }`
- [ ] `create_resumed_session` 收缩为参数转发；`create_session` 传 `None`
- [ ] 将本文件英文注释译为中文（`default_system_prompt`/`ConfirmConfig`/`SessionManager` 字段等）
- [ ] 验证: `cargo test -p parrot-core` 通过

### Task 5: daemon（session_store.rs + runtime.rs）

**Files:**
- Modify: `crates/parrot-daemon/src/session_store.rs`、`runtime.rs`

- [ ] `session_store.rs`：删 `last_snapshot_seq` 字段及两处初始化；读文件确认 serde 属性后删 `system_prompt` 字段（runtime.rs:628 已声明不读回）；提取 `fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()>`（tmp 写 + rename）合并 171-177 与 213-217 两处，删除 `atomic_rename` 薄包装；`update_meta` 的 fallback `SessionMeta`（100-110）与 `init_session`（135-145）提取共享构造
- [ ] `runtime.rs`：提取 `fn send_error(sender: &..., session_id, code, message)` 收敛约 10 处 Error 样板；提取 `send_session_cmd` 合并 Chat(322-347)/Abort(348-370) 同构臂；`create_dir_all` 三重创建（72-77）收敛为一处；`read_history`(587-604)/`resume_session`(632-645) 共享"meta 存在性检查 + 目录构造"；`model_for_meta`（263-283）去掉恒为 `Some` 的 Option 包装；`tokio::spawn` relay 任务 `&mut event_rx` 改按值（300, 487）；`ConnectionContext` 单字段结构体视上下文改局部变量；`shutdown_all` 的 `match timeout { Ok(_)=>{}, Err(_)=>{..} }` 改 `if let Err(_)`
- [ ] 将两文件英文注释译为中文
- [ ] 验证: `cargo test -p parrot-daemon` 通过

### Task 6: providers（anthropic.rs + retry.rs）

**Files:**
- Modify: `crates/parrot-providers/src/anthropic.rs`、`retry.rs`

- [ ] 删 `#[allow(dead_code)]` 的 `stop_reason`/`usage` 字段与 `AnthropicUsage` 结构体；删 `AnthropicMessage` 的 `Clone` derive
- [ ] `chat`(288-297) 与 `chat_stream`(323-332) 提取 `fn build_request(..., stream: Option<bool>) -> AnthropicRequest`
- [ ] `parse_stop_reason`(379-381) 单行包装内联；`extract_system_prompt`(164-170) 的 `reduce` 改 `join("\n")`；`collect_models`(557-580) 用 `filter_map` 压平
- [ ] `retry.rs`：`is_retryable` 去掉 `pub`（同文件测试可访问）
- [ ] 将两文件英文注释译为中文
- [ ] 验证: `cargo test -p parrot-providers` 通过

### Task 7: tools crate

**Files:**
- Modify: `crates/parrot-tools/src/`（lib.rs、shell_exec.rs、file_grep.rs、file_glob.rs、file_read.rs、file_write.rs、web_fetch.rs）

**Interfaces (Produces):**
- `fn resolve_arg_path(p: &str, working_dir: &Path) -> PathBuf`
- `fn str_arg<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, AgentError>`

- [ ] 5 个工具 `new(_working_dir, _max_file_size)` 参数全部删除 → 无参 `new()`；`lib.rs::register_all` 同步简化
- [ ] `lib.rs` 添加上述两个 `pub(crate)` helper；替换 5 处路径解析 + 7 处取参样板
- [ ] `shell_exec.rs`：提取 `spawn_shell` + 输出拼装公共函数，`ShellExecTool::call`(63-114) 与 `run_shell_command`(132-176) 复用
- [ ] `file_grep.rs:154`：`glob::Pattern` 提升到搜索循环外编译一次；`file_glob.rs:66-89` 双层 match 用 `for entry in paths.flatten()` 简化
- [ ] `web_fetch.rs` 删 `impl Default`
- [ ] 将各文件英文注释译为中文；顺带把 `web_fetch.rs:82` 的 `&content[..100_000]` 截断改为 `external.rs:193` 的 `is_char_boundary` 安全写法（同文件 helper）
- [ ] 验证: `cargo test -p parrot-tools` 通过

### Task 8: hooks crate

**Files:**
- Modify: `crates/parrot-hooks/src/lib.rs`、`external.rs`、`shell_denylist.rs`、`dangerous_command_blocker.rs`、`redact_secrets.rs`

- [ ] `lib.rs` 提取 `pub(crate) fn extract_shell_command(event: &HookEvent) -> Option<&str>`（ToolCall + 工具名白名单 `shell_exec|bash|shell` + `command` 参数三层判定），`shell_denylist.rs:64-72`、`dangerous_command_blocker.rs:44-52` 复用
- [ ] `shell_denylist.rs:42-47` 改 `.iter().any(...)`
- [ ] `redact_secrets.rs:68-93` 内置 pattern 与 extra_patterns 两循环统一为一个 `(Regex, 替换闭包)` 迭代
- [ ] `external.rs`：`new` 去掉 `_global_timeout`（`lib.rs:49` 调用点同步）；`resolve_timeout`(78-80)、`toml_to_json`(222-224) 单行包装内联
- [ ] 将各文件英文注释译为中文
- [ ] 验证: `cargo test -p parrot-hooks` 通过

### Task 9: CLI（main.rs + conn.rs + daemon.rs）

**Files:**
- Modify: `src/cli/conn.rs`、`src/cli/main.rs`、`src/cli/daemon.rs`

**Interfaces (Produces):**
- `conn.rs`: `async fn open_conn(cli: &Cli, config: &Config) -> Result<Connection, String>`（含 token 路径解析 + connect + wait_hello + "Connected" 提示）
- `conn.rs`: `async fn expect_msg<T>(receiver: &mut mpsc::Receiver<ServerMessage>, extract: impl Fn(ServerMessage) -> Option<T>, err: &str) -> Result<T, String>`（循环滤掉意外消息；Error 消息直接透传为 Err；连接关闭报错）

- [ ] 5 处 recv 循环（main.rs:344/368/407/435/463）+ conn.rs 3 个 `wait_*` 统一走 `expect_msg`
- [ ] 4 处 token 路径解析 + 7 处握手样板收敛进 `open_conn`
- [ ] `daemon.rs:82-94` 内部 async 块去掉不可达的 `Err` 分支；`kill()`(18-23) 复用单次锁 guard
- [ ] 将三文件英文注释译为中文
- [ ] 验证: `cargo build -p parrot` 通过（CLI 无独立测试，由 e2e 覆盖）

### Task 10: TUI

**Files:**
- Modify: `src/tui/app.rs`、`mod.rs`、`confirm.rs`、`input.rs`

- [ ] `app.rs` 删 `cur_assistant_tools` 字段及全部 5 处操作（69/97/259/282/367/385；367 行的 `let _tools = ...remove()` 直接删除）
- [ ] `mod.rs:101-105` teardown 与 `RawModeGuard::drop`（29-39）去重——run_tui 结尾交给 guard 统一清理（保留 guard 的作用域覆盖整个 TUI 生命周期）
- [ ] `confirm.rs:15-20` 截断逻辑复用 `ui.rs::truncate_str`（按需 `pub(crate)`）
- [ ] `input.rs:32-39` 三处 `match blocking_send { Ok(())=>{} Err(_)=>break }` 改 `if ...is_err() { break }`；删 `:8` 过期 `#[allow(dead_code)]`
- [ ] 将各文件英文注释译为中文
- [ ] 验证: `cargo test -p parrot` 通过（含 replay_test）

### Task 11: 全量验证

- [ ] `cargo build --workspace`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] 逐项修复直至全绿
