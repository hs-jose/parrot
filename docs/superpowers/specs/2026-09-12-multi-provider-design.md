# 多 Provider 接入设计（OpenAI 兼容协议）

日期：2026-09-12
状态：已批准

## 1. 背景与目标

Parrot 目前只有 Anthropic 一个 LLM provider 实现。`parrot-core` 已预留 `LlmProvider` trait 与 `ProviderRegistry`（按 model 路由），`parrot-config` 已支持 `[[providers]]` 段，`parrot-providers/src/lib.rs` 中 `"openai"` 分支是占位 warn。

本需求：接入多种 provider，采用 **OpenAI 兼容协议（标准 Chat Completions）** 作为第二协议实现。因只依赖 `base_url + api_key`，openai.com / DeepSeek / Kimi / vLLM / Ollama 等所有 Chat Completions 兼容服务开箱即用。

同时将现有 Anthropic provider 对齐新标准（主要是 `list_models` 远端拉取）。

范围（已确认）：
- 流式 + 并行工具调用（ReAct 引擎必需）
- 非流式请求（`chat()`）
- 重试机制（复用 `retry.rs`）
- `list_models` 远端拉取（OpenAI 与 Anthropic 均对齐）
- 仅标准 Chat Completions，不做各家特殊字段（如 `reasoning_content`）容错

非目标：
- 不抽象通用 HTTP provider 框架（YAGNI，目前只有两种协议）
- 不做 Gemini 原生协议、自定义网关

## 2. 现状盘点

### 2.1 架构速查

- `parrot-core/src/provider.rs`：`LlmProvider` trait（`provider_id` / `list_models` / `chat_stream` / `chat`）、`ProviderStreamEvent`（TextDelta / ToolCallStart / ToolCallDelta / ToolCallEnd / Finish）、`ProviderStopReason`（EndTurn / ToolUse / MaxTokens）、`ProviderRegistry`（model→provider 映射）
- `parrot-providers/src/anthropic.rs`：现有实现，SSE 解析完整，`list_models` 硬编码 3 个模型
- `parrot-providers/src/retry.rs`：指数退避+抖动，429 尊重 retry-after，流开始后不重试
- `parrot-config/src/config.rs`：`ProviderConfig { id, api_key, default_model, base_url?, models }`
- `parrot-daemon/src/runtime.rs` `collect_models()`：聚合各 provider 的 `list_models()`
- `parrot-core/src/engine.rs` `resolve_model_context_window()`：用 `list_models` 查 context window

### 2.2 Anthropic 差距

| 能力 | 现状 | 差距 |
|---|---|---|
| 流式+工具调用 | ✅ | 无 |
| 非流式 | ✅ | 无 |
| 重试 | ✅ | 无 |
| `list_models` | 硬编码（anthropic.rs:238） | 不拉远端；构造函数 `_default_model` 被丢弃 |

## 3. 设计

### 3.1 配置（parrot-config）

- `ProviderConfig` 增加 **必填** 字段 `protocol: String`，取值 `"anthropic"` | `"openai"`（OpenAI 兼容）。缺省时 TOML 反序列化直接报错，迫使配置显式声明协议。
- `id` 只做唯一标识（provider_id、日志、model 路由），不再承担协议分发职责。
- `api_key` 加 `#[serde(default)]`——Ollama 等本地服务无需密钥，空字符串表示免鉴权。
- 其余字段不变。多 provider 即多个 `[[providers]]` 条目。

```toml
[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-sonnet-4-6"
models = ["claude-sonnet-4-6", "claude-opus-4"]

[[providers]]
id = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
default_model = "deepseek-chat"

[[providers]]
id = "claude-proxy"
protocol = "anthropic"
base_url = "https://my-proxy.example.com"
api_key = "${KEY}"
default_model = "claude-sonnet-4-6"

[[providers]]
id = "ollama"
protocol = "openai"
base_url = "http://localhost:11434/v1"
default_model = "qwen3"
models = ["qwen3", "llama4"]
```

