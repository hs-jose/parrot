# Parrot 生命周期 Hook 扩展点设计

> 对应 idea 备忘录 #17 的"可扩展性"分支：参考 pi 的生命周期设计补充关键节点的 hook 能力。
> 范围只覆盖 hook 扩展点本身，不在本轮重构 ReAct 流程为编排框架（idea #17 下半部分留作后续）。

## 1. 背景与目标

Parrot 已有清晰的三层生命周期事件（`AgentStart/TurnStart/MessageStart+MessageEnd/ToolStart+ToolEnd/TurnEnd/AgentEnd`），但只是**单向广播**给 client + 落盘，没有任何"回灌到流程"的能力。`ConfirmConfig` 是硬编码的工具门控雏形，与"hook 系统"属同一类需求。

本设计为引擎引入**生命周期扩展点（hook point）**：让 daemon 内置 / 第三方实现能观察、修改、阻断 Agent 流程中的关键节点，而不必改 engine 主干。

### 调研要点

- **Pi Agent（Rust, `Dicklesworthstone/pi_agent_rust`）**：`src/extension_events.rs` 定义 `ExtensionEvent` tagged enum（`startup/agent_start/agent_end/turn_start/turn_end/tool_call/tool_result/session_before_*/input`），序列化 JSON 派发到 JS 扩展（内嵌 QuickJS），反序列化得到类型化 outcome。`tests/ext_conformance/reports/lifecycle_hooks/lifecycle_hook_parity_matrix.json` 把 hook 模式归为 `fire_and_forget / transform / first_result / pre_tool / post_tool / cancellable`。
- **OpenCode**：JS/TS 插件通过具名 hook 字符串挂载（`tool.execute.before/after`、`shell.env`、`experimental.session.compacting`、`session.*` / `message.*` / `file.*` 事件），通过 `output` 对象副作用改写参数。
- 两者都把"hook 名"做成有模式的字符串，hook 实现以嵌入脚本为载体。Parrot 不走嵌入脚本这一步（首轮），详见 §3。

### 与差异化定位的关系

Parrot 的差异化优势是"Rust 原生、启动快、trait-based DI、core zero-IO、daemon 唯一 IO 边界"。Hook 系统的设计必须延续这条线，未来嵌入脚本运行时只是其上一层的可选适配器（写一个 `impl Hook for ScriptHook` 即可，不污染核心 trait）。

## 2. 设计原则

1. **核心 trait 化、脚本后置**：本轮 Hook 载体是 Rust `trait Hook`，daemon 编译期注册实现。脚本运行时（rhai/quickjs）作为后续可选 layer。
2. **零 IO 边界不变**：`parrot-core` 不引入 IO，`Hook` trait 自带 IO 能力（hook 实现本就属 daemon 层）。
3. **微创**：不改 `engine.rs` 主干 `run()/handle_turn()/stream_llm_message()` 控制流骨架，只在生命周期边界插 dispatch 调用。
4. **三策略对应命名**：`fire-and-forget` / `waterfall` / `bail`，每个 hook point 绑死一种策略，避免实现者自由发挥引起歧义。
5. **Resume 不重放历史执行**：Resume 从 `events.log` 重建 message 状态后开新 Agent Loop，不再跑一遍历史。Hook 副作用**不进 `events.log`**，受 hook 影响的最终状态（如 `ToolEnd.result` 已是改后内容、`turn_start` 注入的 messages 在首次 snapshot 落盘）自动通过现有的持久化路径复现。

## 3. Hook 载体选择

本轮选择**混合：trait 为主 + 脚本可选**。具体：

- 第一阶段：所有 hook 是 Rust 实现 `Hook` trait，daemon 启动时由 `enabled` 配置项决定注册哪几个到 `HookRegistry`。
- 后续阶段：可选嵌入脚本运行时（rhai 优先 — 纯 Rust、零依赖、与 Parrot 定位一致），写一个 `ScriptHook` 实现 `Hook` trait，把 Rust hook 调用转译成脚本执行。本轮 trait 形状对此适配友好（见 §5）。

不在本轮范围（详见 §11）：脚本运行时本身、`input` 用户输入改写、`session_before_*` cancellable 系列、ReAct 流程重构成 Phase 编排框架。

## 4. 整体架构

