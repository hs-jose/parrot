# Provider thinking / effort 生效实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ModelEntry` 的 `thinking`（anthropic）与 `reasoning_effort`（openai）请求期注入生效；未配置零行为变化。

**Architecture:** 两个适配器在 `build_request` 前经共享纯函数 `find_detailed` 按模型 id 自查 `config_models`，把命中的字段注入请求体；register_all 注册时对交叉配置 warn。无 trait/协议变更。

**Tech Stack:** 现有依赖，无新增。

**Spec:** `docs/superpowers/specs/2026-09-12-provider-thinking-effort-design.md`

## Global Constraints

- 未配置 ⇒ 请求体零新字段（`skip_serializing_if`），行为不变。
- **不自动抬高 max_tokens**（用户决策）：budget >= max_tokens 时 API 报错上抛，配置责任归用户。
- `Simple(String)` 条目永不注入。
- thinking 输出流（`thinking_delta`）本期不处理、不显示。
- 验证命令：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`；每任务结束 `cargo build --workspace` 必须绿。
- 只动 `crates/parrot-providers/src/{models.rs, anthropic.rs, openai.rs, lib.rs}`。
- 代码注释与现有风格一致（中文简洁，仅必要处）。

---

### Task 1: models.rs — find_detailed 共享查找

**Files:**
- Modify: `crates/parrot-providers/src/models.rs`

**Interfaces:**
- Consumes: `parrot_config::ModelEntry` / `DetailedModelEntry`（已存在）
- Produces: `models::find_detailed(entries: &[ModelEntry], model: &str) -> Option<&DetailedModelEntry>`（Task 2/3 消费）

- [ ] **Step 1: 写失败测试（models.rs 测试模块追加）**

```rust
    #[test]
    fn find_detailed_matches_by_id() {
        let entries = vec![
            ModelEntry::Simple("m1".into()),
            detailed("m2", Some(200_000), None),
        ];
        let found = find_detailed(&entries, "m2").expect("found");
        assert_eq!(found.id, "m2");
        assert_eq!(found.context_window, Some(200_000));
    }

    #[test]
    fn find_detailed_misses_simple_and_unknown() {
        let entries = vec![ModelEntry::Simple("m1".into()), detailed("m2", None, None)];
        assert!(find_detailed(&entries, "m1").is_none(), "Simple 条目无元数据");
        assert!(find_detailed(&entries, "m3").is_none());
    }
```

注意：现有测试模块的 helper `detailed(id, cw, mot)` 已存在，直接复用；`use super::*` 已含。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers`
Expected: 编译错误（`find_detailed` 未定义）。

- [ ] **Step 3: 实现（models.rs 顶部函数区追加）**

```rust
/// 按模型 id 查找 Detailed 条目（Simple 简写无元数据，永不命中）。
/// 供两个适配器在 build_request 前读取 thinking / reasoning_effort。
pub fn find_detailed<'a>(
    entries: &'a [ModelEntry],
    model: &str,
) -> Option<&'a parrot_config::config::DetailedModelEntry> {
    entries.iter().find_map(|e| match e {
        ModelEntry::Detailed(d) if d.id == model => Some(d),
        _ => None,
    })
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 2 个新测试 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/models.rs
git commit -m "feat(providers): find_detailed 按模型 id 查元数据条目"
```

---

### Task 2: anthropic.rs — thinking 注入

**Files:**
- Modify: `crates/parrot-providers/src/anthropic.rs`

**Interfaces:**
- Consumes: `models::find_detailed`（Task 1）、现有 `ThinkingConfig`（parrot-config，`budget_tokens: u32`）
- Produces: `AnthropicRequest.thinking` 字段（`skip_serializing_if = "Option::is_none"`）；`build_request` 新增第 6 参 `thinking: Option<&ThinkingConfig>`（签名变为 `(model, messages, tools, system, config, stream, thinking)`，调用点 chat/chat_stream 同步更新）

- [ ] **Step 1: 写失败测试（anthropic.rs 测试模块追加）**

