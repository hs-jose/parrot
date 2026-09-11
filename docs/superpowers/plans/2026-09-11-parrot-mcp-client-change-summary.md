# MCP Client 接入（tools-only）— 变更说明

**Branch:** `feat/mcp-client`（基于 main `8957f60`，11 个 commit，+2212/-41）
**Spec:** `docs/superpowers/specs/2026-09-11-parrot-mcp-client-design.md`
**Plan:** `docs/superpowers/plans/2026-09-11-parrot-mcp-client.md`
**验证状态:** workspace 全部 32 个 suite 0 失败；clippy `-D warnings` / fmt clean

## 1. 交付了什么

parrotd 作为 MCP client，接入外部 stdio MCP server 的 **tools**：工具注册进
`ToolRegistry`，对 ReAct 引擎与模型完全透明。失败状态不只记日志——
`McpNotice` 推送到 UI + `/mcp` 斜杠命令随时查询。协议版本协商、JSON-RPC
收发、子进程管理由官方 Rust SDK **rmcp 3.3.0** 承担。

本期明确不做（spec §8）：HTTP 传输/OAuth、resources/prompts/sampling、
自动重连、懒加载、多模态回传。

## 2. 核心设计决策

| 决策点 | 选择 |
|---|---|
| 能力范围 | 仅 tools；stdio 传输 |
| 协议实现 | rmcp 3.3.0（client + transport-child-process + which-command） |
| 生命周期 | daemon 启动时全量启动（后台 task，不阻塞 WS 监听）；失败隔离不重连 |
| 工具命名 | `mcp__<server_id>__<tool_name>`（杜绝与内置工具冲突） |
| 调用确认 | server 级开关默认 true，运行时把 `mcp__<id>__` 前缀合并进现有 ConfirmConfig，零引擎改动 |
| 崩溃检测 | stdout EOF 事件驱动（见 §5 偏差 1）+ 1s 轮询兜底 |
| 错误保真 | JSON-RPC `code`+`message`+`data` 原样；`isError` content 全文回喂；超时/断连/spawn 失败各有明确文案 |

## 3. 按组件的变更清单

### 新增 crate `crates/parrot-mcp/`（daemon 侧 IO 组件）

| 文件 | 内容 |
|---|---|
| `src/adapter.rs` | `McpTool`（实现 `parrot_core::Tool`）：`input_schema` 直通、`call` → `tools/call`（外层 tokio 超时 + 读锁共享 rmcp service）；纯函数 `qualified_tool_name` / `flatten_content`（text 拼接、image/audio 占位、resource_link/embedded 占位）/ `map_service_error`（错误保真文案）；`ListChangeNotify`（rmcp ClientHandler，转发 `tools/list_changed`） |
| `src/manager.rs` | `start_all(registry, servers) -> McpManager`（非阻塞）：每 server 一个 tokio task——spawn → 握手+枚举（整体受 `startup_timeout_seconds`）→ 查重注册 → monitor（EOF 崩溃信号 + 1s 轮询 + `list_changed` 重枚举整组换工具）；`status_snapshot`（`/mcp` 查询）、`shutdown`（close 5s 兜底 → 批量 unregister → Stopped 通知） |
| `src/bin/mock_server.rs` | 开发/测试用 mock MCP server（`parrot_mcp_mock_server`）：echo/fail/exit 工具 + `extend`（动态加工具并广播 list_changed），可配到 parrot.toml 手工验证 |

### `crates/parrot-config/`

- `McpConfig` / `McpServerConfig`（id、command、args、env、startup_timeout_seconds=30、call_timeout_seconds=120、require_confirmation=true）
- `AppConfig.mcp` 带 `#[serde(default)]`——无 `[mcp]` 段零行为变化

### `crates/parrot-protocol/`

- `types.rs`：`McpServerState { starting, connected, failed, stopped }`、`McpServerStatusWire`
- `server_message.rs`：`McpNotice`（daemon 推送）、`McpServers`（查询应答）
- `client_message.rs`：`ListMcpServers`
- roundtrip 测试 ×3

### `crates/parrot-core/`

- `ToolRegistry::unregister(name)`：热下线（MCP server 崩溃/关闭时批量移除）；进行中调用持有的 `Arc<dyn Tool>` 不受影响（有测试锁定）

### `crates/parrot-daemon/`

