# MCP（Model Context Protocol）能力全景

> 本文是 MCP 协议的完整介绍：是什么、有哪些能力、协议长什么样，最后以 Playwright MCP server 为例走一遍完整流程。面向后续 Parrot 接入 MCP 的设计与开发（当前决策：代码先只支持 **tools**）。

## 1. MCP 是什么

MCP 是一个开放协议，标准化"LLM 应用 ↔ 外部数据源/工具"之间的连接方式。类比 LSP（Language Server Protocol）之于编辑器与语言：任何 MCP server 都能即插即用地接入任何 MCP host（如 Claude Desktop、Cursor、Parrot）。

三个角色：

```
┌───────────────────────────── Host（宿主） ─────────────────────────────┐
│  例如 Claude Desktop / Cursor / parrotd                                │
│                                                                        │
│  ┌────────┐   JSON-RPC 2.0   ┌──────────────┐                          │
│  │ Client │ ◄──────────────► │  MCP Server  │  本地进程(stdio)          │
│  │  (A)   │                  │  filesystem  │  或远程服务(Streamable    │
│  └────────┘                  └──────────────┘  HTTP)                   │
│  ┌────────┐                  ┌──────────────┐                          │
│  │ Client │ ◄──────────────► │  MCP Server  │                          │
│  │  (B)   │                  │  playwright  │                          │
│  └────────┘                  └──────────────┘                          │
└────────────────────────────────────────────────────────────────────────┘
```

- **Host**：用户使用的 LLM 应用，负责编排、安全、UI。内部为每个 server 维护一个 **Client** 实例（1:1 连接）。
- **Server**：暴露能力的小型服务（本地子进程或远程 HTTP 服务），每个聚焦一类职责。
- **协议载体**：JSON-RPC 2.0，有状态连接 + 能力协商。

## 2. 规格版本与传输层

| 版本 | 状态 | 关键变化 |
|---|---|---|
| 2024-11-05 | Legacy | 首个稳定版；HTTP+SSE 传输 |
| 2025-03-26 | — | 引入 Streamable HTTP（替代 HTTP+SSE） |
| **2025-06-18** | **Stable（生态主流）** | 移除 JSON-RPC batching；结构化工具输出；elicitation；OAuth 资源服务器分类 |
| 2025-11-25 | Latest Stable | URL 模式 elicitation；icons 元数据；实验性 Tasks；sampling 工具调用 |
| 2026-07-28 | Latest | 转向无状态自包含请求、多轮请求（MRTR）、订阅/监听式通知 |

传输层两种：

- **stdio**：host 启动 server 子进程，stdin/stdout 收发 JSON-RPC 行，stderr 供日志。本地 server 的标准方式，简单可靠。
- **Streamable HTTP**：client 向 server 的单个 HTTP 端点 POST 请求，响应可以是普通 JSON 或 SSE 流；用于远程 server，配合 OAuth 2.1 鉴权。

## 3. 生命周期与能力协商

以 stdio 为例，client 启动子进程后先握手：

```jsonc
// ① Client → Server: initialize
{
  "jsonrpc": "2.0", "id": 0,
  "method": "initialize",
  "params": {
    "protocolVersion": "2025-06-18",
    "capabilities": { "roots": { "listChanged": true } },   // client 声明自己支持什么
    "clientInfo": { "name": "parrot", "version": "0.1.0" }
  }
}

// ② Server → Client: 返回自己的能力
{
  "jsonrpc": "2.0", "id": 0,
  "result": {
    "protocolVersion": "2025-06-18",
    "capabilities": { "tools": { "listChanged": true } },
    "serverInfo": { "name": "playwright", "version": "0.0.40" }
  }
}

// ③ Client → Server: notifications/initialized（通知，无 id），会话进入工作状态
```

双方此后只能使用各自声明过的能力。会话结束时 client 关闭 stdin / 断开 HTTP，子进程随之退出。

## 4. Server 能力（server 提供给 host 的三大原语）

### 4.1 Tools —— 模型控制的"动作"

**控制权：模型（AI 决定何时调用）**，host 负责把关（人确认）。这是与 LLM function calling 的桥梁。