```rust
    #[test]
    fn build_request_injects_thinking_when_present() {
        let thinking = ThinkingConfig { budget_tokens: 64_000 };
        let config = GenerateConfig::default();
        let messages = vec![ChatMessage::new(ChatRole::User, "hi")];
        let req = AnthropicProvider::build_request(
            "m",
            vec![],
            vec![],
            None,
            &config,
            None,
            Some(&thinking),
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 64_000})
        );
    }

    #[test]
    fn build_request_omits_thinking_when_absent() {
        let config = GenerateConfig::default();
        let messages = vec![ChatMessage::new(ChatRole::User, "hi")];
        let req =
            AnthropicProvider::build_request("m", vec![], vec![], None, &config, None, None);
        let value = serde_json::to_value(&req).unwrap();
        assert!(value.get("thinking").is_none());
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers`
Expected: 编译错误（`ThinkingConfig` 未导入 / build_request 参数不匹配）。

- [ ] **Step 3: 实现**

3.1 import 加：

```rust
use parrot_config::{ModelEntry, ThinkingConfig};
```

3.2 `AnthropicRequest` 增加字段（`max_tokens` 之后）：

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
```

并新增结构体（AnthropicRequest 附近）：

```rust
#[derive(Debug, Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    thinking_type: String,
    budget_tokens: u32,
}
```

3.3 `build_request` 签名加第 7 参 `thinking: Option<&ThinkingConfig>`，并在构造处：

```rust
            thinking: thinking.map(|t| AnthropicThinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: t.budget_tokens,
            }),
```

3.4 两个调用点（`chat_stream` 与 `chat`）改为：

```rust
        let thinking = crate::models::find_detailed(&self.config_models, model)
            .and_then(|d| d.thinking.as_ref());
```

`chat_stream`：

```rust
        let request = Self::build_request(
            model,
            anthropic_messages,
            anthropic_tools,
            system,
            config,
            Some(true),
            thinking,
        );
```

`chat`：同样把 `None` 的 stream 保留、尾部加 `thinking`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 2 个新测试 PASS、既有测试（含 chat_stream 相关）不受影响、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/anthropic.rs
git commit -m "feat(anthropic): ModelEntry thinking 注入请求体"
```

---

### Task 3: openai.rs — reasoning_effort 注入

**Files:**
- Modify: `crates/parrot-providers/src/openai.rs`

**Interfaces:**
- Consumes: `models::find_detailed`（Task 1）、`parrot_config::ReasoningEffort`（serde lowercase 序列化）
- Produces: `OpenAiRequest.reasoning_effort: Option<ReasoningEffort>`；`build_request` 新增尾参 `reasoning_effort: Option<&ReasoningEffort>`

- [ ] **Step 1: 写失败测试（openai.rs 测试模块追加）**

```rust
    #[test]
    fn build_request_injects_reasoning_effort_when_present() {
        use parrot_config::ReasoningEffort;
        let config = GenerateConfig::default();
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request(
            "gpt-5",
            &messages,
            &[],
            &config,
            None,
            Some(&ReasoningEffort::High),
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["reasoning_effort"], "high");
    }

    #[test]
    fn build_request_omits_reasoning_effort_when_absent() {
        let config = GenerateConfig::default();
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request("gpt-5", &messages, &[], &config, None, None);
        let value = serde_json::to_value(&req).unwrap();
        assert!(value.get("reasoning_effort").is_none());
    }
```

注意：既有测试 `build_request_serializes_expected_shape` 的调用点需同步加尾参 `None`（保持原断言不变）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers`
Expected: 编译错误（参数不匹配）。

- [ ] **Step 3: 实现**

3.1 `OpenAiRequest` 增加字段（`stop` 之后）：

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
```

3.2 import 改为：

```rust
use parrot_config::{ModelEntry, ReasoningEffort};
```

3.3 `build_request` 签名加尾参 `reasoning_effort: Option<&ReasoningEffort>`，构造处：

```rust
            reasoning_effort: reasoning_effort.cloned(),
```

3.4 两个调用点（`chat_stream` / `chat`）：

