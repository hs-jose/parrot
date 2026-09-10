# Parrot MCP Client (tools-only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** parrotd 作为 MCP client，接入外部 stdio MCP server 的 tools（rmcp SDK），注册进 `ToolRegistry`，对引擎透明；失败状态推送 UI + `/mcp` 查询。

**Architecture:** 新 crate `parrot-mcp`（生命周期编排 + `Tool` trait 适配器）；daemon 在 `runtime.rs` 接线（启动后台 task、确认前缀合并、广播通知转发）；协议层新增 3 条消息；`ToolRegistry` 增加热下线方法 `unregister`。

**Tech Stack:** rmcp 3.3（client + transport-child-process + which-command + server/transport-io/macros 供 mock server）、tokio、serde_json、thiserror、tracing。

**Spec:** `docs/superpowers/specs/2026-09-11-parrot-mcp-client-design.md`（已批准）。MCP 协议背景见 `docs/mcp.md`。

## Global Constraints

- 命名约定：工具注册名一律 `mcp__<server_id>__<tool_name>`（spec §2）。
- 错误保真（spec §3.4）：adapter 不泛化/吞并错误；JSON-RPC 错误带 `code`+`message`+`data`；连接断开/超时/spawn 失败分别有明确文案；`isError=true` 的 content 全文原样回喂。
- `parrot-core` 保持 zero-IO（本计划只给它加纯内存的 `unregister`）。
- crate 边界错误用 thiserror；`anyhow` 仅允许在 bin（`src/bin/mock_server.rs` 用 `Box<dyn Error>` 即可，不引 anyhow）。
- 单个 MCP server 启动失败只隔离该 server：`tracing::warn` + `McpNotice(failed)`，不阻断 daemon 启动、不影响其他 server（spec §3.3）。
- 已确认的 rmcp 行为（实现者直接依赖，勿重新发明）：
  - stderr 默认 `Stdio::inherit()`（子进程日志直达 daemon 进程 stderr，无管道堵塞问题，无需排空任务）。
  - `TokioChildProcess` 的 Drop 会 kill 子进程（`ChildWithCleanup`），孤儿进程有兜底。
  - 协议版本协商由 rmcp 自动完成，不手写版本号。
  - Windows 上 `npx` 等 `.cmd` shim 用 `rmcp::transport::which_command` 解析 PATH。
- 验证命令（每任务末跑）：`cargo build --workspace`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
- 提交风格沿用仓库惯例：`feat(mcp): ...` / `test(mcp): ...` / `docs: ...`。

---

### Task 1: `ToolRegistry::unregister` 热下线（parrot-core）

**Files:**
- Modify: `crates/parrot-core/src/tool.rs`（`ToolRegistry` impl，约 75-93 行区域）

**Interfaces:**
- Consumes: 无（纯内存结构）
- Produces: `ToolRegistry::unregister(&self, name: &str)`（async，不存在时 no-op）。Task 6/7 的 manager 依赖它。

- [ ] **Step 1: 写失败测试**

加到 `crates/parrot-core/src/tool.rs` 的 `mod tests`（复用现有 `EchoTool` fixture）：

```rust
#[tokio::test]
async fn unregister_removes_tool() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool)).await;
    registry.unregister("echo").await;
    assert!(registry.get("echo").await.is_none());
}

#[tokio::test]
async fn unregister_unknown_tool_is_noop() {
    let registry = ToolRegistry::new();
    registry.unregister("nope").await;
}

#[tokio::test]
async fn unregister_keeps_in_flight_arc_alive() {
    let registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool)).await;
    let held = registry.get("echo").await.expect("held before unregister");
    registry.unregister("echo").await;
    assert!(registry.get("echo").await.is_none());
    let ctx = ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = registry.execute("echo", json!({}), &ctx).await;
    assert!(out.is_err(), "registry 视角下工具已不存在");
    let r = held.call(json!({"message": "hi"}), &ctx).await.unwrap();
    assert_eq!(r.content, "hi", "进行中调用持有的 Arc<dyn Tool> 不受 unregister 影响");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-core --lib tool::tests::unregister -- --nocapture`
Expected: FAIL，`no method named 'unregister' found`

- [ ] **Step 3: 最小实现**

在 `impl ToolRegistry` 中（`register` 之后）加：

```rust
/// 移除一个已注册工具（MCP server 崩溃/下线时批量移除其工具用）。
/// 不存在时静默 no-op。进行中的调用仍持有 `Arc<dyn Tool>`，不受影响。
pub async fn unregister(&self, name: &str) {
    self.tools.write().await.remove(name);
}
```

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot-core --lib tool::tests::unregister -- --nocapture` → 3 个 PASS
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-core/src/tool.rs
git commit -m "feat(core): ToolRegistry::unregister 支持工具热下线"
```

---

### Task 2: `McpConfig` 配置（parrot-config）

**Files:**
- Modify: `crates/parrot-config/src/config.rs`（struct 定义区 + `default_config()` + tests）

**Interfaces:**
- Consumes: 无
- Produces: `McpConfig { servers: Vec<McpServerConfig> }`、`McpServerConfig { id, command, args, env, startup_timeout_seconds, call_timeout_seconds, require_confirmation }`。Task 4/6/7 依赖。

- [ ] **Step 1: 写失败测试**

加到 `crates/parrot-config/src/config.rs` 的 `mod tests`：

```rust
#[test]
fn mcp_servers_parse_from_toml() {
    let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "m"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6

[[mcp.servers]]
id = "playwright"
command = "npx"
args = ["@playwright/mcp@latest"]
env = { DISPLAY = ":0" }
startup_timeout_seconds = 10
call_timeout_seconds = 60
require_confirmation = false
"#;
    let c: AppConfig = toml::from_str(toml).unwrap();
    assert_eq!(c.mcp.servers.len(), 1);
    let s = &c.mcp.servers[0];
    assert_eq!(s.id, "playwright");
    assert_eq!(s.command, "npx");
    assert_eq!(s.args, vec!["@playwright/mcp@latest"]);
    assert_eq!(s.env.get("DISPLAY").map(String::as_str), Some(":0"));
    assert_eq!(s.startup_timeout_seconds, 10);
    assert_eq!(s.call_timeout_seconds, 60);
    assert!(!s.require_confirmation);
}

#[test]
fn mcp_entry_defaults_fill_in() {
    let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "p"
api_key = "x"
default_model = "m"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6

[[mcp.servers]]
id = "mock"
command = "mock-server"
"#;
    let c: AppConfig = toml::from_str(toml).unwrap();
    let s = &c.mcp.servers[0];
    assert!(s.args.is_empty());
    assert!(s.env.is_empty());
    assert_eq!(s.startup_timeout_seconds, 30);
    assert_eq!(s.call_timeout_seconds, 120);
    assert!(s.require_confirmation, "确认默认开启");
}

#[test]
fn missing_mcp_section_defaults_empty() {
    let c = AppConfig::default_config();
    assert!(c.mcp.servers.is_empty(), "无 [mcp] 段 ⇒ servers 为空，零行为变化");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-config mcp -- --nocapture`
Expected: FAIL，`no field 'mcp'`

- [ ] **Step 3: 实现**

`config.rs` 顶部 `fn default_true` 之后加：

```rust
fn default_mcp_startup_timeout() -> u64 {
    30
}
fn default_mcp_call_timeout() -> u64 {
    120
}
```

struct 区（`HooksConfig` 定义之后）加：

