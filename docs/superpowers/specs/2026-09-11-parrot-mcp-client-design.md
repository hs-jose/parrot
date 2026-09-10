# Parrot MCP Client 接入(tools-only)— Design

**Date:** 2026-09-11
**Status:** Approved(brainstorm 6 轮问答 + 设计分节确认)
**Branch:** 待创建

## 1. 目标

parrotd 作为 MCP client 接入外部 MCP server 的 **tools**:工具注册进
`ToolRegistry`,对 ReAct 引擎与模型完全透明。兑现主设计文档 §4.2
「MCP 集成路径(Phase 2)」与业界调研的 Phase 2 规划。协议全景、
版本演进与 Playwright 示例见 `docs/mcp.md`,本文不重复。

## 2. 已确认的决策

| 决策点 | 选择 |
|---|---|
| MCP 能力范围 | 仅 tools(resources/prompts/sampling/elicitation/roots 不实现) |
| 传输层 | 仅 stdio(本地子进程);Streamable HTTP 留下一期 |
| 协议实现 | 官方 Rust SDK `rmcp`(v3.1.x,`client` + `transport-child-process` feature),协议版本协商交给 rmcp 自动完成 |
| 生命周期 | daemon 启动时全量启动;失败只隔离该 server,不自动重连、不懒加载 |
| 工具命名 | `mcp__<server_id>__<tool_name>`(Claude Code 同款,杜绝与内置工具/多 server 间同名冲突) |
| 调用确认 | server 级开关 `require_confirmation`,默认 `true`;实现为运行时把 `mcp__<id>__` 前缀合并进现有 ConfirmConfig,零引擎改动 |
| 失败可见性 | 不只记日志:McpNotice 推送到 UI + `/mcp` 斜杠命令随时查询 |

## 3. 架构

### 3.1 新 crate `parrot-mcp`

```
crates/parrot-mcp/
  src/
    lib.rs      — re-export start_all / McpTool
    config.rs   — McpConfig / McpServerConfig(纯 serde,无 IO)
    manager.rs  — 生命周期编排:spawn → 握手 → 枚举 → 注册 → 关闭
    adapter.rs  — McpTool:实现 parrot_core::Tool
```

- 依赖:`rmcp`(`client`、`transport-child-process`)、`parrot-core`、
  `parrot-config`、`tokio`、`serde_json`、`thiserror`、`tracing`、`async-trait`。
- **为什么不放进 parrot-tools**:MCP client 是连接器/基础设施而非工具
  实现;独立 crate 让 rmcp 依赖与子进程管理收敛一处,延续本仓库
  一个关注点一个 crate 的节奏(tools/providers/hooks 各自成 crate)。
- `parrot-daemon` 依赖 `parrot-mcp`,在 `runtime.rs` 的 `run()` 中
  `parrot_tools::register_all(...)` 之后启动(后台任务,不阻塞 WS 监听)。

### 3.2 配置设计

`AppConfig` 增加 `#[serde(default)] mcp: McpConfig`,无 `[mcp]` 段 =
零行为变化(向后兼容):

```toml
[[mcp.servers]]
id = "playwright"                   # 必填:工具名前缀 + 日志标识
command = "npx"                     # 可执行文件(或路径)
args = ["@playwright/mcp@latest"]
env = { DISPLAY = ":0" }            # 可选:附加环境变量(子进程继承 daemon 环境 + 此项)
startup_timeout_seconds = 30        # 可选,默认 30:spawn+握手+枚举 总超时
call_timeout_seconds = 120          # 可选,默认 120:单次 tools/call 超时
require_confirmation = true         # 可选,默认 true
```

### 3.3 启动流程

`parrot_mcp::start_all(...)` 由 daemon 在 `tokio::spawn` 中调用:

1. 遍历 `[[mcp.servers]]`,每 server 一个独立 tokio task:
   spawn 子进程 → `().serve(TokioChildProcess...)` 完成握手 →
   `list_all_tools()`(自动翻页)→ 逐个构造 `McpTool` 并
   `registry.register(...)`。
2. 全程受 `startup_timeout_seconds` 约束;失败 `tracing::warn!` +
   `McpNotice(failed)`,不阻断 daemon,不影响其他 server。
3. 注册前查重:`id` 重复(第二个 server 启动失败)、工具名与注册表
   已有条目冲突(跳过 + warn)。
4. 热注册语义:晚连上的工具下一 turn 自动对模型可见
   (引擎每 turn 重新 `list_definitions()`,RwLock 已天然支持)。
5. 子进程 stderr 必须有人排空(转发 `tracing`,防 chatty server
   堵死管道)。实现时核实 rmcp `TokioChildProcess` 行为,若未
   自动排空则 manager 自行读取转发。

### 3.4 McpTool 适配器

每 server 每工具一个实例,持有 service 句柄的共享克隆
(实现时核实 rmcp `RunningService` 克隆语义,不可克隆则包
一层 `Arc` proxy):

```
name()         → "mcp__playwright__browser_navigate"
description()  → server 提供的 description(缺省空串)
input_schema() → MCP inputSchema 直通;未提供时兜底 {"type":"object"}
call(args,ctx) → tokio::time::timeout(call_timeout) 包裹 call_tool(raw_name, args)
```

结果映射(MCP `content` 数组 → `ToolOutput{content, is_error}`,
ToolOutput 为 String):

| MCP 结果 | 映射 |
|---|---|
| JSON-RPC 层错误 | `is_error: true`,内容为错误描述 |
| `result.is_error == true` | `is_error: true`,content 照常回喂模型 |
| `text` 块 | 直接拼接 |
| `image`/`audio` 块 | 文本占位 `[image: <mime>, N bytes]`(不做多模态回传) |
| `resource_link`/内嵌资源 | 文本占位 + URI |