- `tools/list`：枚举工具（游标分页），每个工具含 `name` / `title` / `description` / `inputSchema`（JSON Schema）/ `annotations`。
- `tools/call`：执行工具，参数为任意 JSON 对象。
- 变更通知：server 声明 `tools.listChanged` 后，工具集变化时发 `notifications/tools/list_changed`，host 应重新 list。
- **结果内容**（`content` 数组）：`text` / `image` / `audio` / `resource_link` / 内嵌 resource。
- **执行失败语义**：`isError: true` 表示工具跑了但业务失败（内容应回喂给模型）；JSON-RPC 层错误才是协议故障。
- **结构化输出**（2025-06-18+）：工具可声明 `outputSchema`，结果带 `structuredContent`。
- **annotations**：供 host 做策略判断的提示（不可信，仅供参考）：`readOnlyHint`、`destructiveHint`、`idempotentHint`、`openWorldHint`、`title`。

### 4.2 Resources —— 应用控制的"数据"

**控制权：应用/用户**（host 决定何时读取、如何呈现，用户可以浏览选择），不是模型自动调用。适合暴露文件、数据库行、API 响应等上下文。

- `resources/list` / `resources/read`：枚举与读取；URI 标识（如 `file:///…`、`postgres://…`、自定义 scheme）。
- `resources/templates/list`：参数化资源模板（URI Template，如 `file:///{path}`）。
- 订阅：`resources/subscribe` → `notifications/resources/updated`。
- 内容可为文本或 base64 二进制，带 `mimeType`。

### 4.3 Prompts —— 用户控制的"模板"

**控制权：用户**（用户显式选择使用某个 prompt，如斜杠命令）。server 提供带参模板，host 填参后得到一组 messages 注入对话。

- `prompts/list` / `prompts/get`（参数 + 支持参数自动补全 `completion/complete`）。
- 典型用途：`/review` 代码评审模板、`/explain` 解释模板——server 侧定义，所有 host 通用。

## 5. Client 能力（host 提供给 server 的三个反向原语）

server 在会话中可以**反向请求** host：

| 能力 | 方向 | 用途 |
|---|---|---|
| **Sampling** `sampling/createMessage` | server → host 的 LLM | server 请求用 host 的模型生成内容（如"帮我总结刚才抓的网页"）。用户必须逐次审批，server 只能看到被允许的最小 prompt |
| **Roots** `roots/list` | server → host | server 询问 host 的文件系统/URI 边界（"你允许我操作哪些目录？"），host 可声明 `listChanged` |
| **Elicitation** `elicitation/create` | server → 用户 | server 运行中向用户要补充信息（表单模式：JSON Schema 定义字段；2025-11-25 起还有 URL 模式：让用户去网页授权） |

## 6. 通用机制

- **分页**：list 类请求用 `cursor` / `nextCursor`。
- **进度**：请求 `_meta.progressToken` → server 发 `notifications/progress`（适合长任务）。
- **取消**：`notifications/cancelled`（stdio）；HTTP 上关闭 SSE 流即取消。
- **ping**：任一方可 `ping` 探活。
- **日志**：`logging/setLevel` + `notifications/message`。
- **`_meta`**：所有接口类型的扩展元数据字段（协议版本、progressToken 等）。
- **Tasks**（2025-11-25 实验性）：长时间运行操作的任务化跟踪。

## 7. 安全与信任模型

MCP 规范反复强调的安全基线，Parrot 接入时也要遵守：

1. **用户知情同意**：所有数据访问与工具调用需用户明确同意——工具描述/annotations 属于**不可信输入**（server 可能被提示注入污染），host 要基于用户审批而不是描述文本放行。
2. **数据隐私**：未经用户同意不把用户数据发给 server；不把一个 server 的资源转给另一个。
3. **工具安全**：工具即任意代码执行。host 应提供调用前确认 UI（Parrot 已有 `ConfirmToolCall` 流程，天然契合）。
4. **Sampling 审批**：server 发起的每次采样都需用户批准。

## 8. 现实示例：Playwright MCP 全流程

Playwright 官方维护的 MCP server（`@playwright/mcp`），让模型驱动浏览器：导航、截图、点击、填表单。Claude Code / Cursor 等的配置形如：

```json
{ "mcpServers": { "playwright": { "command": "npx", "args": ["@playwright/mcp@latest"] } } }
```