```text
                          ┌────────────────────────────────────────────┐
                          │       parrot-core/src/hooks.rs             │
                          │  ┌──────────────────────────────────────┐   │
   engine 调用 ─────────▶ │  │ HookRegistry                         │   │
                          │  │  - Vec<Arc<dyn Hook>> (注册顺序)     │   │
                          │  │  - on_<point>() helper（三策略）    │   │
                          │  └──────────────────────────────────────┘   │
                          │  + Hook / HookEvent / HookOutcome / HookCtx │
                          └────────────────────────────────────────────┘
                                            ▲ impl
                          ┌─────────────────┴───────────────────────────┐
                          │ crates/parrot-daemon/src/hooks/             │
                          │  - dangerous_command_blocker.rs             │
                          │  - redact_secrets.rs                        │
                          │  - mod.rs: build_registry(enabled list)     │
                          └─────────────────────────────────────────────┘
                                            ▲ used by
                          ┌─────────────────┴───────────────────────────┐
                          │ crates/parrot-daemon/src/runtime.rs         │
                          │  (build HookRegistry, inject into manager) │
                          └─────────────────────────────────────────────┘
```

## 5. Hook trait 与核心类型

### 5.1 `Hook` trait

`单一 Hook trait + enum dispatch`（按 §3 用户决策）。注册：`Vec<Arc<dyn Hook>>` 维持插入顺序；`supported()` 返回 bitmask 让 registry 提前 skip 不关心该点的 handler，避免无效 dispatch。

```rust
// crates/parrot-core/src/hooks.rs
use crate::error::AgentError;
use crate::types::ChatMessage;
use parrot_protocol::agent_event::AgentEvent;
use parrot_protocol::types::ToolOutput;
use uuid::Uuid;

bitflags::bitflags! {
    /// 标记一个 Hook 实现关心哪些扩展点。
    /// Registry 在某个点 dispatch 时跳过未声明此点的 handler，避免无效 await。
    #[derive(Debug, Clone, Copy, Default)]
    pub struct HookPoints: u8 {
        const AGENT_START            = 0b0000_0001;
        const AGENT_END              = 0b0000_0010;
        const TURN_START             = 0b0000_0100;
        const TOOL_CALL              = 0b0000_1000;
        const TOOL_EXECUTION_START   = 0b0001_0000;
        const TOOL_RESULT            = 0b0010_0000;
    }
}

#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// 唯一 id，用于 parrot.toml 的 `enabled` 引用 + `HookFired.hook_id`
    fn id(&self) -> &'static str;

    /// bitmask，registry 用它过滤
    fn supported(&self) -> HookPoints;

    /// 所有扩展点都走这个单方法，参数是 enum
    async fn dispatch(
        &self,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookOutcome, AgentError>;
}
```

### 5.2 `HookEvent` 与 `HookOutcome`

跟现有 `AgentEvent` / `MessageDeltaPayload` 同风格：`#[serde(tag = "type")] snake_case`。每个变体字段就是该点能"看见+改写"的数据。

```rust
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEvent<'a> {
    AgentStart { session_id: Uuid, model: &'a str, provider: &'a str },
    AgentEnd   { session_id: Uuid },
    TurnStart  { session_id: Uuid, turn_id: Uuid, user_message: &'a str },
    ToolCall   {
        session_id: Uuid,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a serde_json::Value,
    },
    ToolExecutionStart {
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a serde_json::Value,
    },
    ToolResult {
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        input: &'a serde_json::Value,
        result: &'a ToolOutput,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookOutcome {
    /// fire-and-forget / waterfall 中无修改时的中性结果
    NoOp,
    /// turn_start 专用：累加进 context（不影响 assistant/user 消息，仅作 system-style 注入）
    /// messages 在多 handler 间累加（waterfall）
    InjectMessages { messages: Vec<ChatMessage> },
    /// bail 点会用：阻止后续执行
    Block { reason: String },
    /// tool_result 专用：替换 ToolOutput.content，last-wins
    ReplaceResult { content: String, is_error: bool },
}

/// 给 hook 实现的上下文，含 daemon 注入的非业务数据
pub struct HookCtx<'a> {
    pub session_id: Uuid,
    pub working_dir: &'a std::path::Path,
    /// hook 自身超时，daemon 在调用前已包了 tokio::time::timeout，这里仅供 hook 实现参考
    pub timeout: std::time::Duration,
}
```

`HookOutcome` 公开序列化字段直接进 `HookFired` wire 事件的 `summary`，方便客户端 UI 渲染。

### 5.3 `HookRegistry`

