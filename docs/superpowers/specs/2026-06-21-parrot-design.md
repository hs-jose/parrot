# Parrot — LLM Agent 技术方案

## 1. 项目定位

Parrot 是一个 Rust 实现的 LLM agent，兼具**通用编程助手**（类似 Claude Code）和**自主多步骤任务编排**能力。

### 核心差异化

- **Rust 原生性能** — 相比 Node/Python 实现更轻更快，资源占用更低
- **强自主编排** — ReAct 循环 + 可升级为混合规划模式，支持复杂任务自动分解
- **本地/云端混合** — 敏感代码走本地模型，通用任务走云端 API
- **多交互通道** — agent 核心与交互层物理分离，CLI / TUI / IM 都是对等客户端

### MVP 范围

- **Phase 1**: CLI + daemon，Anthropic API 单一提供商，跑通 ReAct 全链路 ✅
- **Phase 1.5**: TUI 客户端
  - **Phase 1.5a**: 协议扩展 + daemon 处理器 + CLI 子命令 ✅
  - **Phase 1.5b**: TUI 客户端 ✅
- **Phase 1.6**: 单命令启动 + 本地分发 ✅ — `parrot` 自动 spawn `parrotd` 子进程；打包/安装脚本支持交叉编译
- **Phase 2**: IM 接入（Discord 等），OpenAI + Ollama 适配器，MCP client
- **Phase 3**: 混合路由、模型故障切换、规划子模式

> TUI 从 MVP 移出，先确保 CLI → daemon → Anthropic → 工具调用的核心链路完全跑通后再加 TUI。

### Phase 1 实施状态（2026-06-21）

| 子项 | 状态 | 备注 |
|------|------|------|
| workspace + 4 个 lib crate skeleton | ✅ | `parrot-core/protocol/transport/config` |
| `parrot-protocol`：消息类型 + serde 往返测试 | ✅ | 10 个 roundtrip 测试通过（含 ModelList/History/EventLogEntry） |
| `parrot-config`：TOML 解析 + `dirs` 路径解析 + env 变量展开 | ✅ | |
| `parrot-transport`：Transport trait + WS server/client + Origin 校验 | ✅ | 3 个 transport 测试（含跨域拒绝）通过 |
| `parrot-core`：Tool/Provider trait + ReAct 引擎 + Session 状态机 + thiserror | ✅ | |
| `parrot-core`：上下文裁剪（滑窗 + 完整轮次保留） | ✅ | 4 个 context 测试通过 |
| `parrot-daemon`：WS server + Anthropic adapter + Token 握手 + Origin 校验 | ✅ | Anthropic adapter 含 SSE 流式解析 + 重试退避（§4.8） |
| `parrot-daemon`：内置工具（file_read/glob/grep/write/shell/web_fetch/web_search） | ✅ | 按配置开关注册 |
| `parrot-cli`：瘦客户端（Hello 握手、Chat、流式输出、rustyline 交互模式） | ✅ | |
| E2E 集成测试（mock provider 注入） | ✅ | `tests/integration/e2e_test.rs` 跑通完整 ReAct 链路 + ListModels + GetHistory + Abort |
| `ListModels` 服务端响应 | ✅ | `ServerMessage::ModelList` + 聚合各 provider 的 `list_models()` |
| `GetHistory` 服务端响应（event log 重放） | ✅ | `ServerMessage::History` + `EventLog::replay()` |
| Session 持久化补全（meta.json + index.json + snapshot 触发） | ✅ | `src/daemon/session_store.rs`；每 100 events 写 snapshot |
| Provider 重试退避（指数退避 + jitter） | ✅ | `src/daemon/providers/retry.rs`；`with_retry` 包装 `send_request` |
| 真正的 Abort 取消（中断在飞 LLM 流 + 工具执行） | ✅ | 引擎内 `tokio::select!` 监听 `cmd_rx`，drop stream / cancel tool future |
| System prompt 注入（SessionConfig.system_prompt → context） | ✅ | `ReActEngine::new` 接 `Option<String>`；默认模板在 `session::default_system_prompt` |
| Cassette 录制/回放 provider 测试 | ✅ | `tests/cassette_test.rs` + `tests/cassettes/anthropic/*.json`；`CassetteProvider` 实现 `LlmProvider` |

> 全绿基线：`cargo build --workspace` + `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`。

### Phase 1.5 实施状态（2026-07）

| 子项 | 状态 | 备注 |
|------|------|------|
| 协议扩展：ListSessions / GetHistory / ResumeSession / ListModels / ListTools | ✅ | `crates/parrot-protocol` |
| daemon 处理器：SessionManager + EventLog 持久化 + snapshot | ✅ | `crates/parrot-daemon/src/runtime.rs` + `session_store.rs` |
| CLI 子命令：`sessions list/show/resume/export`、`models`、`tools` | ✅ | `src/cli/main.rs` |
| 会话 resume：replay event log → rebuild context | ✅ | `EventLog::replay()`；resume 修了 AgentEnd lifecycle / disowned MessageStart 等多个边界 bug |
| TUI 客户端：ratatui + crossterm + tui-textarea | ✅ | `src/tui/`；3 层卡片布局，流式渲染，confirm 弹窗，多行输入 |

### Phase 1.6 实施状态（2026-07）

| 子项 | 状态 | 备注 |
|------|------|------|
| 单命令启动：`parrot` 自动 spawn `parrotd` | ✅ | `src/cli/daemon.rs`：绑随机端口（`127.0.0.1:0` 拿空闲端口，经 `PARROTD_PORT` env 传给子进程），`parrot` 退出时 `DaemonChild` Drop kill 子进程。**已废弃 v1 固定端口 + 复用单例方案** |
| `--connect ws://...` 显式连接既有 daemon | ✅ | 跳过自动 spawn，attach 模式 |
| 打包脚本 `package.ps1` / `package.sh` | ✅ | 支持 `-Target <triple>` 交叉编译，选 zip/tar.gz 归档 |
| 安装脚本 `install.ps1` / `install.sh` | ✅ | 支持 `-Target`；`~/.parrot/config/parrot.toml` 已存在则不覆盖 |
| 配置模板 `scripts/parrot.toml.template` | ✅ | `${ENV_VAR}` 占位符；provider 块以示例形式给出（Anthropic / DeepSeek 两例） |
| token 路径统一 | ✅ | `resolve_token_path()` 改用 `dirs::data_dir()`（Windows `%LOCALAPPDATA%`），与打包模板对齐 |

---

## 2. 顶层架构

**模式：Daemon + 瘦客户端（IPC via WebSocket）**