```rust
        let reasoning_effort = crate::models::find_detailed(&self.config_models, model)
            .and_then(|d| d.reasoning_effort.as_ref());
```

调用 `Self::build_request(model, messages, tools, config, stream, reasoning_effort)`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 2 个新测试 PASS、既有测试全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/openai.rs
git commit -m "feat(openai): ModelEntry reasoning_effort 注入请求体"
```

---

### Task 4: lib.rs — register_all 交叉校验 warn

**Files:**
- Modify: `crates/parrot-providers/src/lib.rs`

**Interfaces:**
- Consumes: `ModelEntry::Detailed` 的 `thinking` / `reasoning_effort` 字段
- Produces: 注册期 warn 日志（仅日志，注册行为不变）

- [ ] **Step 1: 写失败测试（lib.rs 测试模块追加）**

```rust
    #[tokio::test]
    async fn cross_protocol_fields_warn_but_register() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        // anthropic 协议配 reasoning_effort：warn 但照常注册
        let mut p = provider("a", "anthropic");
        p.models = vec![parrot_config::ModelEntry::Detailed(
            parrot_config::config::DetailedModelEntry {
                id: "a-model".into(),
                name: None,
                context_window: None,
                max_output_tokens: None,
                thinking: None,
                reasoning_effort: Some(parrot_config::ReasoningEffort::Low),
            },
        )];
        config.providers.push(p);
        // openai 协议配 thinking：同理
        let mut q = provider("b", "openai");
        q.models = vec![parrot_config::ModelEntry::Detailed(
            parrot_config::config::DetailedModelEntry {
                id: "b-model".into(),
                name: None,
                context_window: None,
                max_output_tokens: None,
                thinking: Some(parrot_config::ThinkingConfig { budget_tokens: 1024 }),
                reasoning_effort: None,
            },
        )];
        config.providers.push(q);
        register_all(&registry, &config).await;
        assert_eq!(registry.provider_ids().await.len(), 2);
        assert!(registry.resolve("a-model").await.is_some());
        assert!(registry.resolve("b-model").await.is_some());
    }
```

- [ ] **Step 2: 运行确认通过（预期已经绿——本测试主要锁行为）**

Run: `cargo test -p parrot-providers`
Expected: PASS（交叉配置不会阻止注册）。若已绿，跳到 Step 3（warn 是日志，无断言可加；测试的价值在于锁住「不阻止注册」这一行为）。

- [ ] **Step 3: 实现（register_all for 循环体内、match 之前）**

```rust
        for entry in &provider_config.models {
            let ModelEntry::Detailed(d) = entry else {
                continue;
            };
            match provider_config.protocol.as_str() {
                "anthropic" if d.reasoning_effort.is_some() => {
                    tracing::warn!(
                        "provider {}: reasoning_effort 仅 openai 协议生效，已忽略",
                        provider_config.id
                    );
                }
                "openai" if d.thinking.is_some() => {
                    tracing::warn!(
                        "thinking 仅 anthropic 协议生效，已忽略（provider {}）",
                        provider_config.id
                    );
                }
                _ => {}
            }
        }
```

lib.rs 顶部需要 `use parrot_config::{ModelEntry, AppConfig};`（若 AppConfig 已导入则只补 ModelEntry）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/lib.rs
git commit -m "feat(providers): register_all 交叉协议 thinking/effort 配置 warn"
```

---

### Task 5: 全量验证

**Files:** 无新改动

- [ ] **Step 1:** `cargo test --workspace` 全绿
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] **Step 3:** `cargo fmt --all -- --check` clean
- [ ] **Step 4:** 对照 spec `2026-09-12-provider-thinking-effort-design.md` §2 逐条核对
- [ ] **Step 5:** 无修复则不 commit；有修复则修复后重跑 1-3 再 commit

---

## Final whole-branch review

小分支（4 commits 左右），仍按 subagent-driven-development 走终审：spec 覆盖、跨任务一致性（find_detailed 签名、build_request 调用点全改齐）、未配置零行为变化确认（既有测试不动是证据）。
