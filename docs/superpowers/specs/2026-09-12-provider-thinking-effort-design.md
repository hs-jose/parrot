# Provider thinking / effort 生效设计

日期：2026-09-12
状态：已批准
前置：`docs/superpowers/specs/2026-09-12-multi-provider-design.md` §3.1 扩展点（本期仅解析保存）

## 1. 目标

让 `ModelEntry` 中预留的 `thinking`（anthropic 协议）与 `reasoning_effort`（openai 协议）真正生效——请求期注入。未配置 ⇒ 请求体零新字段，行为不变。

## 2. 设计

### 2.1 注入机制（无接口变更）

适配器持有 `config_models: Vec<ModelEntry>`，`build_request` 前按 `model` 自行查找：

`parrot-providers/src/models.rs` 新增共享纯函数：

```rust
pub fn find_detailed<'a>(entries: &'a [ModelEntry], model: &str) -> Option<&'a DetailedModelEntry>
```

- `Simple(String)` 条目无元数据，永不命中
- 仅 `Detailed` 条目按 `id` 精确匹配

### 2.2 Anthropic thinking 注入

`build_request` 增加 `thinking: Option<&ThinkingConfig>` 参数：

- `Some` ⇒ 请求体加 `thinking: {"type": "enabled", "budget_tokens": N}`（`skip_serializing_if`）
- **不自动抬高 max_tokens**（用户决策 #2）：Anthropic 要求 `max_tokens > budget_tokens`，违反时 API 报错上抛（`ProviderError::Api`），配置责任归用户——需在配置侧保证 max_tokens 足够

### 2.3 OpenAI reasoning_effort 注入

`OpenAiRequest` 增加 `#[serde(skip_serializing_if = "Option::is_none")] reasoning_effort: Option<String>`（序列化为 `"low"|"medium"|"high"`）；`build_request` 增加 `reasoning_effort: Option<&ReasoningEffort>` 参数。非推理模型收到该字段会报错——同为用户配置责任（配在错误模型上=显式错误暴露）。

### 2.4 register_all 交叉校验

注册时检查 protocol 与字段匹配：

- `protocol = "anthropic"` 且任一 Detailed 条目配了 `reasoning_effort` ⇒ warn「reasoning_effort 仅 openai 协议生效，已忽略」
- `protocol = "openai"` 且任一条目配了 `thinking` ⇒ 同理 warn
- 仅 warn，照常注册（适配器天然忽略不属于自己的字段）

### 2.5 非目标

- **thinking 输出流不显示**：anthropic SSE 的 `thinking_delta`/`signature_delta` 被现有解析忽略（unknown delta type 分支），思考内容不出现在 UI——独立需求
- 非 openai 协议的 effort 变体（Qwen `enable_thinking` 等）不做
- 不做运行时切换

## 3. 测试策略

- models.rs：`find_detailed` 命中/未命中/Simple 条目不命中
- anthropic：带 thinking 的请求体序列化断言（含 `type: "enabled"`）；未配置时无该字段
- openai：`reasoning_effort` 序列化断言；未配置时无该字段
- register_all：交叉配置仍注册成功（provider_id 正确）
- 全量：`cargo test --workspace`、clippy `-D warnings`、fmt

## 4. 文件清单

| 文件 | 动作 |
|---|---|
| `crates/parrot-providers/src/models.rs` | 新增 `find_detailed` |
| `crates/parrot-providers/src/anthropic.rs` | `build_request` 签名 + 请求体字段 |
| `crates/parrot-providers/src/openai.rs` | `OpenAiRequest` 字段 + `build_request` 参数 |
| `crates/parrot-providers/src/lib.rs` | register_all 交叉校验 warn |