> **2026-07 更新**：daemon 生命周期从 v1 的"固定端口单例 + 跨调用复用 + outlives CLI"
> 改为 **per-invocation 子进程 + 随机端口 + CLI 退出即 kill**（参考 opencode TUI
> 模式）。`parrot` 启动时若未指定 `--connect`，会先占 `127.0.0.1:0` 拿一个空闲
> 端口，再以 `PARROTD_PORT` env 把端口传给新 spawn 的 `parrotd` 子进程，并持有
> `DaemonChild` kill-on-drop 守卫。这样多个项目并行跑 `parrot` 互不串配置，根除
> v1 单例 daemon 共享一份 `parrot.toml` 的 bug。显式 `--connect ws://...` 仍可
> attach 到既有 daemon（手动起的 `parrotd` 或远程服务）。

```
┌──────────┐  ┌──────────┐  ┌──────────┐
│ CLI      │  │ TUI      │  │ IM Bot   │
│ (thin)   │  │ (1.5)    │  │ (Phase 2)│
└────┬─────┘  └────┬─────┘  └────┬─────┘
     │              │              │
     │  --connect 缺省时：spawn   │
     │  parrotd 子进程(随机端口)   │
     │  持有 DaemonChild 守卫      │
     └──────┬───────┴──────┬───────┘
            │  WebSocket   │  (含本地 token 握手)
       ┌────┴──────────────┴────┐
       │    parrot-daemon       │
       │  ┌──────────────────┐  │
       │  │ orchestration    │  │
       │  │ engine           │  │
       │  ├──────────────────┤  │
       │  │ tool registry    │  │
       │  │ + tool impls     │  │
       │  ├──────────────────┤  │
       │  │ LLM providers    │  │
       │  ├──────────────────┤  │
       │  │ session manager  │  │
       │  │ (event log)      │  │
       │  └──────────────────┘  │
       └────────────────────────┘
```

### 为什么选 Daemon + Thin Clients

- **IPC 边界强制层分离** — agent 核心与交互层物理隔离，不会因内部重构影响客户端
- **IM 天然适配** — IM bot 就是另一个 WS 客户端，零架构改动
- **会话持久化跨重启** — daemon 把 event log 落盘，CLI/TUI 退出后下次起 daemon
  仍能 `sessions resume <id>`（resume 依赖磁盘上 events.log，不依赖 daemon 常驻进程）
- **未来扩展** — 客户端可用任何语言实现（gRPC 备选，WS 更轻量）

> 注：v1 设计文案里写"daemon 状态常驻…客户端可随时断开重连"。在 per-invocation
> 模型下，客户端断 重连到**同一个 daemon 进程**不再成立（CLI 退出会带走子进程）；
> 但 session 状态本身仍持久化在磁盘，重新 `parrot` 会起新 daemon 并 replay 历史，
> 实际体验等价。若需要真正的"daemon 常驻跨 CLI 重连"，可手动起 `parrotd` 并用
> `parrot --connect ws://...` attach。

---

## 3. 传输层

### Transport 抽象

Transport trait 与其 WS 实现放在独立的 **`parrot-transport`** crate（既非纯数据的 protocol，也非带业务逻辑的 core）。这样客户端和服务端可共享同一份抽象，同时不污染 core 与 protocol 的边界。

```rust
// parrot-transport: 服务端侧
#[async_trait]
pub trait TransportServer: Send + Sync {
    async fn accept(&self) -> Result<ClientConnection>;
}

// parrot-transport: 客户端侧
#[async_trait]
pub trait TransportClient: Send + Sync {
    async fn connect(&self, url: &str, token: &str) -> Result<ClientConnection>;
}

pub struct ClientConnection {
    pub id: ClientId,
    pub sender: mpsc::Sender<ServerMessage>,
    pub receiver: mpsc::Receiver<ClientMessage>,
}
```

默认实现：`WsTransportServer` / `WsTransportClient`，基于 `tokio-tungstenite`，绑定 `127.0.0.1`（仅本地访问）。

后续若要切换 gRPC，只需在该 crate 新增 `GrpcTransportServer` / `GrpcTransportClient` 实现。

### 消息类型（`parrot-protocol` crate，纯 serde 数据）

**Client → Server:**
```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// 握手（首个消息，携带本地 token）
    Hello { token: String, client_version: String },
    /// 新建会话
    CreateSession { config: Option<SessionConfig> },
    /// 发送聊天消息
    Chat { session_id: SessionId, message: String },
    /// 中断当前生成
    Abort { session_id: SessionId },
    /// 列出可用模型
    ListModels,
    /// 列出可用工具
    ListTools { session_id: SessionId },
    /// 获取 session 历史
    GetHistory { session_id: SessionId },
    /// 列出本地所有 session（Phase 1.5）
    ListSessions,
    /// 恢复已存在的 session（Phase 1.5）
    ResumeSession { session_id: SessionId },
    /// 工具二次确认响应（Phase 1.5）
    ConfirmToolCall {
        session_id: SessionId,
        tool_id: String,
        decision: ConfirmDecision,
    },
}

#[derive(Serialize, Deserialize, PartialEq)]
pub enum ConfirmDecision {
    /// 客户端用户确认执行
    Approve,
    /// 客户端用户拒绝执行
    Reject,
    /// 客户端超时未响应（daemon 侧也会本地超时）
    Timeout,
}
```

**Server → Client:**
```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// 握手确认
    HelloAck { server_version: String },
    /// Session 创建确认
    SessionCreated { session_id: SessionId },
    /// 文本增量（流式）
    TextDelta { session_id: SessionId, delta: String },
    /// 工具调用开始
    ToolCallStart { session_id: SessionId, tool_id: String, tool_name: String },
    /// 工具调用参数增量
    ToolCallDelta { session_id: SessionId, tool_id: String, args_delta: String },
    /// 工具调用完成（参数已完整）
    ToolCallEnd { session_id: SessionId, tool_id: String, arguments: Value },
    /// 工具执行结果（Observe 阶段）
    ToolResult { session_id: SessionId, tool_id: String, result: ToolOutput },
    /// 生成完成
    Finished { session_id: SessionId, stop_reason: StopReason, usage: Usage },
    /// 错误
    Error { session_id: Option<SessionId>, code: ErrorCode, message: String },
    /// 模型列表（响应 ListModels）
    ModelList { models: Vec<ModelInfo> },
    /// Session 历史响应（event log 重放）
    History { session_id: SessionId, entries: Vec<HistoryEntry> },
    /// Session 列表响应（Phase 1.5）
    SessionList { sessions: Vec<SessionMeta> },
    /// Session 恢复确认（Phase 1.5）
    SessionResumed { session_id: SessionId },
    /// 工具调用需二次确认（Phase 1.5）
    ToolCallConfirmationRequired {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
        arguments: Value,
    },
}
```