```rust
pub struct HookRegistry {
    handlers: Vec<Arc<dyn Hook>>,
    timeout: std::time::Duration,
}

impl HookRegistry {
    pub fn new(timeout: std::time::Duration) -> Self { ... }
    pub fn register(&mut self, hook: Arc<dyn Hook>) { ... }

    /// 6 个 helper，按 §6 的策略实现，错误/超时按 §8 处理。
    /// 每个 helper 内部 already filter handlers by HookPoints mask。
    pub async fn on_agent_start(&self, ...) { /* fire-and-forget */ }
    pub async fn on_agent_end(&self, ...)   { /* fire-and-forget */ }
    pub async fn on_turn_start(&self, ...) -> TurnStartOutcome { /* waterfall + bail on Block */ }
    pub async fn on_tool_call(&self, ...) -> ToolCallOutcome   { /* bail */ }
    pub async fn on_tool_execution_start(&self, ...)            { /* fire-and-forget */ }
    pub async fn on_tool_result(&self, ...) -> ToolResultOutcome { /* waterfall */ }
}
```

`TurnStartOutcome` / `ToolCallOutcome` / `ToolResultOutcome` 是 engine 用的简化 enum（不是 wire 类型），表达"经过 hook 链后的最终决策"。

## 6. 六个扩展点

按 engine.rs 现有生命周期边界接入：

| # | Hook Point | 接入位置 (engine.rs) | 策略 | Allowed Outcome | 短路行为 |
|---|---|---|---|---|---|
| 1 | `agent_start` | `run()`：emit `AgentEvent::AgentStart` 之后、主循环之前 | fire-and-forget | `NoOp` | — |
| 2 | `agent_end` | `AgentEndGuard::fire_and_drop`：emit `AgentEvent::AgentEnd` 之前 | fire-and-forget | `NoOp` | — |
| 3 | `turn_start` | `run()` 收到 `SessionCmd::Chat{message}` 之后、`emit TurnStart` 之前 | waterfall + bail-on-Block | `InjectMessages` / `Block{reason}` / `NoOp` | `Block` → 跳过整 turn，emit `TurnEnd{stop_reason: BlockedHook(reason)}`，不入 `MessageStart` |
| 4 | `tool_call` | `run_one_tool`：emit `ToolStart` 之后、`await_confirmation` 之前 | bail | `Block{reason}` / `NoOp` | 任一 handler 返回 Block 立即结束，向 context 里 push 一条 `Tool{content="blocked: <reason>", is_error=true}` 消息复用 `ConfirmDecision::Reject` 现有路径 |
| 5 | `tool_execution_start` | 确认通过之后、`execute_tool()` 之前 | fire-and-forget | `NoOp` | — （埋点：UI 可显示"工具已批准开始执行"） |
| 6 | `tool_result` | `execute_tool()` 返回 / `Block` 走 reject 路径之后、`emit ToolEnd` 之前 | waterfall | `ReplaceResult{content, is_error}` / `NoOp` | last-wins 替换；最终 ToolEnd.result 落盘的是替换后的内容 |

### 与 `ConfirmConfig` 的关系

`tool_call` hook 在 confirm 之前；hook `Block` 即短路不进 confirm。两者并行：hook 是"前置阻断层"（hook 拦掉的就不阻塞用户），confirm 是"用户确认层"（hook 通过后仍可能需要用户点 Approve）。这样原本硬编码的 ConfirmConfig 不必删除，hook 是叠加的二级可扩展机制。

## 7. 三种执行策略

- **fire-and-forget** (`agent_start`, `agent_end`, `tool_execution_start`)
  - 纯通知，hook 返回 outcome 被忽略（但因 §8 的可观测性，仍会触发 `HookFired` wire 事件）
  - 适用于埋点、UI 更新、`tracing::info!`、外部 metric push（不过这些 IO 是 hook 自己的事）
  - 多 handler 全部执行，无短路

