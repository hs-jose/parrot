# 单 Provider 简化实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `[[providers]]` 数组 → 单个 `[provider]` 表；未列出的模型 id 透传给唯一 provider；注册函数简化为单 provider 注册。

**Architecture:** config 结构变更（AppConfig.provider 单表）+ registry resolve 透传语义（单 provider 且 map 未命中 → 返回它）+ register_provider 接线 + 全仓构造点/fixture 跟进。trait/适配器/thinking 注入零变更。

**Tech Stack:** 现有依赖。

**Spec:** `docs/superpowers/specs/2026-09-12-single-provider-design.md`

## Global Constraints

- 每任务结束 `cargo build --workspace` 必须绿；全量 gates：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
- `ProviderRegistry` 不删除；透传仅当 registry 内恰好一个 provider 时生效。
- `[provider]` 缺失 ⇒ 无 provider 注册 ⇒ 轮次 `No provider found`（与今日空数组一致）。
- 用户 parrot.toml **只编辑不提交**（含密钥与未提交改动）。
- 代码注释与现有风格一致。

---

### Task 1: parrot-config — provider 单表 + Default + fixtures

**Files:**
- Modify: `crates/parrot-config/src/config.rs`
- Modify: `crates/parrot-config/src/lib.rs`（如 re-export 需要动）
- Modify: `crates/parrot-config/tests/config_test.rs`（fixtures）
- Modify: `crates/parrot-providers/src/lib.rs`、`crates/parrot-daemon/src/runtime.rs`、`tests/cassette_test.rs`、`tests/integration/e2e_test.rs`、`tests/integration/phase15_test.rs`（编译 ripple，最小改动）

**Interfaces:**
- Produces: `AppConfig.provider: ProviderConfig`（删除 `providers: Vec<ProviderConfig>` 字段）；`ProviderConfig: Default`（id=""、protocol="anthropic"、api_key=""、default_model=""、base_url=None、models=[]、max_tokens=None）
- Ripple（本任务内最小修复以保编译绿）：
  - `runtime.rs`：`config.providers.first().map(...)` → 临时改为 `Some(&config.provider).filter(|p| !p.id.is_empty()).map(...)`（Task 4 精修）
  - 3 个测试文件：`config.providers.push(...)` → `config.provider = ...`
  - `parrot-providers/src/lib.rs`：`register_all` 读 `config.provider`（完整改造放 Task 3，此处仅编译过）

**Steps:**
1. 写失败测试：`[provider]` 单表解析；`[provider]` 缺失 ⇒ default（id 空）；`[[providers]]` 数组写法不再被接受（含 `providers` 数组的 TOML 解析失败——serde deny 不了顶层额外字段？注意：`[[providers]]` 数组在无 `providers` 字段时会因 unknown field 报错吗？serde 默认忽略 unknown fields——所以 `[[providers]]` 会被静默忽略导致 provider 为 default。**为避免静默丢配置，AppConfig 加 `#[serde(deny_unknown_fields)]`？不行——TOML 顶层可能还有其他段。改为：新测试断言 `[[providers]]` 写法解析后 `config.provider.id.is_empty()` 并在 register_provider 时跳过（Task 3 锁行为）。**
2. 实现：字段替换 + Default impl + `resolve_env_vars` 单对象化
3. 修 ripple（见上）
4. gates + commit `feat(config): providers 数组改为单 provider 表`

（详细测试代码与现有 fixtures 改造：每个 `[[providers]]` 段改写为 `[provider]`，字段不变；`hooks_parse_from_toml` 等 6 处 + `provider_requires_protocol`/`provider_api_key_defaults_empty`/`models_mixed_entries_parse`/`provider_max_tokens_parse_and_default` + `mcp_*` 2 处；config_test.rs 7 处。）

---

### Task 2: parrot-core — resolve 透传语义

**Files:**
- Modify: `crates/parrot-core/src/provider.rs`

**Interfaces:**
- Consumes: 现有 `ProviderRegistry { providers, model_to_provider }`
- Produces: `resolve(model)`: map 命中 → provider；未命中且 `providers.len() == 1` → 该 provider；否则 None（含零个）

**Steps:**
1. 失败测试（provider.rs 内 `#[cfg(test)]`）：单 provider + 未知模型 → Some；多 provider + 未命中 → None；零 provider → None；map 命中（单/多）→ 正确 provider
2. 实现：

```rust
    pub async fn resolve(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        let providers = self.providers.read().await;
        let map = self.model_to_provider.read().await;
        if let Some(provider_id) = map.get(model) {
            return providers.get(provider_id).cloned();
        }
        // 单 provider 透传：models 列表只管展示与元数据，不做过准入
        if providers.len() == 1 {
            return providers.values().next().cloned();
        }
        None
    }
```

3. gates + commit `feat(core): ProviderRegistry 单 provider 透传`

---

### Task 3: parrot-providers — register_provider

**Files:**
- Modify: `crates/parrot-providers/src/lib.rs`

**Interfaces:**
- Consumes: `AppConfig.provider: ProviderConfig`（Task 1）
- Produces: `pub async fn register_provider(registry: &ProviderRegistry, config: &AppConfig)`——`config.provider.id` 为空 ⇒ warn + 跳过；否则按 protocol 注册一个；交叉校验 warn 保留

**Steps:**
1. 迁移既有 3 个测试（openai/anthropic/unknown-protocol）到新签名 + 新增 `no_provider_section_is_skipped`
2. 实现（register_all 主体不变，外层从循环改单读 `config.provider`，models 提取逻辑保留——注意：**models 列表仍然传给 register**（展示用），透传由 Task 2 保证）
3. ripple：daemon `run` 的调用点改名（与 Task 4 合并）
4. gates + commit `feat(providers): register_provider 单 provider 注册`

---

### Task 4: daemon runtime + 集成 fixtures 精修

**Files:**
- Modify: `crates/parrot-daemon/src/runtime.rs`
- Modify: `tests/cassette_test.rs`、`tests/integration/e2e_test.rs`、`tests/integration/phase15_test.rs`

**Steps:**
1. runtime.rs：`default_config` 读 `config.provider`（Task 1 的临时 filter 保留即可）；`register_all` → `register_provider` 调用点更新
2. 3 个测试文件确认 `config.provider = ...` 形态正确
3. 全量 gates + commit `feat(daemon): 单 provider 接线`

---

### Task 5: 用户 parrot.toml + 全量验证

1. parrot.toml 工作树：`[[providers]]` → `[provider]`，保留 anthropic-protocol 条目（含 `[1m]`/thinking/max_tokens），删除 deepseek-openai 条目（单 provider 语义下二选一，可自行切换）
2. `cargo test --workspace` / clippy / fmt 全绿
3. spec §2 逐条核对
4. commit（如有修复）

---

### Final whole-branch review

分支累积（thinking/effort 段 + max_tokens + 移除轮数上限 + 单 provider 简化），按 subagent-driven-development 终审：spec 覆盖、透传语义与 engine 三处 resolve 调用点行为、fixtures 迁移完整性、无孤儿代码（register_all 删除后无引用残留）。