**新增纯数据类型（`parrot-protocol::types`）：**
```rust
/// 模型元信息（响应 ListModels）
#[derive(Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// 历史条目（响应 GetHistory，对齐 EventLogEntry 但带元数据）
#[derive(Serialize, Deserialize)]
pub struct HistoryEntry {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub entry: EventLogEntry,   // 复用 core 的 EventLogEntry（带 tag）
}

/// Session 元信息（响应 ListSessions / SessionList）
#[derive(Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub model: String,
    pub provider: String,
    pub title: Option<String>,
    pub total_tokens: u64,
    pub last_snapshot_seq: u64,
}
```

> `ModelInfo` 既出现在 protocol 也出现在 `parrot-core::types`，二者字段相同。`parrot-core` 的 `ModelInfo` 是引擎内部用，`parrot-protocol` 的是 wire 格式——保留两份避免 core 反向依赖 protocol 的 chrono 类型。daemon 在边界做转换。

### AgentEvent → ServerMessage 映射（2026-06-22 统一事件模型后）

> **重大变更（2026-06-22）**：原 `StreamEvent` / `EventLogEntry` / `ServerMessage` 三套语义重复的类型已合并为统一的 `AgentEvent`（详见 `docs/superpowers/specs/2026-06-22-parrot-event-model.md`）。`session_adapter.rs` 翻译层已删除；daemon 直接把 `AgentEvent` 装进 `ServerMessage::AgentEvent { event }` envelope 转发。`EventLogEntry` 已替换为 `AgentEvent`（持久化用同一类型，按 `is_persistent()` 过滤 `MessageDelta` / `ToolUpdate`）。

编排引擎产出 `AgentEvent`（protocol 公开类型），daemon 直接 envelope 后推给客户端。Provider 适配器产出 `ProviderStreamEvent`（core 内部类型，仅在 engine 内消费），engine 负责补齐 lifecycle envelope：

| `AgentEvent` (protocol) | `ServerMessage` (protocol) |
|---|---|
| `AgentStart { .. }` | `ServerMessage::AgentEvent { event }` |
| `TurnStart { .. }` / `TurnEnd { .. }` | 同上 |
| `MessageStart { .. }` / `MessageDelta { .. }` / `MessageEnd { .. }` | 同上 |
| `ToolStart { .. }` / `ToolUpdate { .. }` / `ToolEnd { .. }` | 同上 |
| `ToolConfirmRequired { .. }` | 同上 |
| `ReplayIntegrityWarning { .. }` | 同上 |
| `AgentEnd { .. }` | 同上 |

> `ProviderStreamEvent` 不出现在 wire 协议上。Provider 适配器（如 `AnthropicProvider`）吐出 `TextDelta` / `ToolCallStart` / `ToolCallDelta` / `ToolCallEnd` / `Finish`，engine 聚合后补 `MessageStart..MessageEnd` / `ToolStart..ToolEnd` / `TurnStart..TurnEnd` / `AgentStart..AgentEnd` 的嵌套边界。

### 非流式请求 → 响应映射

非 ReAct 流的请求是"一问一答"，daemon 直接合成 `ServerMessage`，不经 session adapter：

| `ClientMessage` | `ServerMessage` 响应 | 备注 |
|---|---|---|
| `Hello { token, .. }` | `HelloAck { server_version }` 或 `Error{AuthFailed}` | 握手阶段 |
| `CreateSession { config }` | `SessionCreated { session_id }` | 同时 spawn session task |
| `ListModels` | `ModelList { models }` | 聚合所有 provider 的 `list_models()` |
| `ListTools { session_id }` | 多条 `TextDelta` + `Finished` | 当前实现：JSON dump 到 TextDelta |
| `GetHistory { session_id }` | `History { session_id, events: Vec<PersistedAgentEvent> }` | 重放 events.log（2026-06-22 起 `EventLogEntry` → `AgentEvent`） |
| `ListSessions` *(1.5)* | `SessionList { sessions }` | 读 index.json |
| `ResumeSession { session_id }` *(1.5)* | `SessionResumed { session_id }` 或 `Error{SessionNotFound}` | 重放 + 重建 session task |
| `ConfirmToolCall { .. }` *(1.5)* | （无直接响应；驱动后续 `ToolResult` 或 `ToolCallEnd`） | 见 §4.5 二次确认 |

> `ListTools` 当前实现把工具 schema 用 `TextDelta` 流回，是非正规方式。Phase 1.5 改为新增 `ServerMessage::ToolList { tools: Vec<ToolDefinition> }`，与 `ModelList` 对称。

---

## 4. Agent 核心（`parrot-core`）

### 4.1 编排引擎

**默认：ReAct 循环**

```
Think → Act → Observe → Think → Act → Observe → ... → Final Answer
```

每轮循环：
1. **Think**：LLM 分析当前上下文，决定下一步（调用工具 or 回复用户）
2. **Act**：如果选择工具调用，执行工具并获取结果
3. **Observe**：将工具结果追加到上下文，回到 Think 阶段

**System prompt 注入**：session 创建时若 `SessionConfig.system_prompt` 非空，引擎在 context 初始位置插入一条 `ChatRole::System` 消息。后续每轮 ReAct 都保留这条消息（context 裁剪 §4.6 也永远保留 System）。MVP 不支持会话中途修改 system prompt；如需修改需新建 session。

```rust
// 引擎初始化 context 时：
let mut context: Vec<ChatMessage> = Vec::new();
if let Some(prompt) = &self.system_prompt {
    context.push(ChatMessage {
        role: ChatRole::System,
        content: prompt.clone(),
        tool_call_id: None, tool_name: None, tool_calls: None,
    });
}
```

**默认 system prompt**：若 `SessionConfig.system_prompt` 为空，daemon 注入一份"通用编程助手"模板（声明可用工具、ReAct 工作方式、安全约束）。模板放在 daemon 而非 core，遵守 core 的零 IO/零策略假设。

**后续升级**：ReAct + 规划子模式。复杂任务先让 LLM 生成高层计划，再逐步进入 ReAct 执行。MVP 阶段仅实现纯 ReAct，架构预留规划层的扩展点。

### 4.2 Tool 系统（MCP 对齐）

**Tool trait 定义在 `parrot-core`**（接口契约），但**具体工具实现放在 `parrot-daemon`**（因为 `file_read`/`shell_exec`/`web_fetch` 本质是 IO，违反 core 的零 IO 假设）。

