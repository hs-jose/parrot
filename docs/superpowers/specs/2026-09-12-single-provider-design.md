# 单 Provider 简化设计（路由交给外部 proxy）

日期：2026-09-13
状态：已批准
前置：`docs/superpowers/specs/2026-09-12-multi-provider-design.md`

## 1. 背景与决策

多 provider 引入的 model→provider 路由管理（重叠模型 last-wins、寻址歧义）不被需要——多 provider 路由由外部 proxy（LiteLLM / one-api / OpenRouter 等，均暴露 OpenAI 兼容端点）承担。Parrot 简化为 **只支持一个 provider 配置**。

用户已确认的决策：
- `[[providers]]` 数组 → 单个 `[provider]` 表
- `protocol` 字段保留（anthropic | openai 两种适配器仍需要）
- ModelEntry 元数据（thinking / effort / context_window / max_tokens）与注入逻辑全部保留
- **models 列表不再是路由白名单**：任何模型 id 透传给唯一 provider（proxy 场景无需同步维护列表）；列表只负责 /models 展示与请求期元数据查找
- 不改 quickstart 文档

## 2. 设计

### 2.1 配置（parrot-config）

- `AppConfig.providers: Vec<ProviderConfig>` → `AppConfig.provider: ProviderConfig`
- TOML 段：`[[providers]]` → `[provider]`；缺失时 serde default（空 id ProviderConfig）⇒ daemon 无 provider 可注册，轮次报 `No provider found`（与今日空 providers 数组行为一致）
- `ProviderConfig` 字段不变（id / protocol / api_key / default_model / base_url / models / max_tokens），加 `Default` impl（空值 + `protocol: "anthropic"`）

### 2.2 透传路由（parrot-core ProviderRegistry）

`resolve(model)` 语义扩展：map 命中 → 该 provider；**map 未命中且 registry 只注册了一个 provider → 返回它**（透传）；多个或零个 provider 时维持 None。engine 的 `resolve` / `resolve_provider_id` / `resolve_model_context_window` 路径不变。

### 2.3 注册（parrot-providers）

- `register_all(registry, config)` → `register_provider(registry, config: &AppConfig)`：读 `config.provider`，`id` 为空则跳过（无 provider 配置）
- 交叉校验 warn（anthropic+effort / openai+thinking）逻辑保留
- `models` 列表仍传给 registry（供 /models 聚合与 AgentStart 显示）；透传语义由 §2.2 保证，列表不再做准入

### 2.4 daemon（parrot-daemon）

- `runtime.rs` `default_config`：从 `config.provider` 构造（model = default_model，max_tokens = provider.max_tokens.unwrap_or(8192)）；`provider.id` 为空时 `GenerateConfig::default()`（与现状一致）
- `collect_models` 等聚合路径经 registry 不变

### 2.5 非目标

- 不删 `ProviderRegistry`（trait 与 resolve 结构保留，仅语义扩展）
- 不做 quickstart/proxy 文档
- 不做多 provider 寻址语法（已随路由删除而失去必要性）

## 3. 测试策略

- config：`[provider]` 单表解析、缺失时 default、字段 roundtrip（沿用既有测试改造）
- core：resolve 透传（单 provider + 未知模型 id → 命中；多 provider + 未命中 → None 锁行为）
- providers：register_provider 单注册、空 id 跳过、交叉校验 warn 测试迁移
- 集成 fixtures：cassette / e2e / phase15 的 ProviderConfig 字面量改为 `config.provider = ...`
- 全量 gates

## 4. 文件清单

| 文件 | 动作 |
|---|---|
| `crates/parrot-config/src/config.rs` | providers 数组 → provider 单表 + Default + fixtures |
| `crates/parrot-config/tests/config_test.rs` | fixtures 迁移 |
| `crates/parrot-core/src/provider.rs` | resolve 透传语义 + 测试 |
| `crates/parrot-providers/src/lib.rs` | register_all → register_provider + 测试 |
| `crates/parrot-daemon/src/runtime.rs` | default_config 读 config.provider |
| `tests/{cassette,phase15,e2e}` | 字面量迁移 |
| 用户 parrot.toml | `[provider]` 单表（工作树，不提交） |