下面是 wire 上的完整过程（stdio 传输，每行一个 JSON-RPC 消息）：

```jsonc
// host 启动子进程 npx @playwright/mcp@latest，然后：

// ① 握手
→ {"jsonrpc":"2.0","id":0,"method":"initialize","params":{
     "protocolVersion":"2025-06-18",
     "capabilities":{},
     "clientInfo":{"name":"parrot","version":"0.1.0"}}}
← {"jsonrpc":"2.0","id":0,"result":{
     "protocolVersion":"2025-06-18",
     "capabilities":{"tools":{"listChanged":true}},
     "serverInfo":{"name":"Playwright MCP","version":"0.0.40"}}}
→ {"jsonrpc":"2.0","method":"notifications/initialized"}

// ② 枚举工具（真实工具集节选，共 20+ 个）
→ {"jsonrpc":"2.0","id":1,"method":"tools/list"}
← {"jsonrpc":"2.0","id":1,"result":{"tools":[
     {"name":"browser_navigate",
      "title":"Navigate to a URL",
      "description":"Navigate to a URL",
      "inputSchema":{"type":"object",
        "properties":{"url":{"type":"string","description":"URL to navigate to"}},
        "required":["url"]},
      "annotations":{"readOnlyHint":true,"openWorldHint":true}},
     {"name":"browser_click",
      "title":"Click",
      "description":"Perform click on a web page",
      "inputSchema":{"type":"object",
        "properties":{"element":{"type":"string","description":"Human-readable element description"},
                      "ref":{"type":"string","description":"Exact target element reference from the snapshot"}},
        "required":["element","ref"]}},
     {"name":"browser_snapshot",
      "title":"Page snapshot", ...},
     {"name":"browser_take_screenshot",
      "title":"Take a screenshot", ...}
   ]}}

// ③ 模型决定调用工具，host 经 tools/call 转发
→ {"jsonrpc":"2.0","id":2,"method":"tools/call",
   "params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}
← {"jsonrpc":"2.0","id":2,"result":{
     "content":[{"type":"text","text":"Navigated to https://example.com"}],
     "isError":false}}

// ④ 工具集变化时（如新开标签页多了 browser_tab_select 工具）
← {"jsonrpc":"2.0","method":"notifications/tools/list_changed"}
→ {"jsonrpc":"2.0","id":3,"method":"tools/list"}
```

一个工具调用在 Parrot 中的对应关系（tools-only 接入的目标形态）：

```
用户: "打开 example.com 截个图"
  ↓ ReAct 引擎（tool_registry.list_definitions() 已含 mcp__playwright__* 工具）
模型 → tool_call: mcp__playwright__browser_navigate {url}
  ↓ Parrot 的 McpTool 适配器（实现 parrot-core 的 Tool trait）
  ↓ 转成 MCP tools/call → playwright 子进程 → 结果映射回 ToolOutput
模型拿到结果 → 继续调用 mcp__playwright__browser_take_screenshot → 汇答用户
```

其他常见 server 速览：`@modelcontextprotocol/server-filesystem`（文件读写工具 + 资源）、`@modelcontextprotocol/server-github`（PR/issue 工具）、各类数据库 server（Postgres/SQLite：schema 作资源、查询作工具）。

## 9. 与 Parrot 的映射速查

| MCP 概念 | Parrot 对应物 |
|---|---|
| Host | `parrotd`（daemon 是唯一 IO 组件，MCP client 逻辑天然属于 daemon 侧） |
| Client（每 server 一个） | 新增 MCP 模块：生命周期管理 + JSON-RPC 收发 |
| Tools → `Tool` trait | `McpTool` 适配器：`input_schema` 直通 `inputSchema`，`call` 映射 `tools/call` → `ToolOutput{content, is_error}` |
| 工具注册 | `ToolRegistry`（`RwLock<HashMap>`，支持运行时热注册——server 晚连上时工具下一 turn 自动可见） |
| 工具调用确认 | 现有 `ConfirmToolCall` 流程（`require_confirmation` 前缀匹配，如 `mcp__`） |
| 配置 | `parrot.toml` 新增 `[mcp]` 段（参照 `[[hooks.external]]` 的 `command` 数组模式） |
| Prompts / Resources / Sampling / Elicitation / Roots | 本期不实现，见上文 §4.2–§5 |