- `runtime.rs`：`run_with` 系列内部经 `start_all` 启动 MCP；`require_confirmation` 前缀合并；鉴权成功后才订阅广播并转发 `McpNotice`（未鉴权对端收不到）；`Lagged` 不中断转发；`ListMcpServers` 真实应答（替换 Task 3 的占位 arm）；关闭顺序 sessions → MCP → exit

### `src/tui/` + `src/cli/`

- `/mcp` 斜杠命令：发送查询、状态表渲染（server、状态、工具数、失败原因）
- 任何 `McpNotice` → Info 行提示；CLI 流式输出打印一行

### 测试与文档

- `tests/integration/e2e_test.rs` ×2：MCP 工具走通引擎回路；坏 server 失败状态过线（广播+查询）
- `crates/parrot-mcp/tests/lifecycle.rs` ×5：spawn→握手→注册→调用→错误保真→崩溃热下线→优雅关闭→失败隔离→list_changed 增删→重复/空 id 可见
- `crates/parrot-mcp/tests/adapter_tool.rs` ×3：Tool trait 语义、isError 全文保真、未知工具协议错误带 code
- AGENTS.md 架构速查补 parrot-mcp 一行

## 4. 用户使用方式

```toml
[[mcp.servers]]
id = "playwright"
command = "npx"
args = ["@playwright/mcp@latest"]
# 可选: env / startup_timeout_seconds / call_timeout_seconds / require_confirmation
```

模型侧即插即用：注册后下一 turn 自动可见（引擎每 turn 重枚举）；
工具名形如 `mcp__playwright__browser_navigate`；默认每次调用需确认
（复用现有确认弹窗），只读 server 可设 `require_confirmation = false`。
`/mcp` 随时查看状态。手工验证：把 command 指到
`target/debug/parrot_mcp_mock_server`。

## 5. 计划外偏差（均已独立核实）

1. **崩溃检测机制**：计划假设 rmcp `is_closed()` 可轮询检测子进程死亡——
   源码核实不成立（service 循环在传输 EOF 时以 `QuitReason::Closed` 退出
   且不取消 token）。改为 `CrashWatchTransport` 在 stdout EOF 时事件驱动
   信号 monitor，1s 轮询保留为兜底。结果契约不变（崩溃 → 批量
   unregister + Stopped 通知）。
2. **e2e 失败用例形态**：broadcast 无订阅者时消息被丢——start_all 早于
   WS accept 广播 Starting→Failed 会丢。测试改用 `startup_timeout_seconds: 2`
   的挂起 server 让 Failed 落在订阅之后；spawn 失败味型仍由 lifecycle
   测试覆盖；`/mcp` 查询是 spec 设计的晚订阅补偿路径。

## 6. 已知限制（deferred，非阻塞）

- 重复 id 且一个成功一个失败时，同 key 状态互相覆盖（`/mcp` 只显示一条）
- shutdown abort 窗口可孤儿已注册工具（daemon 退出场景无害）
- 孙进程继承 stdout 时 EOF 不触发（stdio-MCP 固有限制）
- 部分 Minor 分支未单测（timeout 文案、Audio/Resource 拍平、空 entries Info）
- CI 时序余量：e2e 失败用例依赖 client 在 2s 内完成订阅（有 13× 余量，必要时可调 5s）

## 7. 提交列表

```
6e988c4 fix(mcp): 补 list_changed 覆盖与鉴权门控通知，重复 id 失败可见
e0953ae docs: AGENTS.md 架构速查补 parrot-mcp
67d0d16 test(e2e): MCP 工具走通引擎回路与失败状态过线
0266450 feat(tui): /mcp 状态查询与 McpNotice 提示；CLI 打印通知
741d20b feat(daemon): MCP client 接线——后台启动/确认前缀合并/状态通知转发
d75abd2 feat(mcp): MCP server 生命周期管理与崩溃热下线
72add8e feat(mcp): McpTool 适配器——Tool trait 到 rmcp tools/call 的映射
7347a06 feat(mcp): parrot-mcp crate 骨架、mock server 与纯映射函数
4331bbc feat(protocol): MCP 状态通知与查询消息
2509480 feat(config): McpConfig 与 [[mcp.servers]] 配置段
6b32075 feat(core): ToolRegistry::unregister 支持工具热下线
```