```rust
/// MCP server 接入配置（spec §3.2）。无 `[mcp]` 段时 `servers` 为空 ⇒ 零行为变化。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One `[[mcp.servers]]` entry: 本地 stdio MCP server 子进程。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 工具名前缀与日志标识（必填，daemon 内查重）。
    pub id: String,
    /// 可执行文件或 PATH 上的命令名（如 `npx`）。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 附加环境变量（子进程继承 daemon 环境 + 此项）。
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// spawn+握手+枚举工具 总超时。
    #[serde(default = "default_mcp_startup_timeout")]
    pub startup_timeout_seconds: u64,
    /// 单次 tools/call 超时。
    #[serde(default = "default_mcp_call_timeout")]
    pub call_timeout_seconds: u64,
    /// 该 server 的工具调用是否要求用户确认（默认 true，MCP 规范基线）。
    #[serde(default = "default_true")]
    pub require_confirmation: bool,
}
```

`AppConfig` 加字段（`hooks` 之后）：

```rust
    #[serde(default)]
    pub mcp: McpConfig,
```

`default_config()` 的构造里加：

```rust
            mcp: McpConfig::default(),
```

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot-config` → 全 PASS（含既有 hooks 测试不受影响）
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-config/src/config.rs
git commit -m "feat(config): McpConfig 与 [[mcp.servers]] 配置段"
```

---

### Task 3: 协议消息 `McpNotice` / `McpServers` / `ListMcpServers`（parrot-protocol）

**Files:**
- Modify: `crates/parrot-protocol/src/types.rs`（新增 wire 类型）
- Modify: `crates/parrot-protocol/src/server_message.rs`
- Modify: `crates/parrot-protocol/src/client_message.rs`
- Test: `crates/parrot-protocol/tests/roundtrip.rs`

**Interfaces:**
- Consumes: 无
- Produces: `McpServerState { Starting, Connected, Failed, Stopped }`、`McpServerStatusWire { id, state, detail, tool_count }`、`ServerMessage::McpNotice { id, state, detail, tool_count }`、`ServerMessage::McpServers { entries }`、`ClientMessage::ListMcpServers`。Task 6/7/8/9 依赖。

- [ ] **Step 1: 写失败测试**

加到 `crates/parrot-protocol/tests/roundtrip.rs`（沿用文件顶部的 use，`McpServerState` 从 `parrot_protocol::types` 引入）：

```rust
#[test]
fn mcp_notice_roundtrip() {
    let msg = ServerMessage::McpNotice {
        id: "playwright".into(),
        state: McpServerState::Failed,
        detail: "spawn 失败: program not found".into(),
        tool_count: 0,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
    assert!(json.contains(r#""type":"McpNotice""#), "tagged enum: {json}");
    assert!(json.contains("\"failed\""), "state 序列化为 snake_case: {json}");
}

#[test]
fn mcp_servers_roundtrip() {
    let msg = ServerMessage::McpServers {
        entries: vec![parrot_protocol::types::McpServerStatusWire {
            id: "mock".into(),
            state: McpServerState::Connected,
            detail: String::new(),
            tool_count: 3,
        }],
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}

#[test]
fn client_list_mcp_servers_roundtrip() {
    let msg = ClientMessage::ListMcpServers;
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-protocol --test roundtrip mcp -- --nocapture`
Expected: FAIL（找不到 `McpServerState` / `ListMcpServers`）

- [ ] **Step 3: 实现**

`types.rs`（`ToolOutput` 附近）加：

```rust
/// MCP server 运行状态（`McpNotice` / `McpServers` 使用）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Starting,
    Connected,
    Failed,
    Stopped,
}

/// 单个 MCP server 的当前状态快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerStatusWire {
    pub id: String,
    pub state: McpServerState,
    pub detail: String,
    pub tool_count: u32,
}
```

`server_message.rs` enum 加（`ShellResult` 之后）：

```rust
    /// MCP server 状态变更（daemon 主动推送，broadcast 转发）。
    McpNotice {
        id: String,
        state: McpServerState,
        detail: String,
        tool_count: u32,
    },
    /// `ClientMessage::ListMcpServers` 的应答。
    McpServers {
        entries: Vec<McpServerStatusWire>,
    },
```

`client_message.rs` enum 加（`Shell` 之后）：

```rust
    /// 查询 MCP server 状态。Response: `ServerMessage::McpServers`。
    ListMcpServers,
```

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot-protocol` → 全 PASS
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-protocol/src/types.rs crates/parrot-protocol/src/client_message.rs crates/parrot-protocol/src/server_message.rs crates/parrot-protocol/tests/roundtrip.rs
git commit -m "feat(protocol): MCP 状态通知与查询消息"
```

---

### Task 4: `parrot-mcp` crate 骨架 + mock server + 纯映射函数

**Files:**
- Create: `crates/parrot-mcp/Cargo.toml`
- Create: `crates/parrot-mcp/src/lib.rs`
- Create: `crates/parrot-mcp/src/adapter.rs`（本任务只放纯函数，Task 5 加 `McpTool`）
- Create: `crates/parrot-mcp/src/bin/mock_server.rs`（开发/测试用 mock MCP server）
- Modify: `Cargo.toml`（workspace members + 根 `[dependencies]`）
- Test: `crates/parrot-mcp/src/adapter.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `parrot_config::McpServerConfig`（Task 2）、rmcp 类型
- Produces: `parrot_mcp::{qualified_tool_name, flatten_content, map_service_error}`（pub）。Task 5/6 依赖；`bin/parrot_mcp_mock_server` 供 Task 6 集成测试与手动验证。

- [ ] **Step 1: 建 crate**

`crates/parrot-mcp/Cargo.toml`：

```toml
[package]
name = "parrot-mcp"
version.workspace = true
edition.workspace = true

[dependencies]
rmcp = { version = "3.3", features = [
    "client",
    "transport-child-process",
    "which-command",
    "server",
    "transport-io",
    "macros",
] }
parrot-core = { path = "../parrot-core" }
parrot-config = { path = "../parrot-config" }
parrot-protocol = { path = "../parrot-protocol" }
async-trait = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
```

说明：`server`/`transport-io`/`macros` 三个 feature 只被 mock server bin 用到；为避免 cargo feature 组合的脆弱性统一打开（纯编译期成本，无运行时影响）。

根 `Cargo.toml`：

- workspace `members` 列表加 `"crates/parrot-mcp"`。
- 根 `[dependencies]`（parrotd binary 侧）加：

```toml
parrot-mcp           = { path = "crates/parrot-mcp" }
```

`crates/parrot-mcp/src/lib.rs`：

```rust
pub mod adapter;
```

- [ ] **Step 2: mock server bin**

`crates/parrot-mcp/src/bin/mock_server.rs`（开发/测试 fixture：`echo`、`fail`、`exit` 三个工具；供 `parrotd` 配置指向做手工验证，也供 Task 6 集成测试以 `CARGO_BIN_EXE` 引用）：

```rust
//! Mock MCP stdio server（开发/测试用）。
//! 工具：echo(message) 回显；fail() 返回工具级错误；exit() 使本进程退出
//! （用于验证 client 侧崩溃检测/热下线）。

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};

#[derive(Clone)]
struct MockServer {
    tool_router: ToolRouter<MockServer>,
}

#[tool_router]
impl MockServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo the message argument back with an echo: prefix")]
    fn echo(&self, message: String) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "echo: {message}"
        ))]))
    }

    #[tool(description = "Always returns a tool-level error result")]
    fn fail(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "mock failure detail",
        )]))
    }

    #[tool(description = "Terminate this mock server process (crash simulation)")]
    fn exit(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text("bye")]))
    }
}