```rust
// parrot-core: trait 定义
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;          // JSON Schema
    async fn call(&self, arguments: Value, ctx: &ToolContext) -> ToolResult;
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,   // 支持热注册
}
```

**MVP 内置工具（在 daemon 实现）：**
- `file_read` / `file_write` / `file_glob` / `file_grep`
- `shell_exec`（默认禁用，需配置开启）
- `web_fetch` / `web_search`

**MCP 集成路径（Phase 2）：** parrot daemon 作为 MCP client，外部 MCP server 的工具通过同一 trait 适配器接入，对编排引擎透明。

### 4.3 LLM Provider 抽象

核心思路：**内部规范类型 + 提供商适配器**

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError>;
    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatResponse, ProviderError>;
}
```

**统一流事件：**
```rust
pub enum StreamEvent {
    TextDelta { delta: String },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, args_delta: String },
    ToolCallEnd { id: String, arguments: Value },
    ToolResult { id: String, result: ToolOutput },   // 新增：Observe 阶段
    Finish { stop_reason: StopReason, usage: Usage },
}
```

**Anthropic 流式映射**（MVP 实现细节）：

| Anthropic SSE 事件 | `StreamEvent` |
|---|---|
| `message_start` | (内部状态，不产出事件) |
| `content_block_start { type: "text" }` | (准备累积文本) |
| `content_block_delta { type: "text_delta", text }` | `TextDelta { delta: text }` |
| `content_block_start { type: "tool_use", id, name }` | `ToolCallStart { id, name }` |
| `content_block_delta { type: "input_json_delta", partial_json }` | `ToolCallDelta { id, args_delta: partial_json }` |
| `content_block_stop` (tool_use) | `ToolCallEnd { id, arguments }` (累积解析后) |
| `message_delta { stop_reason, usage }` | `Finish { stop_reason, usage }` |
| `message_stop` | (流终止) |

多 tool call 并行：Anthropic 的 `content_block` 顺序产出，每个 block 有独立 `index`；adapter 用 `index → id` 映射表对齐。

**Provider 路由：** `ProviderRegistry` 按 model id 路由到对应适配器（如 `claude-sonnet-4-6` → Anthropic adapter）。

**MVP 策略：** 只实现 Anthropic adapter，但 Provider trait 和 ProviderRegistry 从第一天就设计为多提供商。

### 4.4 Session 管理

- 每个客户端连接可创建多个 session
- Session 之间完全隔离（独立上下文、工具集、模型选择）
- 持久化采用 **append-only event log** + 定期 snapshot（详见 §7）
- Session 恢复：daemon 重启后按需重放 event log 重建内存状态
- 并发模型详见 §4.5

**Session 生命周期与持久化触发点**：

| 事件 | daemon 动作 |
|------|-------------|
| `CreateSession` | 生成 `session_id`，创建 `{data_dir}/sessions/{id}/`，写 `meta.json`（initial）、追加 `SessionCreated` 到 events.log、更新 `index.json` |
| 每条 `EventLogEntry::append` | 写 events.log 一行；同步更新 `meta.json` 的 `updated_at`/`total_tokens`；每 100 条触发 snapshot |
| `Finished`（一轮 ReAct 结束） | 更新 `meta.json`；若 `title` 仍为空且这是第一轮，调用 LLM 生成 title（异步、失败不阻塞） |
| daemon 优雅退出 | 对所有 active session flush + 写最终 snapshot |
| daemon 崩溃 | 重启后通过 `index.json` 找到所有 session；按需重放（客户端 `ResumeSession` 时） |

**index.json（全局索引）**：

```json
{
  "version": 1,
  "sessions": [
    { "id": "uuid", "title": "重构 utils.rs", "model": "...", "updated_at": "...", "total_tokens": 15000 }
  ]
}
```

读写由 daemon 串行化（`RwLock<SessionIndex>`），不并发写——session 创建/更新事件先入队，单个 task 顺序应用，避免文件竞争。

**ResumeSession 流程（Phase 1.5）**：
1. 客户端发 `ResumeSession { session_id }`
2. daemon 查 `index.json` 确认存在
3. 读 `meta.json` → 读最近 snapshot → 重放 snapshot 之后的 events.log → 重建 `Vec<ChatMessage>`
4. spawn 新的 ReAct engine task，注入重建后的 context
5. 回 `SessionResumed { session_id }`
6. 后续 `Chat` 命令接续到该 task

> 重放产物（`Vec<ChatMessage>`）只含 user/assistant/tool 三类，System 由当前 daemon 的 system prompt 模板重新注入——这保证升级 system prompt 模板后旧 session 也能享受到新版行为。

### 4.5 并发模型

**每 session 一个独立 tokio task**，session 内部状态不跨 task 共享，避免锁竞争。

```
                        ┌──────────────────┐
                        │  Session Manager │
                        │  (HashMap<Id,    │
                        │   SessionHandle>)│
                        └────────┬─────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                  │
       ┌──────▼─────┐     ┌──────▼─────┐     ┌──────▼─────┐
       │ Session A  │     │ Session B  │     │ Session C  │
       │ task       │     │ task       │     │ task       │
       │            │     │            │     │            │
       │ ReAct loop │     │ ReAct loop │     │ (idle)     │
       └──────┬─────┘     └────────────┘     └────────────┘
              │
       ┌──────▼─────┐
       │ mpsc recv  │ ◄── Chat / Abort 命令
       └────────────┘
```

**SessionHandle**（manager 持有）：
```rust
pub struct SessionHandle {
    pub cmd_tx: mpsc::Sender<SessionCmd>,    // 发指令给 session task
    pub event_rx: mpsc::Receiver<StreamEvent>, // 接收 session 产出的事件
    pub abort_handle: AbortHandle,            // 强制取消 ReAct 循环
}