annotations(readOnlyHint 等)本期忽略——规范明确其为不可信输入,
不作放行依据;确认策略走 server 级开关。

**错误信息保真原则**:adapter 层不做泛化/吞并/转述,`ToolOutput`
一律携带底层原始错误信息,让用户与模型都能看到"到底错在哪":

| 错误来源 | ToolOutput 内容(保真要求) |
|---|---|
| JSON-RPC error `{code, message, data}` | `MCP 协议错误 code=<code>: <message>`;`data` 存在时附原始 JSON |
| rmcp `ServiceError`(响应解码失败/意外响应等) | 错误 Display 直出(thiserror 原文),不转述 |
| 传输断开 / server 进程退出 | `MCP server <id> 连接已断开(进程可能已退出)`;能取到 exit code 则附上 |
| spawn 失败 | `io::Error` 原样(kind + message,如 program not found) |
| 调用超时 | `MCP 调用超时(超过 <N>s,server 未响应)` |
| `result.isError == true` | content 全文原样保留回喂模型(server 自己写的错误详情,不加工) |

同时 `tracing::warn!` 记录完整链路细节(server id、tool 名、耗时、
底层错误),日志与 UI 各司其职。引擎层 64KB 截断兜底超长错误。

### 3.5 确认策略合并

`require_confirmation == true`(默认)→ daemon 把 `"mcp__<id>__"`
前缀合并进有效 `ConfirmConfig.require_confirmation`(与用户
`[tools.sandbox].require_confirmation` 列表拼接)。复用现有
`ToolConfirmRequired → ConfirmToolCall` 全链路与 TUI 确认 UI。

### 3.6 list_changed 通知

rmcp `ClientHandler` 回调经 channel 唤醒 manager task → 重新
`list_all_tools()` → 对比注册表:新工具 register、消失/改名
unregister。为此给 `parrot-core` 的 `ToolRegistry` 增加
`unregister(name)`(现只有 register/get/list;进行中的调用持有
`Arc<dyn Tool>`,unregister 不影响引用计数内调用)。

## 4. 失败的 UI 展示

**会话内调用失败**:已天然可见——`ToolResult{is_error}` → TUI
工具结果报错渲染,零新增。

**会话外的全局状态**(启动失败/崩溃下线),新增轻量推送通道:

1. 协议新增(`parrot-protocol`,含 roundtrip 测试):
   - `ServerMessage::McpNotice { id, state, detail, tool_count }`,
     `state ∈ { starting, connected, failed, stopped }`,daemon 主动推送
   - `ClientMessage::ListMcpServers` →
     `ServerMessage::McpServers { entries: Vec<McpServerStatusWire> }`,
     客户端主动查询(弥补连接晚收不到历史通知)
   - `McpServerStatusWire { id, state, detail, tool_count }`
2. daemon:`tokio::sync::broadcast` 通道;MCP manager 在每次状态
   迁移时发布;connection handler 建立时订阅并转发。
3. TUI:收到 `McpNotice` → 复用现有 Info 行渲染(如
   `MCP playwright 启动失败: npx 不存在`);新增 `/mcp` 斜杠命令
   弹出状态表(server、状态、工具数、失败原因)。
4. 普通 CLI:同一消息直接 print 一行。

## 5. 错误处理矩阵

| 情形 | 处理 | UI 呈现 |
|---|---|---|
| spawn 失败(命令不存在等) | warn 日志;该 server 全部工具缺席 | `McpNotice(failed)` + `/mcp` |
| 握手/枚举超时 | kill 子进程,同上 | 同上 |
| server 运行中崩溃 | manager 检测断开 → 批量 unregister 该 server 全部工具 | `McpNotice(stopped)` |
| tools/call JSON-RPC 错误 / `is_error` / 超时 | 映射 `ToolOutput{is_error:true}` | 现有工具结果报错 |
| tools/call 期间 server 死亡 | 同上,内容「MCP server \<id\> 不可用」 | 同上 |
| 引擎 abort | 现有 `race_with_abort` 竞速覆盖:Abort 胜出 → call future 在 await 点丢弃 → 合成 aborted ToolEnd;MCP 子进程不随单次调用退出,生命周期归 manager | — |

## 6. 关闭顺序

Ctrl+C / SIGTERM → 现有 `SessionManager.shutdown_all` → MCP manager
shutdown:对每 server `client.cancel()` + kill 子进程
(process-wrap drop 兜底,防孤儿进程)→ 退出。

## 7. 测试策略

1. **单测**(parrot-mcp 内,无 IO 部分):config 解析/默认值、
   工具名构造、content 拍平映射(text/image/resource_link 各 case)。
   注:parrot-mcp 本身是有 IO 的 crate,「无 IO」仅指其中纯函数。
2. **集成测试**(parrot-mcp):用 rmcp 的 server 侧(同一依赖,零新增)
   写 mock MCP server fixture(暴露 echo + fail 两工具),stdio 驱动
   完整链路:spawn→握手→注册→call→list_changed 增删→崩溃 unregister。
3. **e2e**(repo tests/):复用现有 mock provider 基建——配置 mock
   MCP server → 起 daemon → CreateSession → Chat 中模型调用
   `mcp__mock__echo` → 断言结果回流 + 失败工具报错。
4. **协议 roundtrip**:新消息加入 `crates/parrot-protocol/tests/roundtrip.rs`。

## 8. 范围外(本期明确不做)

- Streamable HTTP / OAuth
- resources / prompts / sampling / elicitation / roots
- 崩溃自动重连、懒加载
- 图片等多模态内容回传
- `/mcp` 之外的 server 管理操作(运行时启停 server 等)