#[tool_handler]
impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            name: "parrot-mock-mcp".into(),
            version: "0.1.0".into(),
            ..Default::default()
        }
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let service = MockServer::new()
            .serve(rmcp::transport::stdio())
            .await
            .expect("serve stdio");
        let _ = service.waiting().await;
    });
}
```

注意：`echo` 的 `message: String` 参数由 rmcp 宏从 arguments 的 `message` 字段提取；`exit` 工具返回成功后由调用方决定（这里直接正常返回；真正退出交给 client 侧 close 或测试断言——若宏生成的 handler 不便直接 `std::process::exit`，在 `exit` 返回前调用 `std::process::exit(0)` 亦可，行为目标：进程在工具调用后终止）。运行验证用 Step 4 的测试兜底。

- [ ] **Step 3: adapter 纯函数 + 失败测试**

`crates/parrot-mcp/src/adapter.rs`：

```rust
use rmcp::model::ContentBlock;
use rmcp::service::ServiceError;

/// 工具注册名：`mcp__<server_id>__<tool_name>`（spec §2，Claude Code 同款约定）。
pub fn qualified_tool_name(server_id: &str, tool_name: &str) -> String {
    format!("mcp__{server_id}__{tool_name}")
}

/// 把 MCP content 数组拍平为纯文本（spec §3.4 结果映射）。
pub fn flatten_content(content: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(i) => {
                parts.push(format!("[image: {}, {} bytes]", i.mime_type, i.data.len()));
            }
            ContentBlock::Audio(a) => {
                parts.push(format!("[audio: {}, {} bytes]", a.mime_type, a.data.len()));
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource link: {}]", link.uri));
            }
            ContentBlock::Resource(r) => {
                let uri = match &r.resource {
                    rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri.clone(),
                    rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri.clone(),
                };
                parts.push(format!("[embedded resource: {uri}]"));
            }
        }
    }
    parts.join("\n")
}