pub enum SessionCmd {
    Chat { message: String },
    Abort,
}
```

**取消语义**：
- `Abort` 通过 `tokio::select!` 在 ReAct 循环中监听，收到后立即结束当前 LLM 流（drop stream）和工具执行（cancel-safe）
- 已写入 session log 的事件不回滚（append-only），但当前 ReAct 轮次标记为 `aborted`
- 客户端收到 `Finished { stop_reason: Aborted }`

**真正的 Abort 实现**（修正初版仅 break 主循环的缺陷）：

每轮 ReAct 的"调 LLM + 工具执行"是两个独立可取消点。当前实现把 `cmd_rx.recv()` 放在 while 循环顶部，导致 `Abort` 必须等当前轮完整跑完才被处理。正确做法是把 `Abort` 监听下沉到每轮内部：

```rust
// 每轮 ReAct 内部：
loop {
    tokio::select! {
        // 1. 正常接收 provider stream 事件
        Some(event) = stream.inner.recv() => { /* 累积 text / tool_calls */ }

        // 2. 监听 abort 命令
        Some(SessionCmd::Abort) = cmd_rx.recv() => {
            drop(stream);                    // drop mpsc receiver → spawn 的 SSE 解析 task 退出
            emit Finish { stop_reason: Aborted, .. };
            return;                          // 整个 session task 结束
        }

        // 3. 监听工具取消（工具执行阶段，下方 else 分支）
        else => break;                       // stream 自然结束
    }
}
```

工具执行阶段类似：`tokio::select! { _ = tool.call(...) => ..., _ = cmd_rx.recv() => abort }`。但工具若已发出 IO（如 `file_write` 已落盘），不能"撤回"——只能让后续的 `ToolResult` 标记为 `is_error: true, content: "aborted"`，并把已落盘的事实写进 event log，让用户/LLM 知道副作用已发生。

`SessionHandle::abort_handle` 仅用于"强杀整个 session task"（如 daemon 关闭时），不参与正常 Abort 流程。

**工具二次确认流程（Phase 1.5）**：

`tools.sandbox.require_confirmation` 列表匹配的命令，daemon 在 `Act` 阶段不直接执行，而是走以下流程：

```
1. 引擎拿到 tool_calls 后，对每个需要确认的 tool_call：
   - 不立即调用 tool.call()
   - 通过 event_tx 发送 ToolCallConfirmationRequired { session_id, tool_id, tool_name, arguments }
   - 在 session task 内挂起一个 oneshot::Receiver<ConfirmDecision>
2. daemon 的 connection handler 收到 ToolCallConfirmationRequired 转发给客户端
3. 客户端 UI 弹确认（CLI 输入 y/n；TUI 模态框）
4. 客户端发 ConfirmToolCall { session_id, tool_id, decision }
5. daemon 路由到对应 session task，通过 oneshot sender 唤醒
6. 引擎根据 decision：
   - Approve → 正常 tool.call()，发 ToolResult
   - Reject  → 发 ToolResult { is_error: true, content: "user rejected" }，LLM 后续可决策
   - Timeout（默认 60s） → 同 Reject，但记录 "confirmation timeout"
```

需要"路由 client→session 的 confirm 回调"——daemon 端维护 `HashMap<(SessionId, ToolId), oneshot::Sender<ConfirmDecision>>`，挂在 `RwLock` 后面。这个 map 仅在 confirm 流程激活期间存在条目，正常路径零开销。

**ToolRegistry 并发**：`RwLock<HashMap>` 支持运行时热注册工具（Phase 2 场景），读多写少。

### 4.6 上下文窗口管理

`max_history_tokens` 触发裁剪时，按以下优先级保留：

1. **系统提示**（永远保留）
2. **最近 N 轮对话**（N 可配，默认 6）
3. **当前任务相关工具结果**（最近一轮的完整结果）
4. **历史摘要**：更早的对话用 LLM 生成摘要替代原文

**裁剪算法**（MVP：滑窗 + 丢弃）：
```
while total_tokens(messages) > max_history_tokens:
    if messages.len() <= keep_recent * 2:
        break   // 已到最小保留窗口，不再裁剪，交给模型自身处理
    drop messages[oldest_non_system_index]
```

**Phase 2 升级**：滑窗 + 摘要（被丢弃的消息送 LLM 生成 summary block，插入上下文）。

### 4.7 错误类型（thiserror）

`parrot-core` 与 `parrot-protocol` 使用 `thiserror` 定义可 `match` 的错误枚举，不向上游泄漏 `anyhow`：

```rust
// parrot-core
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool execution failed: {tool} - {message}")]
    ToolExecution { tool: String, message: String },
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    #[error("context window exceeded")]
    ContextWindowExceeded,
    #[error("config error: {0}")]
    Config(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("timeout after {0}ms")]
    Timeout(u64),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}
```

`anyhow` 仅用于 daemon 和客户端 binary 内部（应用层），不跨 crate 边界。

### 4.8 Provider 重试与退避

`ProviderError` 区分可重试与不可重试：

| `ProviderError` 变体 | 可重试 | 备注 |
|---|---|---|
| `RateLimited { retry_after_ms }` | ✅ | 优先按服务端 `retry-after` 头等待 |
| `Timeout(ms)` | ✅ | 网络/上游慢 |
| `Api { status: 5xx, .. }` | ✅ | 服务端错误 |
| `Api { status: 4xx (非 429), .. }` | ❌ | 请求本身错（鉴权、参数），重试无意义 |
| `Network(_)` | ✅ | 连接重置等 |
| `StreamError(_)` | ❌ | 流已部分发送，重试会破坏一致性 |

**退避策略（在 daemon 的 provider 调用包装层实现，不动 core trait）**：

```rust
// 伪代码：daemon/providers/retry.rs
const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS:   u64 = 30_000;

async fn with_retry<F, Fut, T>(op: F) -> Result<T, ProviderError>
where F: Fn() -> Fut, Fut: Future<Output = Result<T, ProviderError>>
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if !is_retryable(&e) => return Err(e),
            Err(e) if attempt >= MAX_RETRIES => return Err(e),
            Err(e) => {
                let delay = match &e {
                    ProviderError::RateLimited { retry_after_ms } => *retry_after_ms,
                    _ => BASE_DELAY_MS * 2u64.pow(attempt),
                };
                let delay = delay.min(MAX_DELAY_MS);
                let jitter = rand::rng().random_range(0..delay / 4 + 1);  // ±25% jitter
                tokio::time::sleep(Duration::from_millis(delay + jitter)).await;
                attempt += 1;
                tracing::warn!(attempt, delay_ms = delay + jitter, error = %e, "retrying provider");
            }
        }
    }
}
```

包装点：`AnthropicProvider::chat_stream` / `chat` 内部的 `send_request` 调用前包一层 `with_retry`。流式阶段（SSE 已开始）不再重试——一旦开始吐 delta，错误只能上报为 `StreamError`。

**降级（Phase 3 预留）**：当某 provider 连续 N 次失败，`ProviderRegistry` 可标记其为"暂时不可用"，`resolve(model)` 路由到 fallback provider。MVP 不实现，仅留 hook（registry 内 `health: HashMap<provider_id, HealthState>`）。

---

## 5. Crate 工作区

```
parrot/
├── Cargo.toml              # workspace manifest
├── parrot.toml              # 默认配置文件
├── AGENTS.md                # build/test/lint 指引（给 agent / 新贡献者）
├── crates/
│   ├── parrot-core/         # lib — 编排引擎、Tool/Provider trait、Session 状态机
│   ├── parrot-protocol/     # lib — WS 消息类型（纯 serde 数据）
│   ├── parrot-transport/    # lib — Transport 抽象 + WS 实现（server + client）
│   └── parrot-config/       # lib — 配置解析
├── src/
│   ├── daemon/              # binary: parrotd — WS server + 工具实现 + 胶水层
│   ├── cli/                 # binary: parrot  — CLI 瘦客户端
│   └── tui/                 # binary: parrot-tui — TUI 客户端（Phase 1.5）
├── docs/
│   └── superpowers/specs/   # 设计文档
└── tests/
    ├── integration/
    │   └── e2e_test.rs      # mock provider 注入式 E2E
    └── cassettes/           # provider API 录制（Phase 1 收尾）
        └── anthropic/       # anthropic SSE 帧序列