**兼容性说明**：`protocol` 必填意味着现有配置文件与打包模板需同步补上该字段（仓库内模板与测试 fixture 一起改）。这是有意的破坏性变更——显式协议声明避免 id 推断的歧义（如给 Anthropic 代理配自定义 id 时会被误路由）。

注意：OpenAI 官方端点是 `https://api.openai.com/v1`（`base_url` 需含 `/v1`，适配器不再追加）；Anthropic 保持 `base_url` 不含 `/v1`（适配器内拼 `/v1/messages`），维持现有行为不变。

### 3.2 OpenAI 兼容适配器（parrot-providers/src/openai.rs，新文件）

```rust
pub struct OpenAiProvider {
    provider_id: String,   // 即配置里的 id（"openai"/"deepseek"/…）
    api_key: String,       // 可为空
    base_url: String,
    config_models: Vec<String>, // 配置的 models / default_model，list_models 失败时兜底
    client: reqwest::Client,
}
```

实现 `LlmProvider`：

**端点**
- `POST {base_url}/chat/completions`（`"stream": true` 流式 / 省略或 false 非流式）
- `GET {base_url}/models`

**鉴权**
- `Authorization: Bearer <key>`；`api_key` 为空则整个 header 省略（Ollama）。

**消息映射**（`ChatMessage` ↔ openai messages）
- `ChatRole::System` → `{"role":"system","content":...}`（原样发送，不抽取 system prompt——与 anthropic 的差异点，Chat Completions 标准做法）
- `ChatRole::User` → `{"role":"user","content":...}`
- `ChatRole::Assistant` 带 `tool_calls` → `{"role":"assistant","content":..., "tool_calls":[{"id","type":"function","function":{"name","arguments":JSON字符串}}]}`；`arguments` 需从 `serde_json::Value` 序列化为字符串
- `ChatRole::Tool` → `{"role":"tool","tool_call_id":...,"content":...}`（每条 Tool 消息独立一条，不合并）

**工具映射**（`ToolDefinition` → tools）
- `{"type":"function","function":{"name","description","parameters":input_schema}}`

**SSE 流解析**（对齐 `ProviderStreamEvent`）
- `choices[0].delta.content` → `TextDelta`
- `choices[0].delta.tool_calls[i]`：按 `index` 聚合。首片（带 id/name）→ `ToolCallStart`；`arguments` 分片拼接 → `ToolCallDelta`；同一 index 结束（出现新 index 或流结束/finish_reason 到达）→ `ToolCallEnd`（arguments 累积串解析为 JSON，失败按空对象）
- `choices[0].finish_reason`：`"tool_calls"`→ToolUse；`"stop"`→EndTurn；`"length"`→MaxTokens；其他→EndTurn
- `usage`：非流式从响应体取；流式从带 `usage` 的 chunk 取（OpenAI 兼容服务在最后一个 chunk 附带，`stream_options.include_usage` 不强求——取不到则 0/0）
- 流中错误（非 2xx / JSON `error` 字段）→ `ProviderError::StreamError` 上报，与 anthropic 行为一致

**非流式**
- 响应 `choices[0].message`：`content` → 文本；`tool_calls` → `ToolCallInfo` 列表（`arguments` 字符串反序列化为 Value）
- `finish_reason` → stop reason（仅 usage 记录用，`chat()` 返回 `ChatMessage`）

**重试**
- 复用 `crate::retry::with_retry`，语义与 anthropic 相同：429 尊重 retry-after、5xx/网络错误退避重试、流开始后不重试（`ProviderError::StreamError` 不重试）。

**list_models**
- `GET {base_url}/models`，解析 `{"data":[{"id",...}]}`；`ModelInfo` 的 `context_window`/`max_output_tokens` 未知填 0（引擎 `resolve_model_context_window` 对 0 视为未知处理，见 §3.5）
- 失败 fail-open：回退 `config_models`（再兜底空列表）。`name` 取 id 原值。

### 3.3 register_all 接线（parrot-providers/src/lib.rs）