- **waterfall** (`turn_start` 的 InjectMessages, `tool_result` 的 ReplaceResult)
  - 多 handler 链式执行：handler[i] 的输出作为 handler[i+1] 的输入
  - `turn_start`：handler 返回 `InjectMessages{messages}`，registry 把这些 messages append 到内部累加器（不替换 `user_message` 本身，因为是 system-style 注入）；handler 自身 Also 收到上家累加后的 events（实际实现上 `HookEvent::TurnStart.user_message` 还是原值，累加后的 messages 不回灌给下家，否则会重复）
  - **简化决定 1**：InjectMessages 实现仅"累加到引擎入参 context"，不传递给下游 handler；下游看到的是原 user_message。这样不会因顺序导致累加倍增。
    - **简化决定 2（注入位置）**：注入在 `handle_turn()` 第一步 push `ChatMessage{role=User, content=user_msg}` 之前。实际 context 顺序变为 `[...历史, 注入 messages, 新 user_msg]`，让模型看到"hook 额外语境"再看本 turn 的用户问题。注入消息不限定 role（但典型用法是 `System` 或 `User` 标注的"上下文提示"），由 hook 实现者决定。
  - `tool_result`：上一个 handler 的 `ReplaceResult` 后，下一个 handler 的 `HookEvent::ToolResult.result` 看到的是替换后的 result（registry 内部维护"滚动 result"，每 handler 后更新）
  - 不短路

- **bail** (`tool_call`，以及 `turn_start` 内的 `Block` outcome 嵌入)
  - 任一 handler 返回 `Block{reason}` → 立即结束此链
  - 后续未跑的 handler 不执行（避免被 block 后又有副作用）
  - registry 收集所有已收到的 reason 合并展示（log warn 多个 reason）

## 8. 执行与失败语义

### 8.1 超时

- 每个 dispatch 调用包一层 `tokio::time::timeout(timeout, hook.dispatch(...))`
- `timeout` 来自 `HooksConfig.timeout`，默认 5s
- **超时**：tracing::warn、fail-open（视作 `NoOp`）、不再参与 chain、仍 emit `HookFired{outcome_kind: "timeout"}`
- 同一 hook 在 bail 点超时：fail-open 使其不产生 `Block`，但允许短路上游已有的 `Block`（已有的 Block 不会因下游超时被擦除）

### 8.2 Error

- hook `dispatch` 返回 `Err(AgentError)`：log::warn、emit `HookFired{outcome_kind: "error", summary: Some(err.to_string())}`、视作 NoOp
- 不中断 agent 主流程（仅观察/告知客户端），不允许 hook 通过返回 Err 来强行中止 turn；想中止必须显式返回 `Block`
- bail 点：下游已 Block 不会被擦除，但当前出错 handler 的 NoOp 处理会继续 chain

### 8.3 多 handler 顺序

- 按 `runtime.rs` 启动时构造的 `HookRegistry` 中 `handlers` Vec 顺序执行（daemon 读 `enabled` 配置顺序构造）
- 让 `enabled` 顺序即执行顺序，文档中说明这点，用户可按需调序

### 8.4 空注册表快速跳过

`HookRegistry::on_<point>()` 实现第一步都判 `if self.handlers.iter().all(|h| !h.supported().contains(point)) { return default; }`，零注册或无关心此点的 handler 不产生 async work、不 emit `HookFired`。

## 9. Wire protocol & 持久化

### 9.1 新增 `AgentEvent::HookFired`

```rust
// crates/parrot-protocol/src/agent_event.rs
pub enum AgentEvent {
    // ... 现有变体
    HookFired {
        session_id: SessionId,
        hook_id: String,         // Hook::id()
        event_kind: String,      // "tool_call" / "turn_start" / ...
        outcome_kind: String,    // "noop" / "block" / "inject_messages" / "replace_result" / "timeout" / "error"
        #[serde(default)]
        summary: Option<String>,
    },
}
```

`is_persistent()` 加入新 variant 后返回 `false`（与 `MessageDelta` / `ToolUpdate` 同列非持久化）。

> **Wire 兼容性**：Parrot 的 client/server 同 crate 同版本无跨版本部署场景，新增 `AgentEvent` variant 不需要在 wire 上保留后向兼容；旧客户端遇到 `HookFired` tag 会反序列化失败但实际不会发生（同 workspace 同编译同发布）。

### 9.2 Relay 路径

`relay_session_events`（`crates/parrot-daemon/src/runtime.rs:450`）已是 `ServerMessage::AgentEvent { event }` 透传任意 `AgentEvent`，新增 `HookFired` 自动穿过，**不需修改 relay**。`ServerMessage` 不需要新增 variant。

### 9.3 Resume 行为

按用户给出的 resume 定义："从持久化的会话文件中加载历史消息状态，然后继续执行新的 Agent Loop，而非把历史执行过程重新跑一遍"。设计 implications：