```

### Binary 与 Crate 命名

| Binary 名 | 所属位置 | 类型 |
|-----------|----------|------|
| `parrotd` | `src/daemon/` | 二进制（不是独立 crate，依赖所有 lib crate） |
| `parrot` | `src/cli/` | 二进制 |
| `parrot-tui` | `src/tui/` | 二进制（Phase 1.5） |

三个 binary 共享同一 workspace，通过 `[[bin]]` 在根 `Cargo.toml` 声明。lib crate 放 `crates/` 下。

### 依赖规则

| Crate | 规则 |
|-------|------|
| `parrot-core` | **零 IO 假设** — 不直接访问网络、文件系统、环境变量。Tool trait 在此定义，但工具实现不在此 |
| `parrot-protocol` | **纯数据** — 只有 serde 结构体和枚举，无业务逻辑、无 async |
| `parrot-transport` | **传输抽象** — Transport trait + WS 实现，不依赖 core |
| `parrot-config` | **配置解析** — 依赖 `dirs`（唯一拥有路径解析权的 crate） |
| 交互层（CLI/TUI） | **不依赖 parrot-core** — 仅通过 protocol + transport 与 daemon 通信 |
| `parrot-daemon` (binary) | **唯一胶水** — 唯一同时依赖 core + protocol + transport + config 并执行 IO 的组件，也是内置工具实现所在地 |

### 主要依赖

| Crate | 核心依赖 |
|-------|----------|
| parrot-core | `tokio`, `serde`, `serde_json`, `async-trait`, `async-stream`, `thiserror`, `tracing` |
| parrot-protocol | `serde`, `serde_json`, `uuid`, `thiserror` |
| parrot-transport | `tokio`, `tokio-tungstenite`, `futures-util`, `async-trait`, protocol |
| parrot-config | `serde`, `toml`, `dirs`, `thiserror` |
| parrot-daemon (binary) | 全部 lib crate + `reqwest`, `tokio`, `tracing-subscriber`, `anyhow` |
| parrot-cli (binary) | `tokio`, `clap`, `transport`, `protocol`, `config`, `anyhow` |
| parrot-tui (binary) | `tokio`, `clap`, `ratatui`, `crossterm`, `transport`, `protocol`, `config` |

---

## 6. 配置设计

### 配置文件：`parrot.toml`

```toml
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "{config_dir}/parrot/token"   # 本地 token 存储位置

[[providers]]
id = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"    # 支持环境变量展开
default_model = "claude-sonnet-4-6"

# [[providers]]                      # Phase 2
# id = "openai"
# api_key = "${OPENAI_API_KEY}"
# default_model = "gpt-5"

# [[providers]]                      # Phase 2
# id = "ollama"
# base_url = "http://localhost:11434"
# default_model = "llama4"

[tools]
# 默认全部禁用，显式开启
shell_allowed = false                # 默认 false，开启后受 sandbox 限制
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
# shell_exec 沙箱配置
working_dir = "."                    # 限定工作目录（相对 daemon 启动目录）
allowlist = []                       # 命令白名单，空 = 全部禁止
denylist = ["rm -rf /", "sudo", "chmod 777"]
require_confirmation = ["git push", "rm"]  # 需要客户端二次确认的命令

[session]
data_dir = "{data_dir}/parrot"       # 由 parrot-config 用 dirs 解析
max_history_tokens = 100000
keep_recent_turns = 6
```

### 配置加载优先级

1. 命令行参数（`--config path`）
2. 当前目录 `parrot.toml`
3. 用户配置目录 `{config_dir}/parrot/parrot.toml`（平台解析，如 Linux `~/.config/parrot/`）
4. 内置默认值

---

## 7. Session 持久化

### 存储格式：Append-only Event Log

每个 session 一个目录，包含事件日志和定期快照：

```
{data_dir}/parrot/                # 平台解析:
├── sessions/                     #   Linux ~/.local/share/parrot/
│   └── {session_id}/             #   macOS ~/Library/Application Support/parrot/
│       ├── events.log            #   Windows %APPDATA%\parrot\
│       ├── snapshot-001.json     # 定期快照（每 N 个事件或显式 flush）
│       └── meta.json             # session 元信息
├── index.json                    # session 索引（id → 摘要）
└── logs/
    └── parrot-daemon.log         # daemon 运行日志