/// rmcp `ServiceError` → 保真错误文案（spec §3.4 错误保真原则）。
pub fn map_service_error(server_id: &str, e: &ServiceError) -> String {
    match e {
        ServiceError::McpError(err) => match &err.data {
            Some(data) => format!(
                "MCP 协议错误 code={}: {}\ndata: {data}",
                err.code.0, err.message
            ),
            None => format!("MCP 协议错误 code={}: {}", err.code.0, err.message),
        },
        ServiceError::TransportClosed => {
            format!("MCP server {server_id} 连接已断开(进程可能已退出)")
        }
        ServiceError::Timeout { timeout } => format!("MCP 内部超时({timeout:?})"),
        ServiceError::Cancelled { reason } => match reason {
            Some(r) => format!("MCP 调用被取消: {r}"),
            None => "MCP 调用被取消".to_string(),
        },
        other => format!("MCP 调用失败: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ErrorData, ErrorCode, ImageContent, TextContent};
    use rmcp::service::ServiceError;
    use rmcp::model::McpError;

    #[test]
    fn qualified_name_format() {
        assert_eq!(
            qualified_tool_name("playwright", "browser_navigate"),
            "mcp__playwright__browser_navigate"
        );
    }

    #[test]
    fn flatten_text_joins_with_newline() {
        let blocks = vec![ContentBlock::text("a"), ContentBlock::text("b")];
        assert_eq!(flatten_content(&blocks), "a\nb");
    }

    #[test]
    fn flatten_image_becomes_placeholder() {
        let img = ContentBlock::Image(ImageContent::new("ABCD", "image/png"));
        assert_eq!(flatten_content(&[img]), "[image: image/png, 4 bytes]");
    }

    #[test]
    fn flatten_empty_is_empty() {
        assert_eq!(flatten_content(&[]), "");
    }

    #[test]
    fn service_error_mcp_error_includes_code_and_data() {
        let err = ServiceError::McpError(McpError::new(
            ErrorCode::INVALID_PARAMS,
            "bad input",
            Some(serde_json::json!({"hint": "x"})),
        ));
        let s = map_service_error("mock", &err);
        assert!(s.contains("code=-32602"), "got: {s}");
        assert!(s.contains("bad input"));
        assert!(s.contains(r#""hint":"x""#) || s.contains("hint"), "data 原样: {s}");
    }

    #[test]
    fn service_error_transport_closed_mentions_server() {
        let s = map_service_error("pw", &ServiceError::TransportClosed);
        assert!(s.contains("pw") && s.contains("连接已断开"), "got: {s}");
    }

    #[test]
    fn text_content_new_helper_matches_construct() {
        let t = TextContent::new("hello");
        assert_eq!(t.text, "hello");
        let e = ErrorData::internal_error("boom", None);
        assert_eq!(e.code.0, -32603);
    }
}
```

- [ ] **Step 4: 编译 + 单测**

Run: `cargo test -p parrot-mcp` → adapter 单测全 PASS（mock bin 能编译）
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/parrot-mcp
git commit -m "feat(mcp): parrot-mcp crate 骨架、mock server 与纯映射函数"
```

---

### Task 5: `McpTool` 适配器（实现 `parrot_core::Tool`）

**Files:**
- Modify: `crates/parrot-mcp/src/adapter.rs`（追加 `McpTool` / `McpService` / `ListChangeNotify`）
- Modify: `crates/parrot-mcp/src/lib.rs`（re-export）
- Test: `crates/parrot-mcp/tests/adapter_tool.rs`

**Interfaces:**
- Consumes: Task 4 的纯函数；rmcp `RunningService<RoleClient, H>`（`call_tool(&self, params) -> Result<CallToolResult, ServiceError>`，MRTR-aware；`list_all_tools()` 在 deref 到的 `Peer<RoleClient>` 上；`close_with_timeout(&mut self, Duration)`；`is_closed(&self) -> bool`）
- Produces:
  - `pub struct McpTool`（impl `parrot_core::tool::Tool`）
  - `pub type McpService = Arc<RwLock<RunningService<RoleClient, ListChangeNotify>>>`
  - `pub struct ListChangeNotify { pub tx: mpsc::Sender<()> }`（impl rmcp `ClientHandler`）
  - `McpTool::new(server_id: &str, def: &rmcp::model::Tool, service: McpService, call_timeout_secs: u64) -> Self`
  - `McpTool::call_for_test(&self, arguments: Value) -> ToolOutput`（Task 6 无 ToolContext 的调用口）。Task 6/7 依赖这些类型。

- [ ] **Step 1: 写失败测试**

`crates/parrot-mcp/tests/adapter_tool.rs`（走 mock server 子进程，验证 Tool trait 语义）：

```rust
//! McpTool 适配器行为测试：借助 mock server 验证 name/schema/调用映射。

use parrot_core::tool::Tool;
use parrot_mcp::{McpService, qualified_tool_name};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

async fn connect_mock() -> (McpService, Vec<rmcp::model::Tool>) {
    let cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot_mcp_mock_server"));
    let transport = rmcp::transport::TokioChildProcess::new(cmd).expect("spawn mock server");
    let (tx, _rx) = mpsc::channel::<()>(4);
    let service = parrot_mcp::ListChangeNotify { tx }
        .serve(transport)
        .await
        .expect("handshake");
    let tools = service.list_all_tools().await.expect("list tools");
    (Arc::new(RwLock::new(service)), tools)
}

#[tokio::test]
async fn mcp_tool_trait_semantics() {
    let (service, defs) = connect_mock().await;
    let echo = defs.iter().find(|t| t.name == "echo").expect("echo tool");
    let tool = parrot_mcp::McpTool::new("mock", echo, Arc::clone(&service), 30);

    assert_eq!(tool.name(), "mcp__mock__echo");
    assert!(tool.description().contains("Echo"));
    let schema = tool.input_schema();
    assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));

    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = tool
        .call(serde_json::json!({"message": "hi"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.content, "echo: hi");
    assert!(!out.is_error);
}

#[tokio::test]
async fn tool_level_error_maps_to_is_error_with_full_text() {
    let (service, defs) = connect_mock().await;
    let fail = defs.iter().find(|t| t.name == "fail").expect("fail tool");
    let tool = parrot_mcp::McpTool::new("mock", fail, service, 30);
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
    assert!(out.is_error, "server isError=true 必须映射 is_error");
    assert_eq!(out.content, "mock failure detail", "错误全文原样");
}

#[tokio::test]
async fn unknown_tool_name_yields_protocol_error_with_code() {
    let (service, _defs) = connect_mock().await;
    let t = defs_placeholder_tool(&service);
    let out = t.call_for_test(serde_json::json!({})).await;
    assert!(out.is_error);
    assert!(out.content.contains("code=-32602") || out.content.contains("code=-32603") || out.content.contains("MCP 协议错误"), "保真: {}", out.content);
}

fn defs_placeholder_tool(service: &McpService) -> parrot_mcp::McpTool {
    unreachable!("占位，防止误读——见下方实现说明")
}
```

> 实现说明：`unknown tool` 用例不依赖 defs 里的工具——直接用 `rmcp::model::Tool::new("no_such_tool", "phantom", rmcp::model::JsonObject::default())` 构造一个 `McpToolDef` 传给 `McpTool::new("mock", &def, service, 30)`，然后 `call_for_test(json!({}))`。上面的 `defs_placeholder_tool` 辅助函数按此实现（删掉 `unreachable!` 桩）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-mcp --test adapter_tool`
Expected: FAIL，`McpTool` / `ListChangeNotify` 未导出

- [ ] **Step 3: 实现 adapter.rs**

追加到 `crates/parrot-mcp/src/adapter.rs`：

```rust
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parrot_core::tool::{Tool, ToolContext};
use parrot_protocol::types::ToolOutput;
use rmcp::handler::client::{ClientHandler, NotificationContext};
use rmcp::model::{CallToolRequestParams, JsonObject, Tool as McpToolDef};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use serde_json::Value;
use tokio::sync::{RwLock, mpsc};

use crate::manager;
pub use crate::manager::qualified_tool_name;

/// server `tools/list_changed` 通知 → 唤醒 manager 重枚举。
#[derive(Clone)]
pub struct ListChangeNotify {
    pub tx: mpsc::Sender<()>,
}

impl ClientHandler for ListChangeNotify {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.tx.send(()).await;
    }
}

/// manager 与各 `McpTool` 共享的 rmcp client 服务句柄。
/// `call_tool` 只需 `&self`，读锁即可；`close_with_timeout` 需要 `&mut`，manager 用写锁。
pub type McpService = Arc<RwLock<RunningService<RoleClient, ListChangeNotify>>>;

pub struct McpTool {
    server_id: String,
    qualified_name: String,
    raw_name: String,
    description: String,
    input_schema: Value,
    service: McpService,
    call_timeout_secs: u64,
}

impl McpTool {
    pub fn new(
        server_id: &str,
        def: &McpToolDef,
        service: McpService,
        call_timeout_secs: u64,
    ) -> Self {
        Self {
            server_id: server_id.to_string(),
            qualified_name: qualified_tool_name(server_id, &def.name),
            raw_name: def.name.to_string(),
            description: def
                .description
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_default(),
            input_schema: def.schema_as_json_value(),
            service,
            call_timeout_secs,
        }
    }

    /// 无 `ToolContext` 的调用入口（测试/管理用途，与 `Tool::call` 共用核心逻辑）。
    pub async fn call_for_test(&self, arguments: Value) -> ToolOutput {
        self.invoke(arguments).await
    }

    async fn invoke(&self, arguments: Value) -> ToolOutput {
        let args: JsonObject = arguments.as_object().cloned().unwrap_or_default();
        let params = CallToolRequestParams::new(self.raw_name.clone()).with_arguments(args);
        let service = Arc::clone(&self.service);
        let call = async move {
            let svc = service.read().await;
            svc.call_tool(params).await
        };
        match tokio::time::timeout(Duration::from_secs(self.call_timeout_secs), call).await {
            Err(_) => ToolOutput {
                content: format!(
                    "MCP 调用超时(超过 {}s, server 未响应)",
                    self.call_timeout_secs
                ),
                is_error: true,
            },
            Ok(Err(e)) => {
                let detail = map_service_error(&self.server_id, &e);
                tracing::warn!(
                    server = %self.server_id,
                    tool = %self.qualified_name,
                    "MCP tool call failed: {detail}"
                );
                ToolOutput {
                    content: detail,
                    is_error: true,
                }
            }
            Ok(Ok(result)) => {
                let is_error = result.is_error.unwrap_or(false);
                if is_error {
                    tracing::warn!(
                        server = %self.server_id,
                        tool = %self.qualified_name,
                        "MCP tool reported error"
                    );
                }
                ToolOutput {
                    content: flatten_content(&result.content),
                    is_error,
                }
            }
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(self.invoke(arguments).await)
    }
}
```

修正项（写代码时同步做）：`use parrot_core::error::AgentError;`；`qualified_tool_name` / `map_service_error` / `flatten_content` 就定义在本文件（Task 4 已建），删除误写的 `crate::manager` 引用；`ErrorData::internal_error` 等测试辅助按编译器提示微调。`lib.rs` 改为：

```rust
pub mod adapter;
pub mod manager;

pub use adapter::{ListChangeNotify, McpService, McpTool};
pub use manager::McpManager;
```

（`manager` 模块 Task 6 建档；本任务先把 `lib.rs` 保持为 `pub mod adapter;` + `pub use adapter::{ListChangeNotify, McpService, McpTool};`，Task 6 再加 manager。）

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot-mcp` → 全 PASS
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-mcp
git commit -m "feat(mcp): McpTool 适配器——Tool trait 到 rmcp tools/call 的映射"
```

---

### Task 6: `McpManager` 生命周期 + 集成测试

**Files:**
- Create: `crates/parrot-mcp/src/manager.rs`
- Modify: `crates/parrot-mcp/src/lib.rs`（导出 `McpManager`）
- Test: `crates/parrot-mcp/tests/lifecycle.rs`

**Interfaces:**
- Consumes: Task 1 `ToolRegistry::unregister`、Task 2 `McpServerConfig`、Task 3 `McpServerState`/`McpServerStatusWire`、Task 4/5 adapter
- Produces:
  - `parrot_mcp::start_all(registry: Arc<ToolRegistry>, servers: Vec<McpServerConfig>) -> McpManager`（非阻塞：每 server 一个后台 task）
  - `McpManager::{subscribe(&self) -> broadcast::Receiver<McpServerStatusWire>, status_snapshot(&self) -> Vec<McpServerStatusWire>, shutdown(&self)}`
  - 崩溃检测：monitor 每 1s 查 `is_closed()`；`list_changed` → 重枚举并整组换工具。Task 7 依赖全部。

- [ ] **Step 1: 写失败集成测试**

`crates/parrot-mcp/tests/lifecycle.rs`：

```rust
//! MCP client 完整生命周期：spawn→握手→注册→调用→崩溃热下线→优雅关闭。

use parrot_config::McpServerConfig;
use parrot_core::tool::ToolRegistry;
use parrot_mcp::{McpManager, McpServerState};
use std::sync::Arc;
use std::time::Duration;

fn mock_config() -> McpServerConfig {
    McpServerConfig {
        id: "mock".into(),
        command: env!("CARGO_BIN_EXE_parrot_mcp_mock_server").into(),
        args: vec![],
        env: Default::default(),
        startup_timeout_seconds: 30,
        call_timeout_seconds: 30,
        require_confirmation: false,
    }
}

async fn wait_state(
    manager: &McpManager,
    id: &str,
    want: McpServerState,
    secs: u64,
) -> parrot_protocol::types::McpServerStatusWire {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let snap = manager.status_snapshot().await;
        if let Some(s) = snap.iter().find(|s| s.id == id) {
            if s.state == want {
                return s.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for {id} -> {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn spawn_handshake_register_call() {
    let registry = Arc::new(ToolRegistry::new());
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![mock_config()]).await;

    let status = wait_state(&manager, "mock", McpServerState::Connected, 30).await;
    assert_eq!(status.tool_count, 3, "echo/fail/exit");

    let echo = registry.get("mcp__mock__echo").await.expect("echo registered");
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = echo
        .call(serde_json::json!({"message": "hi"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.content, "echo: hi");
    assert!(!out.is_error);

    let fail = registry.get("mcp__mock__fail").await.expect("fail registered");
    let out = fail.call(serde_json::json!({}), &ctx).await.unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "mock failure detail", "server 错误全文保真");

    manager.shutdown().await;
    let status = wait_state(&manager, "mock", McpServerState::Stopped, 10).await;
    assert_eq!(status.tool_count, 0);
    assert!(registry.get("mcp__mock__echo").await.is_none(), "关闭后工具下线");
}

#[tokio::test]
async fn crash_triggers_hot_unregister_with_notice() {
    let registry = Arc::new(ToolRegistry::new());
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![mock_config()]).await;
    let mut rx = manager.subscribe();
    wait_state(&manager, "mock", McpServerState::Connected, 30).await;

    let exit = registry.get("mcp__mock__exit").await.expect("exit registered");
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let _ = exit.call(serde_json::json!({}), &ctx).await.unwrap();

    let mut stopped = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while stopped.is_none() && tokio::time::Instant::now() < deadline {
        if let Ok(notice) = rx.try_recv() {
            if notice.id == "mock" && notice.state == McpServerState::Stopped {
                stopped = Some(notice);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stopped.is_some(), "崩溃后应收到 McpNotice(Stopped)");
    assert!(registry.get("mcp__mock__echo").await.is_none(), "崩溃后工具热下线");
}

#[tokio::test]
async fn bad_command_is_isolated_with_failed_notice() {
    let registry = Arc::new(ToolRegistry::new());
    let mut bad = mock_config();
    bad.id = "nope".into();
    bad.command = "this_binary_definitely_does_not_exist_12345".into();
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![bad, mock_config()]).await;
    let mut rx = manager.subscribe();

    let failed = wait_state(&manager, "nope", McpServerState::Failed, 30).await;
    assert!(!failed.detail.is_empty(), "失败必须带具体原因: {:?}", failed.detail);

    let ok = wait_state(&manager, "mock", McpServerState::Connected, 30).await;
    assert_eq!(ok.tool_count, 3, "坏 server 不影响好 server");

    let mut got_failed_notice = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(n) = rx.try_recv() {
            if n.id == "nope" && n.state == McpServerState::Failed {
                got_failed_notice = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(got_failed_notice, "失败应广播 McpNotice(Failed)");
    manager.shutdown().await;
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-mcp --test lifecycle`
Expected: FAIL，`McpManager` 未导出

- [ ] **Step 3: 实现 manager.rs**

```rust
//! MCP server 生命周期编排（spec §3.3）：
//! 每 server 一个 tokio task：spawn → 握手 → 枚举 → 注册（热注册，下一 turn 可见）
//! → monitor（is_closed 轮询 + list_changed 重枚举）。
//! 启动失败只隔离该 server：warn + McpNotice(Failed)，不阻断 daemon。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_config::McpServerConfig;
use parrot_core::tool::ToolRegistry;
use parrot_protocol::types::{McpServerState, McpServerStatusWire};
use rmcp::service::RoleClient;
use tokio::sync::{OnceCell, RwLock, broadcast, mpsc};

use crate::adapter::{ListChangeNotify, McpService, McpTool, map_service_error, qualified_tool_name};

pub type McpStatusMap = Arc<RwLock<HashMap<String, McpServerStatusWire>>>;

struct ServerRuntime {
    id: String,
    /// 握手完成前为空（此时 shutdown 只 abort 任务，transport Drop 兜底杀子进程）。
    service: Arc<tokio::sync::OnceCell<McpService>>,
    tool_names: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

pub struct McpManager {
    /// 状态变更通知（daemon 转发给客户端 UI）。晚订阅者收不到历史通知——
    /// 补救手段是 `ListMcpServers` 查询。
    notices: broadcast::Sender<McpServerStatusWire>,
    status: McpStatusMap,
    servers: Vec<ServerRuntime>,
}

pub async fn start_all(
    registry: Arc<ToolRegistry>,
    servers: Vec<McpServerConfig>,
) -> McpManager {
    let (notices, _) = broadcast::channel(16);
    let status: McpStatusMap = Arc::new(RwLock::new(HashMap::new()));
    let mut runtimes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for server in servers {
        if server.id.is_empty() {
            tracing::warn!("MCP server id 为空，跳过");
            continue;
        }
        if !seen.insert(server.id.clone()) {
            tracing::warn!("MCP server id '{}' 重复，跳过", server.id);
            continue;
        }
        let (list_tx, list_rx) = mpsc::channel::<()>(4);
        let service_cell = Arc::new(tokio::sync::OnceCell::new());
        let tool_names = Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = tokio::spawn(run_server_task(
            server.clone(),
            Arc::clone(&registry),
            notices.clone(),
            Arc::clone(&status),
            ListChangeNotify { tx: list_tx },
            list_rx,
            Arc::clone(&service_cell),
            Arc::clone(&tool_names),
        ));
        runtimes.push(ServerRuntime {
            id: server.id.clone(),
            service: service_cell,
            tool_names,
            task,
        });
    }
    McpManager {
        notices,
        status,
        servers: runtimes,
    }
}

impl McpManager {
    /// 订阅状态变更通知（connection handler 建立时调用）。
    pub fn subscribe(&self) -> broadcast::Receiver<McpServerStatusWire> {
        self.notices.subscribe()
    }

    /// 当前全部 server 状态快照（`ListMcpServers` 应答用）。
    pub async fn status_snapshot(&self) -> Vec<McpServerStatusWire> {
        let map = self.status.read().await;
        let mut list: Vec<McpServerStatusWire> = map.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 优雅关闭：close 每个连接（transport.close → 等 3s → kill 子进程），
    /// abort monitor，最后批量 unregister 工具并广播 Stopped。
    pub async fn shutdown(&self) {
        for rt in &self.servers {
            rt.task.abort();
            if let Some(service) = rt.service.get() {
                let mut svc = service.write().await;
                let _ = svc.close_with_timeout(Duration::from_secs(5)).await;
            }
            for name in rt.tool_names.lock().unwrap().drain(..) {
                // registry 从 runtime 里拿不到——shutdown 由 Task 7 的 daemon
                // 在工具注册表仍可用时调用；这里经由闭包注入（见下）。
                (self.unregister_hook)(&name).await;
            }
            self.set_status(&rt.id, McpServerState::Stopped, "daemon 关闭", 0)
                .await;
        }
    }

    async fn set_status(&self, id: &str, state: McpServerState, detail: &str, tool_count: u32) {
        let entry = McpServerStatusWire {
            id: id.to_string(),
            state,
            detail: detail.to_string(),
            tool_count,
        };
        self.status.write().await.insert(id.to_string(), entry.clone());
        let _ = self.notices.send(entry);
    }
}
```

> 写代码时修正：`shutdown` 里的 `unregister_hook` 不要用函数字段——`McpManager` 直接持有 `registry: Arc<ToolRegistry>`（`start_all` 已有），`unregister` 走 `self.registry.unregister(name).await`。`set_status` 需要 `&self`（当前写法正确）。

`run_server_task`（同文件续）：

```rust
async fn run_server_task(
    server: McpServerConfig,
    registry: Arc<ToolRegistry>,
    notices: broadcast::Sender<McpServerStatusWire>,
    status: McpStatusMap,
    notify_handler: ListChangeNotify,
    list_rx: mpsc::Receiver<()>,
    service_cell: Arc<tokio::sync::OnceCell<McpService>>,
    tool_names: Arc<std::sync::Mutex<Vec<String>>>,
) {
    set_status_map(&status, &notices, &server.id, McpServerState::Starting, "", 0).await;

    let startup = Duration::from_secs(server.startup_timeout_seconds);
    let attempt = tokio::time::timeout(startup, async {
        let transport = make_transport(&server).map_err(|e| format!("spawn 失败: {e}"))?;
        let service = notify_handler
            .clone()
            .serve(transport)
            .await
            .map_err(|e| format!("握手失败: {e}"))?;
        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| map_service_error(&server.id, &e))?;
        Ok::<_, String>((service, tools))
    })
    .await;

    let (service, tools) = match attempt {
        Err(_) => {
            let detail = format!("启动超时(超过 {}s)", server.startup_timeout_seconds);
            tracing::warn!(server = %server.id, "{detail}");
            set_status_map(&status, &notices, &server.id, McpServerState::Failed, &detail, 0).await;
            return; // transport Drop → ChildWithCleanup kill 子进程
        }
        Ok(Err(e)) => {
            tracing::warn!(server = %server.id, "{e}");
            set_status_map(&status, &notices, &server.id, McpServerState::Failed, &e, 0).await;
            return;
        }
        Ok(Ok(v)) => v,
    };

    let service: McpService = Arc::new(RwLock::new(service));
    let _ = service_cell.set(Arc::clone(&service));

    let names = register_tools(&server.id, &registry, &tools, &service, server.call_timeout_seconds).await;
    *tool_names.lock().unwrap() = names.clone();
    let count = names.len() as u32;
    set_status_map(&status, &notices, &server.id, McpServerState::Connected, "", count).await;

    // monitor：崩溃检测(1s 轮询) + list_changed 重枚举
    let mut list_rx = list_rx;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if service.read().await.is_closed() {
                    tracing::warn!(server = %server.id, "MCP server 进程已退出");
                    for name in tool_names.lock().unwrap().drain(..) {
                        registry.unregister(&name).await;
                    }
                    set_status_map(&status, &notices, &server.id, McpServerState::Stopped, "server 进程已退出", 0).await;
                    return;
                }
            }
            _ = list_rx.recv() => {
                let tools = match service.read().await.list_all_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(server = %server.id, "list_changed 重枚举失败: {}", map_service_error(&server.id, &e));
                        continue;
                    }
                };
                for name in tool_names.lock().unwrap().drain(..) {
                    registry.unregister(&name).await;
                }
                let names = register_tools(&server.id, &registry, &tools, &service, server.call_timeout_seconds).await;
                *tool_names.lock().unwrap() = names.clone();
                set_status_map(&status, &notices, &server.id, McpServerState::Connected, "", names.len() as u32).await;
            }
        }
    }
}

fn make_transport(
    server: &McpServerConfig,
) -> std::io::Result<rmcp::transport::TokioChildProcess> {
    let mut cmd = rmcp::transport::which_command(&server.command)?;
    cmd.args(&server.args);
    for (k, v) in &server.env {
        cmd.env(k, v);
    }
    rmcp::transport::TokioChildProcess::new(cmd)
}

async fn register_tools(
    server_id: &str,
    registry: &ToolRegistry,
    tools: &[rmcp::model::Tool],
    service: &McpService,
    call_timeout_secs: u64,
) -> Vec<String> {
    let mut names = Vec::new();
    for def in tools {
        let qualified = qualified_tool_name(server_id, &def.name);
        if registry.get(&qualified).await.is_some() {
            tracing::warn!(server = server_id, tool = %qualified, "MCP 工具名冲突，跳过注册");
            continue;
        }
        registry
            .register(Arc::new(McpTool::new(server_id, def, Arc::clone(service), call_timeout_secs)))
            .await;
        names.push(qualified);
    }
    names
}

async fn set_status_map(
    status: &McpStatusMap,
    notices: &broadcast::Sender<McpServerStatusWire>,
    id: &str,
    state: McpServerState,
    detail: &str,
    tool_count: u32,
) {
    let entry = McpServerStatusWire {
        id: id.to_string(),
        state,
        detail: detail.to_string(),
        tool_count,
    };
    status.write().await.insert(id.to_string(), entry.clone());
    let _ = notices.send(entry);
}
```

修正项：`RoleClient` import 若未用则删；`service_cell.set` 时不要 move（`OnceCell::set` 接受值，传 `Arc::clone`）；`shutdown` 中 `close_with_timeout` 的 `&mut` 来自 `service.write().await`（`RwLock`）。`use rmcp::service::{RoleClient}` 视编译器提示增删。

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot-mcp` → lifecycle 3 个测试 + adapter 测试全 PASS
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-mcp
git commit -m "feat(mcp): MCP server 生命周期管理与崩溃热下线"
```

---

### Task 7: daemon 接线（runtime.rs）

**Files:**
- Modify: `crates/parrot-daemon/src/runtime.rs`
- Modify: `crates/parrot-daemon/Cargo.toml`（加 `parrot-mcp` 依赖；根 `Cargo.toml` `[dependencies]` Task 4 已加）
- Test: `crates/parrot-daemon/src/runtime.rs` 无单测改动；行为由 Task 9 e2e 验证

**Interfaces:**
- Consumes: `parrot_mcp::{start_all, McpManager}`（Task 6）、Task 3 消息、Task 2 配置
- Produces: daemon 行为——MCP 后台启动、确认前缀合并、`McpNotice` 转发、`ListMcpServers` 应答。`run_with` / `run_with_confirm_timeout` 签名变更（追加 `mcp: McpManager` 参数）——所有调用点同步更新（含 e2e 里 4 处 `run_with` 调用，由本任务一并修改）。

- [ ] **Step 1: 修改 runtime.rs**

`run()`（MCP 注册接在 tools/providers 之后）：

```rust
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    let auth = Auth::new(&token_path).await?;
    let auth = Arc::new(auth);

    let tool_registry = Arc::new(ToolRegistry::new());
    let provider_registry = Arc::new(ProviderRegistry::new());

    parrot_tools::register_all(&tool_registry, &config).await;
    parrot_providers::register_all(&provider_registry, &config).await;

    let mcp = parrot_mcp::start_all(Arc::clone(&tool_registry), config.mcp.servers.clone()).await;
    run_with_confirm_timeout(
        config,
        auth,
        provider_registry,
        tool_registry,
        Duration::from_secs(60),
        mcp,
    )
    .await
}
```

`run_with` 保持对外签名（内部自建 manager，e2e 兼容）：

```rust
pub async fn run_with(
    config: AppConfig,
    auth: Arc<Auth>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mcp = parrot_mcp::start_all(Arc::clone(&tool_registry), config.mcp.servers.clone()).await;
    run_with_confirm_timeout(
        config,
        auth,
        provider_registry,
        tool_registry,
        Duration::from_secs(60),
        mcp,
    )
    .await
}
```

`run_with_confirm_timeout` 追加参数 `mcp: parrot_mcp::McpManager`，函数体内：

```rust
    let mcp = Arc::new(mcp);
    let mut require_confirmation = config.tools.sandbox.require_confirmation.clone();
    for server in &config.mcp.servers {
        if server.require_confirmation {
            require_confirmation.push(format!("mcp__{}__", server.id));
        }
    }
    let confirm_config = ConfirmConfig {
        require_confirmation,
        timeout: confirm_timeout,
        router: Some(Arc::clone(&confirm_router)),
    };
```

关闭段（`shutdown_all` 之后）加：

```rust
    info!("Shutting down MCP servers...");
    mcp.shutdown().await;
```

主 loop 里 accept 分支把 `mcp` 克隆进 handler：`let mcp = Arc::clone(&mcp);`（`tokio::spawn(handle_connection(...))` 调用追加实参）。

`handle_connection` 追加参数 `mcp: Arc<parrot_mcp::McpManager>`，函数开头（`authenticated` 声明前）加通知转发：

```rust
    // MCP 状态通知 → 客户端（晚订阅收不到历史通知，可经 ListMcpServers 补查）
    let mut notice_rx = mcp.subscribe();
    let notice_sender = client.sender.clone();
    tokio::spawn(async move {
        while let Ok(n) = notice_rx.recv().await {
            let msg = ServerMessage::McpNotice {
                id: n.id,
                state: n.state,
                detail: n.detail,
                tool_count: n.tool_count,
            };
            if notice_sender.send(msg).await.is_err() {
                break;
            }
        }
    });
```

消息 match 加新臂（`ListSessions` 臂后）：

```rust
            ClientMessage::ListMcpServers => {
                let entries = mcp.status_snapshot().await;
                let _ = client
                    .sender
                    .send(ServerMessage::McpServers { entries })
                    .await;
            }
```

- [ ] **Step 2: 编译 + 全测试**

Run: `cargo build --workspace` → OK
Run: `cargo test --workspace` → 既有 e2e（4 处 `run_with` 调用不变）全 PASS

- [ ] **Step 3: lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/parrot-daemon
git commit -m "feat(daemon): MCP client 接线——后台启动/确认前缀合并/状态通知转发"
```

---

### Task 8: TUI `/mcp` 命令与 McpNotice 展示；CLI 通知打印

**Files:**
- Modify: `src/tui/slash.rs`（`SlashAction` 变体 + REGISTRY + execute）
- Modify: `src/tui/app.rs`（`apply_server_message` 两个新臂 + 测试）
- Modify: `src/cli/stream.rs`（`McpNotice` 打印臂）

**Interfaces:**
- Consumes: Task 3 协议消息
- Produces: 用户可见行为——`/mcp` 发 `ListMcpServers` 并把 `McpServers` 应答渲染为 Info 条目；任何 `McpNotice` 渲染为一行 Info；CLI 流式输出打印一行。

- [ ] **Step 1: 写失败测试**

`src/tui/app.rs` `mod tests`（参照既有 `apply_server_message(ShellResult)` 测试写法，约 741 行处）：

```rust
#[test]
fn mcp_notice_renders_info_line() {
    let mut app = test_app(); // 若该测试辅助名不同，跟随文件内既有模式
    app.apply_server_message(ServerMessage::McpNotice {
        id: "playwright".into(),
        state: McpServerState::Failed,
        detail: "spawn 失败: program not found".into(),
        tool_count: 0,
    });
    let last = app.entries.last().unwrap();
    match last {
        ChatEntry::Info(text) => {
            assert!(text.contains("MCP playwright"), "got: {text}");
            assert!(text.contains("spawn 失败"), "失败详情必须可见: {text}");
        }
        other => panic!("expected Info, got {other:?}"),
    }
}

#[test]
fn mcp_servers_reply_renders_status_table() {
    let mut app = test_app();
    app.apply_server_message(ServerMessage::McpServers {
        entries: vec![parrot_protocol::types::McpServerStatusWire {
            id: "mock".into(),
            state: parrot_protocol::types::McpServerState::Connected,
            detail: String::new(),
            tool_count: 3,
        }],
    });
    match app.entries.last().unwrap() {
        ChatEntry::Info(text) => {
            assert!(text.contains("mock") && text.contains("3"), "got: {text}");
        }
        other => panic!("expected Info, got: {other:?}"),
    }
}
```

（若 `mod tests` 无现成 `test_app()` helper，按文件内其他测试构造 `App` 的方式复制。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot --bin parrot tui::app::tests::mcp -- --nocapture`
Expected: FAIL（match 非穷尽编译错误即视为失败信号）

- [ ] **Step 3: 实现**

`src/tui/slash.rs`：

```rust
pub(crate) enum SlashAction {
    Help,
    Usage,
    Abort,
    Mcp,
    Exit,
}
```

REGISTRY 追加（`abort` 之后）：

```rust
    SlashCommand {
        name: "mcp",
        description: "查看 MCP server 状态",
        action: SlashAction::Mcp,
    },
```

execute 加臂（`Abort` 臂后）：

```rust
        SlashAction::Mcp => {
            conn.sender.send(ClientMessage::ListMcpServers).await?;
            app.entries
                .push(ChatEntry::Info("已请求 MCP server 状态…".into()));
            Ok(Some(false))
        }
```

`src/tui/app.rs` `apply_server_message` match（`ShellResult` 臂后）加：

```rust
            ServerMessage::McpNotice {
                id,
                state,
                detail,
                tool_count,
            } => {
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                };
                self.entries.push(ChatEntry::Info(format!(
                    "MCP {id}: {state:?}（{tool_count} 个工具）{suffix}"
                )));
            }
            ServerMessage::McpServers { entries } => {
                if entries.is_empty() {
                    self.entries
                        .push(ChatEntry::Info("MCP: 未配置任何 server".into()));
                } else {
                    let lines: Vec<String> = entries
                        .iter()
                        .map(|e| {
                            let suffix = if e.detail.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", e.detail)
                            };
                            format!(
                                "· {} {:?}（{} 个工具）{}",
                                e.id, e.state, e.tool_count, suffix
                            )
                        })
                        .collect();
                    self.entries.push(ChatEntry::Info(lines.join("\n")));
                }
            }
```

`src/cli/stream.rs` match 加（`Error` 臂前）：

```rust
            Some(ServerMessage::McpNotice {
                id,
                state,
                detail,
                tool_count,
            }) => {
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                };
                writeln!(stdout, "\n[MCP] {id}: {state:?}（{tool_count} 个工具）{suffix}")?;
                stdout.flush()?;
            }
```

- [ ] **Step 4: 运行确认通过 + lint**

Run: `cargo test -p parrot --bin parrot` → 全 PASS
Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 5: Commit**

```bash
git add src/tui/slash.rs src/tui/app.rs src/cli/stream.rs
git commit -m "feat(tui): /mcp 状态查询与 McpNotice 提示；CLI 打印通知"
```

---

### Task 9: e2e——MCP 工具走通引擎 + 失败状态过线

**Files:**
- Modify: `tests/integration/e2e_test.rs`

**Interfaces:**
- Consumes: Task 3 协议消息、Task 7 daemon 接线（`run_with` 内部读 `config.mcp.servers`）
- Produces: 两个 e2e 用例。

- [ ] **Step 1: 追加测试**

`tests/integration/e2e_test.rs` 末尾追加（复用文件内 `MockProvider`/helpers 风格；`McpServerConfig`、`McpServerState` 从 `parrot_config` / `parrot_protocol::types` 引入）：

```rust
// ---------------------------------------------------------------------------
// E2E: MCP-qualified tool drives the full engine loop (fake McpTool in registry)
// ---------------------------------------------------------------------------

struct McpEchoProvider {
    call_count: AtomicU32,
}

impl McpEchoProvider {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for McpEchoProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo {
            id: "mock-model".to_string(),
            name: "Mock Model".to_string(),
            provider: "mock".to_string(),
            context_window: 200_000,
            max_output_tokens: 8192,
        }])
    }

    async fn chat_stream(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<ProviderStreamEvent>(16);
        tokio::spawn(async move {
            if n == 0 {
                tx.send(ProviderStreamEvent::ToolCallStart {
                    id: "tc_mcp".to_string(),
                    name: "mcp__mock__echo".to_string(),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::ToolCallDelta {
                    id: "tc_mcp".to_string(),
                    args_delta: r#"{"message":"from-mcp"}"#.to_string(),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::ToolCallEnd {
                    id: "tc_mcp".to_string(),
                    arguments: json!({"message": "from-mcp"}),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })
                .await
                .ok();
            } else {
                tx.send(ProviderStreamEvent::TextDelta {
                    delta: "done".to_string(),
                })
                .await
                .ok();
                tx.send(ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                    },
                })
                .await
                .ok();
            }
        });
        Ok(ChatStream { inner: rx })
    }

    async fn chat(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        unimplemented!()
    }
}