- `HookFired` **不落盘**（`is_persistent() == false`），不会进入 `events.log`，resume 时根本看不到它。
- `tool_result` hook 替换后的 `ToolEnd.result.content` 已是替换后内容，落盘到 `events.log`；resume 重建 context 时直接拿最终 Tool 消息，自动复现 post-hook 状态。
- `turn_start` hook 注入的 messages 在 `event_log.maybe_snapshot(context)` 落盘到 snapshot 时已成为 context 一部分；resume 读 snapshot 重建 context 自动包含。
- `tool_call` hook `Block` 后 context 中 push 的 `Tool{content="blocked: ..."}` 会随 `MessageEnd` 之后 snapshot 同步到磁盘。
- 整个 Resume 过程无需 replay hook，无需"反推 hook 执行"。

### 9.4 协议测试

根据 AGENTS.md 约定，更新 `crates/parrot-protocol/tests/roundtrip.rs` 增加 `HookFired` roundtrip case。

## 10. parrot.toml 配置

```toml
[hooks]
enabled = ["dangerous_command_blocker", "redact_secrets"]
timeout_seconds = 5
```

```rust
// crates/parrot-config/src/config.rs
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_timeout() -> u64 { 5 }
```

`AppConfig` 增 `pub hooks: HooksConfig`（`#[serde(default)]` 保证旧配置兼容），`default_config()` 里 `hooks: HooksConfig::default()`。

注入路径（沿用现有 `with_confirm_config` 风格）：

1. `runtime.rs::run_with` 从 `config.hooks` 调 `daemon::hooks::build_registry(&config.hooks)` 得 `Arc<HookRegistry>`
2. `SessionManager::new(...).with_hooks(registry)`（新方法，镜像 `with_confirm_config`）
3. `SessionManager::spawn_session` / `create_resumed_session` 把 `Arc<HookRegistry>` clone 进 `ReActEngine::new(...).with_hooks(registry.clone())`
4. `ReActEngine::with_hooks(mut self, r: Arc<HookRegistry>) -> Self`

## 11. daemon 内置预设 hook（首轮 2 个）

放在 `crates/parrot-daemon/src/hooks/`：

- `dangerous_command_blocker.rs`：`tool_call` hook，对工具名 `shell_exec`/`bash`/`shell` 的 `arguments.command` 做正则黑名单匹配（教学示例 pattern：`rm\s+-rf\s+/`、`/\b(sh|bash|zsh)\s+-c.*\|\s*(sh|bash|zsh)\b/` 即 reverse shell 形态、`chmod\s+777\s+/`、`:\(\)\{\s*:\|:&\s*\};:` 即 fork bomb）。命中返回 `Block{reason: "dangerous command: <pattern>"}`。与现有 `ToolsConfig.sandbox.denylist` 互补（hook 是二级 cmd-pattern 防御层，工具层 denylist 是一级"工具内白名单"）。
- `redact_secrets.rs`：`tool_result` hook，对工具名 `read`/`shell_exec` 的 `result.content` 做 secret pattern 替换为 `[REDACTED]`（pattern：AKIA/`ghp_...`/`sk-ant-...` 之类的 well-known key prefix）。返回 `ReplaceResult{content: redacted, is_error: false}`（不改 is_error）。展示 transform 能力。

`hooks/mod.rs` 提供 `pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry>`：按 `cfg.enabled` 列表选择内置实现，按列表顺序注册。

未匹配的 id → warn log + 跳过（不构成硬错误，方便用户配置错 typo 不至于启不来 daemon）。

## 12. Crate 落点

