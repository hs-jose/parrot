# Parrot — LLM Agent 技术方案

## 1. 项目定位

Parrot 是一个 Rust 实现的 LLM agent，兼具**通用编程助手**（类似 Claude Code）和**自主多步骤任务编排**能力。

### 核心差异化

- **Rust 原生性能** — 相比 Node/Python 实现更轻更快，资源占用更低
- **强自主编排** — ReAct 循环 + 可升级为混合规划模式，支持复杂任务自动分解
- **本地/云端混合** — 敏感代码走本地模型，通用任务走云端 API
- **多交互通道** — agent 核心与交互层物理分离，CLI / TUI / IM 都是对等客户端

### MVP 范围

- **Phase 1**: CLI + daemon，Anthropic API 单一提供商，跑通 ReAct 全链路
- **Phase 1.5**: TUI 客户端
- **Phase 2**: IM 接入（Discord 等），OpenAI + Ollama 适配器，MCP client
- **Phase 3**: 混合路由、模型故障切换、规划子模式

> TUI 从 MVP 移出，先确保 CLI → daemon → Anthropic → 工具调用的核心链路完全跑通后再加 TUI。

---

## 2. 顶层架构

**模式：Daemon + 瘦客户端（IPC via WebSocket）**

```
┌──────────┐  ┌──────────┐  ┌──────────┐
│ CLI      │  │ TUI      │  │ IM Bot   │
│ (thin)   │  │ (1.5)    │  │ (Phase 2)│
└────┬─────┘  └────┬─────┘  └────┬─────┘
     │              │              │
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
- **状态常驻** — daemon 管理 session 生命周期、工具注册、模型连接，客户端可随时断开重连
- **未来扩展** — 客户端可用任何语言实现（gRPC 备选，WS 更轻量）

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
}
```

### StreamEvent → ServerMessage 映射

编排引擎产出 `StreamEvent`（core 内部类型），daemon 的 session adapter 负责加上 `session_id` 转换为 `ServerMessage` 推给客户端：

| `StreamEvent` (core) | `ServerMessage` (protocol) |
|---|---|
| `TextDelta { delta }` | `TextDelta { session_id, delta }` |
| `ToolCallStart { id, name }` | `ToolCallStart { session_id, tool_id: id, tool_name: name }` |
| `ToolCallDelta { id, args_delta }` | `ToolCallDelta { session_id, tool_id: id, args_delta }` |
| `ToolCallEnd { id, arguments }` | `ToolCallEnd { session_id, tool_id: id, arguments }` |
| `ToolResult { id, result }` *(core 新增)* | `ToolResult { session_id, tool_id: id, result }` |
| `Finish { stop_reason, usage }` | `Finished { session_id, stop_reason, usage }` |

> `StreamEvent` 需要在原设计基础上补一个 `ToolResult` 变体，对应 ReAct 的 Observe 阶段——工具执行结果同样要流式推给客户端，不能只存在 core 内部。

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

---

## 5. Crate 工作区

```
parrot/
├── Cargo.toml              # workspace manifest
├── parrot.toml              # 默认配置文件
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
└── tests/                   # 集成测试
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

**Provider 适配器测试：cassette 录制/回放**

- 首次运行录制真实 Anthropic API 响应到 `tests/cassettes/`
- 后续测试从 cassette 回放，不消耗 API 配额
- CI 环境完全确定性
- 录制新 cassette 需显式触发（`RECORD=1 cargo test`）

---

## 11. 实施路线

### Phase 1 — MVP（当前）

1. 搭建 workspace，创建所有 crate skeleton（含 `parrot-transport`）
2. `parrot-protocol`：WS 消息类型定义 + serde 往返测试
3. `parrot-config`：配置文件解析 + `dirs` 路径解析
4. `parrot-transport`：Transport trait + WS server/client 实现
5. `parrot-core`：Tool trait + Provider trait + ReAct 引擎 + Session 状态机 + thiserror 错误类型
6. `parrot-daemon`：WS server + Anthropic adapter + 内置工具实现 + Token 握手 + Origin 校验
7. `parrot-cli`：CLI 瘦客户端（Hello 握手、Chat、流式输出展示）
8. 集成测试（cassette 录制）+ 文档

**MVP 验收标准**：CLI 启动 daemon → 创建 session → 发送"读取 X 文件并总结"→ agent 调用 `file_read` → 流式返回总结，全程 token 校验生效。

### Phase 1.5

- TUI 客户端（ratatui + mpsc 喂 WS 事件到事件循环）
- 工具二次确认流程（`ConfirmToolCall` 协议消息）
- Session 历史搜索与管理（CLI 子命令）

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