```

**为什么不用单文件 JSON**：长会话每次更新都全量重写，写放大严重且容易在崩溃时损坏。Append-only event log 写入是 O(1)，崩溃恢复也只需丢弃最后一条不完整的事件。

### events.log 格式（每行一个 JSON 事件）

```jsonl
{"seq":1,"ts":"2026-06-21T10:00:00Z","type":"SessionCreated","model":"claude-sonnet-4-6","provider":"anthropic"}
{"seq":2,"ts":"2026-06-21T10:00:01Z","type":"UserMessage","content":"帮我重构这个函数"}
{"seq":3,"ts":"2026-06-21T10:00:02Z","type":"AssistantText","content":"好的，我先看看..."}
{"seq":4,"ts":"2026-06-21T10:00:03Z","type":"ToolCall","tool_id":"tc_1","tool_name":"file_read","arguments":{"path":"src/utils.rs"}}
{"seq":5,"ts":"2026-06-21T10:00:03Z","type":"ToolResult","tool_id":"tc_1","output":{"content":"..."}}
{"seq":6,"ts":"2026-06-21T10:00:05Z","type":"AssistantText","content":"我发现这个函数..."}
{"seq":7,"ts":"2026-06-21T10:00:06Z","type":"Finish","stop_reason":"end_turn","usage":{"input":1500,"output":800}}
```

### 恢复策略

- daemon 启动时不主动加载所有 session，按需加载（客户端 `GetHistory` 时重放）
- 重放：读最近 snapshot → 重放 snapshot 之后的 events.log
- Snapshot 触发：每 100 条事件或 daemon 优雅退出时

### meta.json

```json
{
  "id": "uuid-v4",
  "created_at": "2026-06-21T10:00:00Z",
  "updated_at": "2026-06-21T10:30:00Z",
  "model": "claude-sonnet-4-6",
  "provider": "anthropic",
  "title": "重构 utils.rs",
  "total_tokens": 15000,
  "last_snapshot_seq": 100
}
```

**写入时机**：
- `CreateSession`：写 initial 版本（`title: null`, `total_tokens: 0`, `last_snapshot_seq: 0`）
- 每次 `EventLogEntry::Finish`：更新 `updated_at` + 累加 `total_tokens`
- snapshot 完成后：更新 `last_snapshot_seq`
- 第一轮 `Finished` 后异步生成 `title`（调 LLM 用 user message 概括 ≤ 30 字），失败保持 null

**原子写**：写 `meta.json` 用 `write to .tmp then rename` 模式，避免崩溃时半截文件。

### index.json

```json
{
  "version": 1,
  "sessions": [
    {
      "id": "uuid-v4",
      "title": "重构 utils.rs",
      "model": "claude-sonnet-4-6",
      "provider": "anthropic",
      "updated_at": "2026-06-21T10:30:00Z",
      "total_tokens": 15000
    }
  ]
}
```

**角色**：客户端 `ListSessions` 不需要扫盘，单文件读即得全部 session 摘要。**只有 daemon 写**：每次 `CreateSession` / `meta.json` 更新时同步刷 index.json（同样原子写）。

### Snapshot 触发

| 触发条件 | 动作 |
|----------|------|
| `events.log` 自上次 snapshot 后新增 ≥ 100 条 | 写 `snapshot-{NNN}.json`，更新 `meta.json::last_snapshot_seq` |
| daemon 收到 `SIGTERM` / `Ctrl+C` | 对所有 active session flush 当前 events.log + 写最终 snapshot |
| 客户端显式 `FlushSession`（Phase 1.5 可选） | 立即 snapshot |

**Snapshot 内容**：完整的 `Vec<ChatMessage>` 序列化（含 tool_calls / tool_call_id），外加 `last_seq` 字段。重放时优先加载 snapshot，再 replay `seq > last_seq` 的 events。

**Snapshot 轮转**：保留最近 3 个 snapshot，更老的删除（避免无限增长）。

---

## 8. 安全模型

### 威胁模型

daemon 监听 `127.0.0.1:9876`，本机任何进程都可尝试连接。主要威胁：
- **恶意进程调用 `shell_exec`** 执行任意命令
- **DNS rebinding 攻击**：浏览器中恶意网页通过 DNS 重绑定访问本地 daemon
- **CSRF**：恶意网页通过 `fetch()` 向本地 daemon 发起 WS 连接

### 防护措施

**1. 本地 Token 握手（强制）**

- daemon 启动时生成随机 token，写入 `{config_dir}/parrot/token`（权限 0600）
- 客户端连接后必须先发 `Hello { token }`，daemon 校验失败立即断开
- 客户端通过读取同一 token 文件获取（仅本机用户可读）

**2. Origin 校验**

- WS 升级握手时检查 `Origin` header，拒绝非空且非 `http://localhost:*` / `http://127.0.0.1:*` 的请求
- 防 DNS rebinding 与浏览器 CSRF

**3. 工具权限沙箱**

- 所有危险工具（`shell_exec`、`file_write`）默认禁用，需配置显式开启
- `shell_exec` 沙箱：工作目录限定、命令白名单/黑名单、危险命令需客户端二次确认
- `file_write` 沙箱：工作目录限定、文件大小上限
- 客户端二次确认流程：daemon 发 `ToolCallStart` → 客户端 UI 弹确认 → 客户端发 `ConfirmToolCall`（Phase 1.5 扩展协议消息）

**4. 绑定 127.0.0.1**

- 默认仅监听 loopback，不暴露到局域网
- 如需远程访问（Phase 3），必须额外配置 TLS + 强认证

### Phase 1 MVP 安全基线

- ✅ Token 握手
- ✅ Origin 校验
- ✅ 工具默认禁用
- ✅ 127.0.0.1 绑定
- ⏳ 工具白名单/二次确认（Phase 1.5）
- ⏳ TLS（Phase 3，远程访问场景）

---

## 9. 错误处理

### 分层策略

| 层 | 策略 |
|----|------|
| `parrot-protocol` | `thiserror` 定义 `ProtocolError`（序列化/反序列化错误） |
| `parrot-core` | `thiserror` 定义 `AgentError` / `ProviderError`，可被下游 `match` |
| `parrot-transport` | `thiserror` 定义 `TransportError` |
| `parrot-config` | `thiserror` 定义 `ConfigError` |
| `parrot-daemon` (binary) | 捕获所有错误，转换为 `ServerMessage::Error` 发送给客户端；内部可用 `anyhow` 串联 |
| 客户端 | 展示用户可读的错误信息 |

### 关键错误场景

- **Provider API 超时/限流** → 指数退避重试（最多 3 次），失败后回 `ProviderError` 给客户端
- **Tool 执行失败** → 将错误注入上下文，让 LLM 自行决策重试或告知用户（不中断 ReAct 循环）
- **WS 连接断开** → 客户端自动重连（1s / 2s / 5s 退避，最多 3 次），重连后用 session_id 恢复
- **Daemon 崩溃** → Session event log 已持久化，重启后可按需重放恢复
- **Token 校验失败** → 立即断开连接，记录日志（防扫描）

---

## 10. 测试策略

| 层级 | 测试类型 | 说明 |
|------|----------|------|
| `parrot-core` | 单元测试 | Tool trait 实现测试、编排引擎纯逻辑测试 |
| `parrot-core` | 集成测试 | Mock LLM provider 进行 ReAct 循环测试 |
| `parrot-protocol` | 单元测试 | 消息序列化/反序列化往返测试 |
| `parrot-transport` | 集成测试 | WS server + client 回环测试 |
| `parrot-daemon` | 集成测试 | 启动 daemon → WS 连接 → 发送消息 → 验证响应 |
| 客户端 | 集成测试 | CLI 端到端测试（daemon 在后台） |

**当前已落地测试**（`cargo test --workspace` 全绿）：

- `parrot-protocol/tests/roundtrip.rs` — 7 个 serde 往返测试
- `parrot-transport/tests/transport_test.rs` — 3 个 WS 测试（含跨域 Origin 拒绝）
- `parrot-core/tests/react_loop.rs` — 1 个 mock provider + ReAct loop 测试
- `parrot-core::context::tests` — 4 个上下文裁剪测试
- `parrot-core::tool::tests` — 3 个 ToolRegistry 测试
- `src/daemon/auth.rs::tests` — 3 个 token 生成/校验测试
- `tests/integration/e2e_test.rs` — 1 个完整 E2E（Hello → CreateSession → Chat → tool_use → ToolResult → Finished）