| 位置 | 新增内容 |
|---|---|
| `crates/parrot-core/src/hooks.rs` | `Hook` trait / `HookRegistry` / `HookEvent` / `HookOutcome` / `HookCtx` / `HookPoints` bitflags / outcome enums |
| `crates/parrot-core/src/lib.rs` | `pub mod hooks; pub use hooks::{Hook, HookRegistry, ...};` |
| `crates/parrot-core/src/engine.rs` | `ReActEngine` 加 `hooks: Arc<HookRegistry>` 字段 + `with_hooks` builder；`run/handle_turn/run_one_tool` 插 6 个 dispatch 点；新增 `TurnStopReason::BlockedHook(String)` 到 parrot-protocol |
| `crates/parrot-core/src/session.rs` | `SessionManager` 加 `hooks: Arc<HookRegistry>` + `with_hooks` + 在 `spawn_session`/`create_resumed_session` 传给 engine |
| `crates/parrot-protocol/src/agent_event.rs` | `AgentEvent::HookFired` + `TurnStopReason::BlockedHook(String)` |
| `crates/parrot-protocol/tests/roundtrip.rs` | `HookFired` / `BlockedHook` roundtrip case |
| `crates/parrot-config/src/config.rs` | `HooksConfig` + `AppConfig.hooks` + default |
| `crates/parrot-daemon/src/lib.rs` | `pub mod hooks;` |
| `crates/parrot-daemon/src/hooks/mod.rs` | `build_registry(&HooksConfig) -> Arc<HookRegistry>` |
| `crates/parrot-daemon/src/hooks/dangerous_command_blocker.rs` | impl Hook |
| `crates/parrot-daemon/src/hooks/redact_secrets.rs` | impl Hook |
| `crates/parrot-daemon/src/runtime.rs` | 在 `run_with` 构造 `HookRegistry` 并注入 `SessionManager` |

`bitflags` 和 `async_trait` parrot-core 已是依赖，直接复用。

## 13. engine 改动细节（关键摘录）

只列 hook dispatch 插入点，不改控制流：

```rust
// run() AgentStart 之后
let _ = event_log.append(agent_start.clone());
self.hooks.on_agent_start(&ctx, &agent_start).await;
// ... 主循环 ...

// Chat 收到后
match cmd {
    Some(SessionCmd::Chat { message }) => {
        let turn_outcome = self.hooks.on_turn_start(&ctx, session_id, &message).await;
        match turn_outcome {
            TurnStartOutcome::Blocked(reason) => {
                // 不 emit TurnStart，直接 emit TurnEnd{BlockedHook}，跳过 handle_turn
                let _ = event_tx.send(AgentEvent::TurnEnd {
                    session_id, turn_id,
                    stop_reason: TurnStopReason::BlockedHook(reason),
                    usage: Usage::default(),
                }).await.ok();
                continue;
            }
            TurnStartOutcome::Continue { injected_messages } => {
                context.extend(injected_messages);
            }
        }
        // emit TurnStart (原代码)
        // handle_turn (原代码)
    }
    ...
}

// run_one_tool() ToolStart 之后、confirm 之前
let tool_outcome = self.hooks.on_tool_call(&ctx, tool_call_id, tool_name, args).await;
if let ToolCallOutcome::Blocked(reason) = tool_outcome {
    // 复用 run_one_tool 末尾的 reject 路径：push Tool msg + emit ToolEnd + return Ok
    // 等价于 confirm reject，但用 reason 而非 "user rejected"
    return reject_tool(/* reason */);
}
// confirm (原代码)
// confirm 通过后
self.hooks.on_tool_execution_start(&ctx, tool_call_id, tool_name, args).await;
let result = match race_with_abort(cmd_rx, self.execute_tool(...)).await { /* 原代码 */ };
let mod_result = self.hooks.on_tool_result(&ctx, tool_call_id, tool_name, &result).await.apply(result);
// emit ToolEnd(mod_result)
```

`agent_end` 在 `AgentEndGuard::fire_and_drop` 里：

```rust
async fn fire_and_drop(mut self, _tx, event_log, ctx) {
    self.hooks.on_agent_end(&ctx, self.session_id).await;
    // 原落盘 AgentEnd + 发 wire 的逻辑
}
```

注意 `AgentEndGuard` 目前不持有 `hooks`，需把 `Arc<HookRegistry>` clone 进 guard（drop 时不需要，只在显式 `fire_and_drop` 调 hook；guard 的 `Drop` 路径仍 try_send 不 await hook，符合现有"Drop 不能 await"约束）。

## 14. 测试

### 14.1 单元（crates/parrot-core/tests/hooks_test.rs，独立集成测试）

造 `RecordingHook`（用 `Mutex<Vec<RecordedEvent>>` 捕获调用顺序 + 可注入返回 outcome）覆盖：