```rust
match provider_config.protocol.as_str() {
    "anthropic" => /* AnthropicProvider::new(api_key, base_url, models, id) */,
    "openai" => /* OpenAiProvider::new(id, api_key, base_url, models) */,
    other => tracing::warn!("Unknown protocol: {}, skipping", other),
}
```

- 删除 `"openai" => warn not implemented` 占位与按 id 分发的 `unknown provider id` warn。
- `protocol` 决定适配器，`id` 作为 `provider_id` 传入构造函数（anthropic 适配器的 `provider_id()` 也从硬编码 `"anthropic"` 改为返回配置 id，支持 claude-proxy 场景）。
- 未知 `protocol` 值在注册时 warn 并跳过（配置错误的暴露点提前到 daemon 启动日志，而不是首次请求时）。

### 3.4 Anthropic 对齐（anthropic.rs 小改）

- 构造函数签名改为 `new(api_key, base_url, models: Vec<String>)`（替换被丢弃的 `_default_model: String`），保存为 `config_models`。
- `list_models`：
  1. `GET {base_url}/v1/models`（带 `x-api-key` / `anthropic-version` header，`limit=1000`，`after_id` 分页追平）
  2. 解析 `{"data":[{"id","display_name",...}]}`
  3. 失败 fail-open：回退 `config_models`；`config_models` 也为空则回退当前硬编码 3 模型（保持现状兜底）
- 其余（请求构造、SSE 解析、重试）不动。

### 3.5 引擎 context window 语义

`ModelInfo.context_window = 0` 表示未知。`engine.rs resolve_model_context_window` 现有逻辑 `find(...).map(...)` 会把 0 当真实预算传给压缩模块——需加一层过滤：`filter(|m| m.context_window > 0)`，0 视为「未收录」保持配置预算不变。

### 3.6 错误处理

与 anthropic 完全一致（跨 crate 边界仍是 `ProviderError`，`thiserror`）：
- 429 → `RateLimited { retry_after_ms }`（读 `retry-after` header）
- 4xx/5xx → `Api { status, body }`
- 网络/超时 → `Network` / `Timeout`
- 流中 → `StreamError`

## 4. 测试策略

- **openai.rs 单测**（`#[cfg(test)]`）：
  - 消息映射：system 原样保留、assistant+tool_calls（Value→字符串）、tool 独立成条
  - 工具映射：ToolDefinition→function 格式
  - SSE 解析：文本 delta、并行 tool_calls 分片聚合（多 index 交错）、finish_reason 三态映射、usage 提取、error 事件
  - 空 api_key 不发 Authorization header（mock 或构造层断言）
- **anthropic list_models 单测**：远端成功解析、分页、失败回退 config_models、再兜底硬编码
- **config 测试**：`api_key` 缺省时解析成功；`protocol` 缺失时解析失败（必填）；多 `[[providers]]` 共存
- **lib.rs 接线测试**：`protocol = "openai"` 注册为 OpenAiProvider、`protocol = "anthropic"` 注册为 AnthropicProvider、未知 protocol 跳过（可用 registry 查询 provider_id 验证）
- **既有配置 fixture**：仓库内 parrot.toml 模板与测试用 TOML 全部补 `protocol` 字段
- **全量**：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`

## 5. 文件清单

| 文件 | 动作 |
|---|---|
| `crates/parrot-providers/src/openai.rs` | 新增：OpenAI 兼容适配器 |
| `crates/parrot-providers/src/lib.rs` | 修改：register_all 按 protocol 分发 |
| `crates/parrot-providers/src/anthropic.rs` | 修改：构造函数签名（含 provider_id）+ list_models 远端拉取 |
| `crates/parrot-config/src/config.rs` | 修改：`protocol` 必填字段 + `api_key` serde default |
| `crates/parrot-core/src/engine.rs` | 修改：context_window=0 过滤 |
| 仓库内 parrot.toml 模板 / 测试 TOML fixture | 修改：补 `protocol` 字段 |
| 设计文档 §3 表格 | 无需更新（StreamEvent 映射未变） |