**Provider 适配器测试：cassette 录制/回放**

- 首次运行录制真实 Anthropic API 响应到 `tests/cassettes/`
- 后续测试从 cassette 回放，不消耗 API 配额
- CI 环境完全确定性
- 录制新 cassette 需显式触发（`RECORD=1 cargo test`）

**Cassette 框架设计**：

```
tests/
└── cassettes/
    └── anthropic/
        ├── chat_stream_simple_text.json       # 单文本响应
        ├── chat_stream_tool_use.json          # 单工具调用 + 后续 end_turn
        ├── chat_stream_multi_tool_use.json    # 并行多工具
        └── chat_stream_error_rate_limited.json
```

每个 cassette 是一份 JSON，结构对齐 Anthropic SSE 帧序列：

```json
{
  "name": "chat_stream_tool_use",
  "request": {
    "url": "/v1/messages",
    "method": "POST",
    "body_matches": { "model": "claude-sonnet-4-6", "stream": true }
  },
  "response": {
    "status": 200,
    "events": [
      { "event": "message_start", "data": { "message": { "usage": { "input_tokens": 10 } } } },
      { "event": "content_block_start", "data": { "index": 0, "content_block": { "type": "tool_use", "id": "tc_1", "name": "file_read" } } },
      { "event": "content_block_delta",  "data": { "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } } },
      { "event": "content_block_delta",  "data": { "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"src/lib.rs\"}" } } },
      { "event": "content_block_stop",   "data": { "index": 0 } },
      { "event": "message_delta",        "data": { "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 25 } } },
      { "event": "message_stop",         "data": {} }
    ]
  }
}
```

**实现路径**：在 `src/daemon/providers/anthropic.rs` 抽出 `fn send_request` 的可插拔 HTTP client。测试用一个 `MockHttpClient` 实现 `reqwest` 的子集接口，根据 request URL/body 匹配 cassette，按 `events` 数组顺序 yield SSE 帧。生产代码用真实 `reqwest::Client`。

或者更轻量：测试不调 `AnthropicProvider::chat_stream`，而是直接构造一个 `CassetteProvider` 实现 `LlmProvider` trait，把 cassette 里的 events 转成 `StreamEvent` 推入 mpsc——绕过 SSE 解析，专注测引擎 + 工具链路。前者覆盖 SSE 解析、后者覆盖 ReAct 行为，互补。

MVP 采用 **后者**（`CassetteProvider`）：成本低、覆盖关键路径。SSE 解析的单测另写：把 `parse_sse_stream` 抽 pub(crate) 或加 `#[cfg(test)]` 入口，喂入字节流验证 StreamEvent 序列。

**录制模式（`RECORD=1`）**：

```rust
// tests/cassette/record.rs (only compiled when RECORD=1)
if env::var("RECORD").is_ok() {
    let real = AnthropicProvider::new(api_key, ..);
    let stream = real.chat_stream(..).await?;
    let mut events = Vec::new();
    while let Some(ev) = stream.inner.recv().await { events.push(ev); }
    serde_json::to_writer(File::create(path)?, &events)?;
}
```

CI 默认跑回放，开发者本地偶尔 `RECORD=1 cargo test --test cassette` 刷新。

---

## 11. 实施路线

### Phase 1 — MVP（当前）

1. ✅ 搭建 workspace，创建所有 crate skeleton（含 `parrot-transport`）
2. ✅ `parrot-protocol`：WS 消息类型定义 + serde 往返测试
3. ✅ `parrot-config`：配置文件解析 + `dirs` 路径解析
4. ✅ `parrot-transport`：Transport trait + WS server/client 实现
5. ✅ `parrot-core`：Tool trait + Provider trait + ReAct 引擎 + Session 状态机 + thiserror 错误类型
6. ✅ `parrot-daemon`：WS server + Anthropic adapter + 内置工具实现 + Token 握手 + Origin 校验
7. ✅ `parrot-cli`：CLI 瘦客户端（Hello 握手、Chat、流式输出展示）
8. ✅ 集成测试（mock provider 注入）+ 文档

**Phase 1 收尾（本批推进）**：

9. ✅ `ListModels` 协议消息 + daemon 实现（聚合各 provider 的 `list_models()`）
10. ✅ `GetHistory` 协议消息 + event log 重放
11. ✅ Session 持久化补全：`meta.json` 写入时机、`index.json`、snapshot 触发与轮转
12. ✅ Provider 重试退避（`with_retry` 包装层，§4.8）
13. ✅ 真正的 Abort 取消（`tokio::select!` 下沉到 ReAct 每轮内部，§4.5）
14. ✅ System prompt 注入（`SessionConfig.system_prompt` → context 头部，§4.1）
15. ✅ Cassette 测试框架（`CassetteProvider` + `tests/cassettes/anthropic/`）

**MVP 验收标准**：CLI 启动 daemon → 创建 session → 发送"读取 X 文件并总结"→ agent 调用 `file_read` → 流式返回总结，全程 token 校验生效。

### Phase 1.5

- ✅ TUI 客户端（ratatui + mpsc 喂 WS 事件到事件循环）— 2026-06-25 完成（Phase 1.5b）
- ✅ 工具二次确认流程（`ConfirmToolCall` 协议消息）
- ✅ Session 历史搜索与管理（CLI 子命令）
- ✅ `ListSessions` / `ResumeSession` 协议消息 + daemon 处理器
- ✅ `ToolList` 消息（替换当前 `ListTools` 用 `TextDelta` 回传 JSON 的临时实现）

> 详细规范与实施状态见 [`2026-06-21-parrot-phase-1.5.md`](2026-06-21-parrot-phase-1.5.md)。该文档拆分自本文档以控制单文件规模——主文档保留架构与 Phase 1 基线，phase-1.5 文档专注该阶段的协议扩展、daemon 处理器、CLI 子命令、二次确认流程、TUI（Phase 1.5b 已实施 2026-06-25）。

### Phase 2

- OpenAI adapter + Ollama adapter
- MCP client 集成
- IM bot（Discord）作为新客户端
- 上下文裁剪升级为滑窗 + 摘要

### Phase 3

- 混合路由（按内容敏感度选择本地/云端模型）
- 模型故障切换（fallback chain）
- 高级编排（ReAct + 规划子模式）
- TLS + 远程访问支持