struct FakeMcpTool;

#[async_trait]
impl Tool for FakeMcpTool {
    fn name(&self) -> &str {
        "mcp__mock__echo"
    }
    fn description(&self) -> &str {
        "Fake MCP tool (engine-path check)"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"message": {"type": "string"}}})
    }
    async fn call(&self, arguments: Value, _ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        Ok(ToolOutput {
            content: format!(
                "mcp echo: {}",
                arguments.get("message").and_then(|v| v.as_str()).unwrap_or("")
            ),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn e2e_mcp_qualified_tool_roundtrip() {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = Arc::new(parrot_daemon::auth::Auth::new(&token_path).await.expect("auth"));
    let token = std::fs::read_to_string(&token_path).unwrap().trim().to_string();

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(Arc::new(McpEchoProvider::new()) as Arc<dyn LlmProvider>, vec!["mock-model".to_string()])
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(FakeMcpTool) as Arc<dyn Tool>)
        .await;

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("connect");

    expect_server_message(&mut conn.receiver, |m| {
        if let ServerMessage::HelloAck { .. } = m {
            Some(())
        } else {
            None
        }
    }, "HelloAck")
    .await;

    conn.sender
        .send(ClientMessage::CreateSession {
            config: Some(SessionConfig { model: None, provider: None, system_prompt: None }),
        })
        .await
        .expect("CreateSession");
    let session_id = expect_server_message(&mut conn.receiver, |m| {
        if let ServerMessage::SessionCreated { session_id } = m {
            Some(*session_id)
        } else {
            None
        }
    }, "SessionCreated")
    .await;

    conn.sender
        .send(ClientMessage::Chat { session_id, message: "call the mcp tool".into() })
        .await
        .expect("Chat");

    let (content, is_error) = expect_agent_event(&mut conn.receiver, |ev| {
        if let AgentEvent::ToolEnd { session_id: sid, result, .. } = ev {
            if *sid == session_id {
                return Some((result.content.clone(), result.is_error));
            }
        }
        None
    }, "ToolEnd(mcp)")
    .await;
    assert_eq!(content, "mcp echo: from-mcp");
    assert!(!is_error);

    daemon_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E: bad MCP command ⇒ McpNotice(Failed) broadcast + ListMcpServers shows it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_bad_mcp_server_surfaces_failure() {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = Arc::new(parrot_daemon::auth::Auth::new(&token_path).await.expect("auth"));
    let token = std::fs::read_to_string(&token_path).unwrap().trim().to_string();

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>, vec!["mock-model".to_string()])
        .await;
    let tool_registry = Arc::new(ToolRegistry::new());

    let mut config = test_config(port, &data_dir, &token_path);
    config.mcp.servers.push(parrot_config::McpServerConfig {
        id: "nope".into(),
        command: "this_binary_definitely_does_not_exist_12345".into(),
        args: vec![],
        env: Default::default(),
        startup_timeout_seconds: 30,
        call_timeout_seconds: 30,
        require_confirmation: false,
    });

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("connect");

    expect_server_message(&mut conn.receiver, |m| {
        if let ServerMessage::HelloAck { .. } = m {
            Some(())
        } else {
            None
        }
    }, "HelloAck")
    .await;

    // 等失败通知广播（start_all 在后台运行）
    let failed = expect_server_message(&mut conn.receiver, |m| {
        if let ServerMessage::McpNotice { id, state, .. } = m {
            if id == "nope" && *state == parrot_protocol::types::McpServerState::Failed {
                Some(())
            } else {
                None
            }
        } else {
            None
        }
    }, "McpNotice(Failed)")
    .await;

    conn.sender.send(ClientMessage::ListMcpServers).await.expect("ListMcpServers");
    let entries = expect_server_message(&mut conn.receiver, |m| {
        if let ServerMessage::McpServers { entries } = m {
            Some(entries.clone())
        } else {
            None
        }
    }, "McpServers")
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "nope");
    assert_eq!(entries[0].state, parrot_protocol::types::McpServerState::Failed);
    assert!(!entries[0].detail.is_empty(), "失败详情过线: {:?}", entries[0].detail);

    let _ = failed;
    daemon_handle.abort();
}
```

- [ ] **Step 2: 运行确认通过**

Run: `cargo test --test e2e mcp -- --nocapture` → 2 个 PASS
（若 `McpNotice` 断言与后台 start_all 时序竞争：`expect_server_message` 循环已有 10s 容忍，且 `Starting→Failed` 两次广播必达。）

- [ ] **Step 3: 全量验证 + commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add tests/integration/e2e_test.rs
git commit -m "test(e2e): MCP 工具走通引擎回路与失败状态过线"
```

---

### Task 10: 文档 + 全量验证收尾

**Files:**
- Modify: `AGENTS.md`（架构速查补一行）

- [ ] **Step 1: AGENTS.md 架构速查表加一行**

在 `- crates/parrot-hooks ...` 风格的列表（Architecture quick reference 段）追加：

```markdown
- `parrot-mcp` — MCP client：stdio server 生命周期（spawn/握手/崩溃热下线）+ `McpTool` 适配器（基于 rmcp）；daemon 侧 IO 组件
```

- [ ] **Step 2: 全量验证**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全绿。

- [ ] **Step 3: 手工冒烟（可选但推荐）**

```powershell
cargo run --bin parrot_mcp_mock_server --background  # 或另开终端直接运行
# parrot.toml 加 [[mcp.servers]] id="mock" command="<abs path to target\debug\parrot_mcp_mock_server>"
# 启动 parrotd + parrot，输入 /mcp 查看状态，对话让模型调用 mcp__mock__echo
```

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md
git commit -m "docs: AGENTS.md 架构速查补 parrot-mcp"
```

---

## Self-Review 记录

1. **Spec 覆盖**：§3.1 crate 布局→Task 4/5/6；§3.2 配置→Task 2；§3.3 启动流程→Task 6；§3.4 适配器+错误保真→Task 5；§3.5 确认合并→Task 7；§3.6 list_changed→Task 6；§4 UI 展示→Task 8（协议 Task 3）；§5 错误矩阵→Task 5/6（崩溃）+Task 7（转发）；§6 关闭→Task 6/7；§7 测试→Task 4/5/6/9；§8 范围外未实现（正确）。`ToolRegistry::unregister`→Task 1。
2. **占位符扫描**：Task 4 mock server 的 exit 工具保留了"若宏不便则 exit(0)"的说明与 Task 5 的占位 helper 说明——均已给出明确实现路径（提供代码 + 编译器提示微调），非 TBD。
3. **类型一致性**：`McpService`/`ListChangeNotify`/`McpTool::new`/`qualified_tool_name`/`McpManager::{subscribe,status_snapshot,shutdown}` 在 Task 5/6/7/8/9 间签名一致；`McpServerStatusWire` 字段 Task 3→6→7→9 一致。