- `tool_call` hook 返回 `Block`：`RecordingHook::on_tool_call` 记录；调用方断言工具真正 `execute_tool` 未被调；context 里 `Tool{content="blocked: ..."}` 进去；`ToolEnd.result.is_error == true`；wire 收到 `HookFired{outcome_kind: "block"}`
- `tool_result` 多 handler（两个 `RecordingHook` 都返回 `ReplaceResult`）：链式 last-wins，最后 ToolEnd 是第二个的内容；前者 `HookEvent::ToolResult.result` 是原始、后者是前者 replaced 后的值
- `turn_start` 返回 `InjectMessages`：context 在 `TurnStart` 之后多了注入消息；handle_turn 被调
- `turn_start` 返回 `Block(reason)`：handle_turn 不被调，TurnEnd{BlockedHook}
- `agent_start` / `agent_end` / `tool_execution_start` fire-and-forget：handler 被调但 outcome 忽略
- timeout 模拟：rotein hook dispatch future sleep 10s（配置 100ms）→ outcome 视作 NoOp，HookFired{outcome_kind="timeout"} 仍发，agent 继续
- error 模拟：hook 返回 `Err(AgentError::...)` → 同上 NoOp + HookFired{outcome_kind="error"}
- 空注册表：所有 dispatch helper 走快路径不产生 HookFired

### 14.2 协议（crates/parrot-protocol/tests/roundtrip.rs）

`hook_fired_roundtrip`：序列化 `AgentEvent::HookFired{...}` 并比对字段。
`turn_stop_reason_blocked_hook_roundtrip`：同。

### 14.3 端到端（tests/integration/e2e_test.rs 已有 mock provider 框架）

`e2e_hook_blocks_tool_call`：用 mock provider 让模型返回一个 tool_call；configure `dangerous_command_blocker` + 工具 `shell_exec` arguments.command="rm -rf /"；断言 `ToolStart` 后 `HookFired` 然后 `ToolEnd{is_error:true, content="blocked: dangerous command: rm -rf /"}`，工具未执行。

## 15. 实施步骤（高层 - 详细步骤进 plan 文档）

1. 加 `crates/parrot-core/src/hooks.rs` 创建 trait + types + registry（无 IO、无依赖）
2. 加 `AgentEvent::HookFired` + `TurnStopReason::BlockedHook` 到 protocol + 更新 `is_persistent`、roundtrip test
3. 改 `engine.rs` 加 6 个 dispatch 点 + `with_hooks` builder
4. 改 `session.rs` `SessionManager::with_hooks` + 传到 engine
5. 加 `parrot-config` 的 `HooksConfig` + `AppConfig.hooks`
6. 加 `crates/parrot-daemon/src/hooks/` 模块（mod + 两个 impl + `build_registry`）
7. 改 `runtime.rs` 构造 registry + 注入 manager
8. 单元/集成测试
9. `cargo build --workspace`+`cargo test --workspace`+`cargo clippy --workspace --all-targets -- -D warnings`+`cargo fmt --all -- --check`

## 16. 不在范围

- **脚本运行时**（rhai/quickjs 嵌入）：后续 layer，写 `ScriptHook` impl `Hook` 即可。
- **`input` 用户输入改写 hook**（pi 的 `input`）：下一批按需。
- **`session_before_*` cancellable 系列**（switch/fork/compact/tree）：Parrot 还没有 switch/fork，纯增 hook 没意义，留待功能落地时配套加。
- **Engine ReAct 流程重构成 Phase 编排框架**（idea #17 下半部分）：本轮只做 hook 插入，不改控制流骨架。
- **`EventSink` trait 把 EventLog 移出 core**（idea #2）：本设计 HookFired 不落盘，与该重构解耦；可后续单独做。
- **多 agent / sub-delegation / 复杂编排**：调研中 P3，本轮不动。

## 17. 风险与权衡

- ** trait + enum dispatch 的代价**：每个 hook impl 必须 `match event { ... }` 处理所有变体（不关心的用 `_ => NoOp`）。换来注册简单（一个 `Vec<Arc<dyn Hook>>`）+ outcome 类型统一。可接受，且 `supported()` 让不关心的 handler 直接跳过 await。
- **Resume 不可重放 hook 副作用**：本设计靠"hook 改动的最终结果已落盘"保证一致性。若未来加入"hook 修改了 user_message 本身"这种 hook（不在本轮），需重新审视。
- **Hook 慢调阻塞**：贴默认 5s + fail-open。配置过严的 timeout 可能导致 hook 被悄悄忽略，靠 `HookFired{outcome_kind:"timeout"}` 客户端可视化追踪。
- **`turn_start` 的 InjectMessages 简化决定**：不向下传递累加结果，避免 water 后连发"累加倍增"问题。代价是下游 hook 看不到上家的注入。这是合理简化（hook 一般不应观察 hook 之间副作用）。