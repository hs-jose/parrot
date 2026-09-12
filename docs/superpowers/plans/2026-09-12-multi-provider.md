# 多 Provider 接入（OpenAI 兼容协议）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 OpenAI 兼容协议适配器，`register_all` 按 `protocol` 字段分发，Anthropic provider 对齐（`list_models` 远端拉取 + 配置元数据合并），引擎 context_window=0 视为未知。

**Architecture:** `parrot-config` 定义 `ModelEntry`（字符串简写或含元数据的表）与必填 `protocol`；`parrot-providers` 新增 `openai.rs` 适配器与共享的 `models.rs` 合并逻辑；`anthropic.rs` 构造函数接收 `provider_id` + `config_models`；适配器请求构造、SSE 解析均以可单测的纯函数为主。

**Tech Stack:** reqwest + serde（复用 anthropic.rs 同款依赖）、async-trait、tokio mpsc、复用 `retry.rs`。

**Spec:** `docs/superpowers/specs/2026-09-12-multi-provider-design.md`

## Global Constraints

- `parrot-core` 零 IO 不动（本计划只改 `engine.rs` 一个纯逻辑过滤）。
- 跨 crate 错误用 `ProviderError`（thiserror），不新增变体。
- `base_url` 语义：OpenAI 兼容端点 `base_url` **含** `/v1`（适配器拼 `/chat/completions`、`/models`）；Anthropic `base_url` **不含** `/v1`（适配器拼 `/v1/messages`、`/v1/models`）——维持现状。
- `api_key` 为空 ⇒ 不发送鉴权 header（`Authorization` / `x-api-key`）。
- `thinking` / `reasoning_effort` 本期**只解析保存，不注入请求**。
- 远端 `GET /models`（两家）都**没有** context window 信息 ⇒ 远端条目填 0。
- 流式开始后的错误不重试（`StreamError` 不重试，与 anthropic 一致）。
- 每个任务结束工作区必须可编译：`cargo build --workspace` 通过。
- 验证命令：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
- 代码注释风格与现有文件一致（中文简洁注释，仅必要处）。

---

### Task 1: parrot-config — ModelEntry 类型 + protocol 必填 + 全仓构造点跟进

**Files:**
- Modify: `crates/parrot-config/src/config.rs`
- Modify: `crates/parrot-config/src/lib.rs`
- Modify: `crates/parrot-providers/src/lib.rs`（register_all 的 models 提取）
- Modify: `tests/cassette_test.rs:371`、`tests/integration/phase15_test.rs:162`、`tests/integration/e2e_test.rs:220`（ProviderConfig 字面量）
- Modify: `parrot.toml`（补 protocol）

**Interfaces:**
- Consumes: 无（首个任务）
- Produces: `parrot_config::ModelEntry`（`Simple(String)` / `Detailed(DetailedModelEntry)`，untagged；`impl From<String>`、`From<&str>`；`fn id(&self) -> &str`）、`DetailedModelEntry { id, name?, context_window?, max_output_tokens?, thinking?, reasoning_effort? }`、`ThinkingConfig { budget_tokens: u32 }`、`ReasoningEffort { Low/Medium/High }`；`ProviderConfig { id, protocol: String(必填), api_key: String(serde default), default_model, base_url?, models: Vec<ModelEntry> }`

- [ ] **Step 1: 写失败测试（config.rs 测试模块追加）**

```rust
    #[test]
    fn provider_requires_protocol() {
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
"#;
        assert!(
            toml::from_str::<AppConfig>(toml).is_err(),
            "protocol 缺失必须解析失败"
        );
    }

    #[test]
    fn provider_api_key_defaults_empty() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "ollama"
protocol = "openai"
base_url = "http://localhost:11434/v1"
default_model = "qwen3"

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
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.providers[0].api_key, "");
    }

    #[test]
    fn models_mixed_entries_parse() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "x"
default_model = "m"
models = [
  "claude-sonnet-4-6",
  { id = "deepseek-v4-flash[1m]", context_window = 1000000, max_output_tokens = 8192,
    thinking = { budget_tokens = 64000 } },
]

[[providers]]
id = "openai-official"
protocol = "openai"
api_key = "x"
default_model = "gpt-5"
models = [
  { id = "gpt-5", name = "GPT-5", reasoning_effort = "high" },
]

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
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        let p0 = &c.providers[0];
        assert_eq!(p0.models[0], ModelEntry::Simple("claude-sonnet-4-6".into()));
        match &p0.models[1] {
            ModelEntry::Detailed(d) => {
                assert_eq!(d.id, "deepseek-v4-flash[1m]");
                assert_eq!(d.context_window, Some(1_000_000));
                assert_eq!(d.max_output_tokens, Some(8192));
                assert_eq!(d.thinking.as_ref().unwrap().budget_tokens, 64000);
                assert!(d.reasoning_effort.is_none());
            }
            other => panic!("expected Detailed, got {other:?}"),
        }
        match &c.providers[1].models[0] {
            ModelEntry::Detailed(d) => {
                assert_eq!(d.id, "gpt-5");
                assert_eq!(d.name.as_deref(), Some("GPT-5"));
                assert_eq!(d.reasoning_effort, Some(ReasoningEffort::High));
            }
            other => panic!("expected Detailed, got {other:?}"),
        }
        // 序列化回 TOML 值不丢字段（roundtrip）
        let back = toml::Value::try_from(&c.providers[1].models[0]).unwrap();
        assert_eq!(
            back.get("reasoning_effort").and_then(|v| v.as_str()),
            Some("high")
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p parrot-config 2>&1 | Select-String -Pattern "provider_requires_protocol|error\["`
Expected: 编译错误（`ModelEntry`/`ReasoningEffort` 未定义）或解析成功导致断言失败。

- [ ] **Step 3: 实现 config.rs 类型**

在 `ProviderConfig` 定义前新增：

```rust
/// [[providers]] models 条目。字符串简写（上下文未知）或含元数据的表。
/// 两家协议的 GET /models 都不返回 context window，配置是唯一可靠来源
/// （spec §3.1）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ModelEntry {
    Simple(String),
    Detailed(DetailedModelEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetailedModelEntry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// 思考模式扩展点（anthropic 协议）。本期只解析保存，不注入请求。
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// effort 扩展点（openai 协议）。本期只解析保存，不注入请求。
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingConfig {
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ModelEntry {
    pub fn id(&self) -> &str {
        match self {
            ModelEntry::Simple(id) => id,
            ModelEntry::Detailed(d) => &d.id,
        }
    }
}

impl From<String> for ModelEntry {
    fn from(s: String) -> Self {
        ModelEntry::Simple(s)
    }
}

impl From<&str> for ModelEntry {
    fn from(s: &str) -> Self {
        ModelEntry::Simple(s.to_string())
    }
}
```

`ProviderConfig` 改为：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    /// 必填：`"anthropic"` | `"openai"`（OpenAI 兼容）。缺失即解析报错。
    pub protocol: String,
    #[serde(default)]
    pub api_key: String,
    pub default_model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}
```

注意：`models` 加 `#[serde(default)]`（配置可只配 default_model，如 deepseek/claude-proxy 示例）。

`crates/parrot-config/src/lib.rs` 增加 re-export：

```rust
pub use config::ModelEntry;
pub use config::ReasoningEffort;
pub use config::ThinkingConfig;
```

- [ ] **Step 4: 修复全仓构造点（保持编译绿）**

`crates/parrot-providers/src/lib.rs` 的 `register_all`：models 提取改为按 `ModelEntry::id()`，分发暂仍按 id（Task 6 切到 protocol）：

```rust
pub async fn register_all(registry: &ProviderRegistry, config: &AppConfig) {
    for provider_config in &config.providers {
        let models = if provider_config.models.is_empty() {
            vec![provider_config.default_model.clone()]
        } else {
            provider_config
                .models
                .iter()
                .map(|m| m.id().to_string())
                .collect()
        };
        match provider_config.id.as_str() {
            "anthropic" => {
                let provider = crate::anthropic::AnthropicProvider::new(
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.default_model.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
            "openai" => {
                tracing::warn!("OpenAI provider not yet implemented, skipping");
            }
            other => {
                tracing::warn!("Unknown provider id: {}, skipping", other);
            }
        }
    }
}
```

三个测试文件的 `ProviderConfig` 字面量补 `protocol` 字段、models 元素加 `.into()`：

`tests/cassette_test.rs:371`：

```rust
    config.providers.push(parrot_config::ProviderConfig {
        id: "anthropic".to_string(),
        protocol: "anthropic".to_string(),
        api_key: String::new(),
        default_model: "claude-sonnet-4-6".to_string(),
        base_url: None,
        models: vec!["claude-sonnet-4-6".into()],
    });
```

`tests/integration/phase15_test.rs:162`：

```rust
    config.providers.push(parrot_config::ProviderConfig {
        id: "mock".to_string(),
        protocol: "anthropic".to_string(),
        api_key: String::new(),
        default_model: "mock-model".to_string(),
        base_url: None,
        models: vec!["mock-model".into()],
    });
```

`tests/integration/e2e_test.rs:220`：

```rust
    config.providers.push(parrot_config::ProviderConfig {
        id: "mock".to_string(),
        protocol: "anthropic".to_string(),
        api_key: String::new(),
        default_model: "mock-model".to_string(),
        base_url: None,
        models: vec!["mock-model".into()],
    });
```

`parrot.toml` 的 `[[providers]]` 补一行：

```toml
[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "sk-48f0e36d24a149848c3e31debb0666cc"
default_model = "deepseek-v4-flash"
base_url = "https://api.deepseek.com/anthropic"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p parrot-config && cargo test --workspace 2>&1 | Select-String -Pattern "test result"`
Expected: parrot-config 全 PASS；workspace 无编译错误、测试 PASS（旧测试的 TOML fixture 都需要补 `protocol = "anthropic"`：config.rs 内 `hooks_parse_from_toml`、`hooks_parse_per_hook_configs`、`hooks_missing_configs_defaults_empty`、`hooks_parse_redact_secrets_extra_patterns`、`mcp_servers_parse_from_toml`、`mcp_entry_defaults_fill_in` 共 6 处，在每个 `[[providers]]` 段内加一行）。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(config): ModelEntry 元数据 + protocol 必填 + api_key 可空"
```

---

### Task 2: parrot-providers — models.rs 共享合并逻辑

**Files:**
- Create: `crates/parrot-providers/src/models.rs`
- Modify: `crates/parrot-providers/src/lib.rs`（`pub mod models;`）

**Interfaces:**
- Consumes: `parrot_config::ModelEntry`（Task 1）、`parrot_core::types::ModelInfo { id, name, provider, context_window: u32, max_output_tokens: u32 }`
- Produces: `models::entry_to_model_info(entry: &ModelEntry, provider_id: &str) -> ModelInfo`；`models::merge_models(remote: Vec<ModelInfo>, config_models: &[ModelEntry], provider_id: &str) -> Vec<ModelInfo>`

- [ ] **Step 1: 写失败测试（models.rs `#[cfg(test)]`）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::DetailedModelEntry;

    fn remote(id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            provider: "p".into(),
            context_window: 0,
            max_output_tokens: 0,
        }
    }

    fn detailed(id: &str, cw: Option<u32>, mot: Option<u32>) -> ModelEntry {
        ModelEntry::Detailed(DetailedModelEntry {
            id: id.to_string(),
            name: None,
            context_window: cw,
            max_output_tokens: mot,
            thinking: None,
            reasoning_effort: None,
        })
    }

    #[test]
    fn merge_overrides_config_metadata() {
        let result = merge_models(
            vec![remote("m1", "Remote Name")],
            &[detailed("m1", Some(1_000_000), Some(8192))],
            "p",
        );
        assert_eq!(result[0].name, "Remote Name");
        assert_eq!(result[0].context_window, 1_000_000);
        assert_eq!(result[0].max_output_tokens, 8192);
    }

    #[test]
    fn simple_entry_does_not_override_remote() {
        let result = merge_models(
            vec![remote("m1", "Remote Name")],
            &[ModelEntry::Simple("m1".into())],
            "p",
        );
        assert_eq!(result[0].name, "Remote Name");
        assert_eq!(result[0].context_window, 0);
    }

    #[test]
    fn merge_appends_config_only_entries() {
        let result = merge_models(
            vec![remote("m1", "M1")],
            &[
                ModelEntry::Simple("m2".into()),
                detailed("m3", Some(200_000), None),
            ],
            "p",
        );
        assert_eq!(result.len(), 3);
        assert_eq!(result[1].id, "m2");
        assert_eq!(result[1].name, "m2");
        assert_eq!(result[1].context_window, 0);
        assert_eq!(result[2].id, "m3");
        assert_eq!(result[2].context_window, 200_000);
        assert_eq!(result[2].max_output_tokens, 0);
    }

    #[test]
    fn entry_to_model_info_fills_defaults() {
        let info = entry_to_model_info(&ModelEntry::Simple("m".into()), "prov");
        assert_eq!(info.name, "m");
        assert_eq!(info.provider, "prov");
        assert_eq!(info.context_window, 0);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers 2>&1 | Select-String -Pattern "error"`
Expected: 编译错误（models 模块不存在）。

- [ ] **Step 3: 实现 models.rs**

```rust
//! list_models 配置元数据合并（spec §3.1）。
//!
//! 远端 `GET /models` 拿不到 context window，配置是唯一可靠来源：
//! 以远端结果为基底，配置条目按 id 覆盖（Detailed 条目逐字段覆盖 Some
//! 字段）；配置独有条目追加。

use parrot_config::ModelEntry;
use parrot_core::types::ModelInfo;

pub fn entry_to_model_info(entry: &ModelEntry, provider_id: &str) -> ModelInfo {
    match entry {
        ModelEntry::Simple(id) => ModelInfo {
            id: id.clone(),
            name: id.clone(),
            provider: provider_id.to_string(),
            context_window: 0,
            max_output_tokens: 0,
        },
        ModelEntry::Detailed(d) => ModelInfo {
            id: d.id.clone(),
            name: d.name.clone().unwrap_or_else(|| d.id.clone()),
            provider: provider_id.to_string(),
            context_window: d.context_window.unwrap_or(0),
            max_output_tokens: d.max_output_tokens.unwrap_or(0),
        },
    }
}

pub fn merge_models(
    remote: Vec<ModelInfo>,
    config_models: &[ModelEntry],
    provider_id: &str,
) -> Vec<ModelInfo> {
    let mut result = remote;
    for entry in config_models {
        let id = entry.id().to_string();
        match result.iter_mut().find(|m| m.id == id) {
            Some(existing) => {
                if let ModelEntry::Detailed(d) = entry {
                    if let Some(name) = &d.name {
                        existing.name = name.clone();
                    }
                    if let Some(cw) = d.context_window {
                        existing.context_window = cw;
                    }
                    if let Some(mot) = d.max_output_tokens {
                        existing.max_output_tokens = mot;
                    }
                }
            }
            None => result.push(entry_to_model_info(entry, provider_id)),
        }
    }
    result
}
```

`crates/parrot-providers/src/lib.rs` 头部加 `pub mod models;`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers`
Expected: 4 个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/models.rs crates/parrot-providers/src/lib.rs
git commit -m "feat(providers): ModelEntry→ModelInfo 与远端列表合并逻辑"
```

---

### Task 3: anthropic.rs — provider_id + list_models 远端拉取

**Files:**
- Modify: `crates/parrot-providers/src/anthropic.rs`
- Modify: `crates/parrot-providers/src/lib.rs`（register_all anthropic 分支传新签名）

**Interfaces:**
- Consumes: `models::{entry_to_model_info, merge_models}`（Task 2）、`crate::retry::with_retry`（现有）
- Produces: `AnthropicProvider::new(provider_id: String, api_key: String, base_url: Option<String>, config_models: Vec<ModelEntry>)`；`provider_id()` 返回配置 id；纯函数 `parse_anthropic_models_page(json: &Value) -> (Vec<(String, String)>, bool, Option<String>)`（(id, name), has_more, last_id）；`AnthropicProvider::fallback_models(config_models: &[ModelEntry], provider_id: &str) -> Vec<ModelInfo>`

- [ ] **Step 1: 写失败测试（anthropic.rs `#[cfg(test)]`，文件末尾追加）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::DetailedModelEntry;
    use serde_json::json;

    #[test]
    fn parse_models_page_extracts_ids_and_pagination() {
        let json = json!({
            "data": [
                {"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "type": "model"},
                {"id": "claude-opus-4", "display_name": "Claude Opus 4", "type": "model"}
            ],
            "has_more": true,
            "last_id": "claude-opus-4"
        });
        let (models, has_more, last_id) = parse_anthropic_models_page(&json);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], ("claude-sonnet-4-6".to_string(), "Claude Sonnet 4.6".to_string()));
        assert!(has_more);
        assert_eq!(last_id.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn parse_models_page_end_of_pages() {
        let json = json!({"data": [{"id": "m1"}], "has_more": false});
        let (models, has_more, last_id) = parse_anthropic_models_page(&json);
        assert_eq!(models.len(), 1);
        assert!(!has_more);
        assert_eq!(last_id, None);
        // display_name 缺省回退 id
        assert_eq!(models[0].1, "m1");
    }

    #[test]
    fn fallback_prefers_config_models() {
        let entries = vec![ModelEntry::Detailed(DetailedModelEntry {
            id: "deepseek-v4-flash[1m]".into(),
            name: None,
            context_window: Some(1_000_000),
            max_output_tokens: Some(8192),
            thinking: None,
            reasoning_effort: None,
        })];
        let models = AnthropicProvider::fallback_models(&entries, "claude-proxy");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-v4-flash[1m]");
        assert_eq!(models[0].context_window, 1_000_000);
        assert_eq!(models[0].provider, "claude-proxy");
    }

    #[test]
    fn fallback_hardcoded_when_config_empty() {
        let models = AnthropicProvider::fallback_models(&[], "anthropic");
        assert_eq!(models.len(), 3);
        assert!(models.iter().all(|m| m.provider == "anthropic"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers 2>&1 | Select-String -Pattern "error"`
Expected: 编译错误（`parse_anthropic_models_page`、`fallback_models`、`ModelEntry` 导入不存在）。

- [ ] **Step 3: 实现**

3.1 结构体与构造函数（anthropic.rs:12-16、63-71）：

```rust
pub struct AnthropicProvider {
    provider_id: String,
    api_key: String,
    base_url: String,
    config_models: Vec<ModelEntry>,
    client: Client,
}
```

```rust
impl AnthropicProvider {
    pub fn new(
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        config_models: Vec<ModelEntry>,
    ) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self {
            provider_id,
            api_key,
            base_url,
            config_models,
            client: Client::new(),
        }
    }
```

文件头部 import 加：

```rust
use parrot_config::ModelEntry;
```

3.2 `provider_id()`（原 anthropic.rs:234-236）：

```rust
    fn provider_id(&self) -> &str {
        &self.provider_id
    }
```

3.3 替换整个 `list_models`（原 anthropic.rs:238-262）为远端拉取 + 合并 + 回退：

```rust
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        match self.fetch_remote_models().await {
            Ok(remote) => {
                Ok(crate::models::merge_models(remote, &self.config_models, &self.provider_id))
            }
            Err(e) => {
                tracing::warn!(
                    "anthropic list_models 远端拉取失败({e})，回退配置 models"
                );
                Ok(Self::fallback_models(&self.config_models, &self.provider_id))
            }
        }
    }

    /// 远端分页拉取 /v1/models。任一页失败即整体失败（fail-open 在调用方）。
    async fn fetch_remote_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut all = Vec::new();
        let mut after_id: Option<String> = None;
        loop {
            let mut url = format!("{}/v1/models?limit=1000", self.base_url);
            if let Some(after) = &after_id {
                url.push_str("&after_id=");
                url.push_str(after);
            }
            let response = crate::retry::with_retry(|| self.get_models_page(&url)).await?;
            let json: Value = response
                .json()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            let (page_models, has_more, last_id) = parse_anthropic_models_page(&json);
            all.extend(page_models.into_iter().map(|(id, name)| ModelInfo {
                id,
                name,
                provider: self.provider_id.clone(),
                context_window: 0,
                max_output_tokens: 0,
            }));
            if !has_more {
                break;
            }
            match last_id {
                Some(id) => after_id = Some(id),
                None => break,
            }
        }
        Ok(all)
    }

    async fn get_models_page(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let mut request = self.client.get(url).header("anthropic-version", "2023-06-01");
        if !self.api_key.is_empty() {
            request = request.header("x-api-key", &self.api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5000);
            return Err(ProviderError::RateLimited {
                retry_after_ms: retry_after,
            });
        }
        if status.is_client_error() || status.is_server_error() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }

    fn fallback_models(config_models: &[ModelEntry], provider_id: &str) -> Vec<ModelInfo> {
        if !config_models.is_empty() {
            return config_models
                .iter()
                .map(|m| crate::models::entry_to_model_info(m, provider_id))
                .collect();
        }
        vec![
            ModelInfo {
                id: "claude-sonnet-4-6".to_string(),
                name: "Claude Sonnet 4.6".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-opus-4".to_string(),
                name: "Claude Opus 4".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "claude-haiku-3-5".to_string(),
                name: "Claude Haiku 3.5".to_string(),
                provider: provider_id.to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            },
        ]
    }
```

3.4 纯解析函数（放在 `parse_sse_stream` 旁、文件级自由函数）：

```rust
/// 解析一页 /v1/models 响应：(id, name) 列表、has_more、last_id。
fn parse_anthropic_models_page(json: &Value) -> (Vec<(String, String)>, bool, Option<String>) {
    let mut models = Vec::new();
    if let Some(data) = json.get("data").and_then(|v| v.as_array()) {
        for m in data {
            let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = m
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            models.push((id.to_string(), name));
        }
    }
    let has_more = json.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false);
    let last_id = json.get("last_id").and_then(|v| v.as_str()).map(String::from);
    (models, has_more, last_id)
}
```

3.5 `crates/parrot-providers/src/lib.rs` anthropic 分支改传新签名：

```rust
            "anthropic" => {
                let provider = crate::anthropic::AnthropicProvider::new(
                    provider_config.id.clone(),
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.models.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/anthropic.rs crates/parrot-providers/src/lib.rs
git commit -m "feat(anthropic): list_models 远端分页拉取 + 配置回退；provider_id 可配置"
```

---

### Task 4: openai.rs — 请求/响应映射 + 非流式 chat()

**Files:**
- Create: `crates/parrot-providers/src/openai.rs`
- Modify: `crates/parrot-providers/src/lib.rs`（`pub mod openai;`）

**Interfaces:**
- Consumes: `LlmProvider`/`ChatStream`/`ProviderStreamEvent`/`ProviderStopReason`（parrot-core）、`ToolDefinition`、`ChatMessage`/`ChatRole`/`GenerateConfig`/`ModelInfo`/`ToolCallInfo`、`parrot_protocol::types::Usage`、`crate::retry::with_retry`、`crate::models::entry_to_model_info`
- Produces: `OpenAiProvider::new(provider_id: String, api_key: String, base_url: Option<String>, config_models: Vec<ModelEntry>)`；纯函数 `stop_reason_from_openai(&str) -> ProviderStopReason`；`OpenAiStreamAggregator`（Task 5 用）；`OpenAiProvider::auth_header(api_key: &str) -> Option<String>`（返回 `Bearer <key>` 或 None）

- [ ] **Step 1: 写失败测试（openai.rs `#[cfg(test)]`）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage::new(role, content)
    }

    #[test]
    fn convert_messages_maps_all_roles() {
        let messages = vec![
            msg(ChatRole::System, "sys"),
            msg(ChatRole::User, "hello"),
            {
                let mut m = msg(ChatRole::Assistant, "");
                m.tool_calls = Some(vec![ToolCallInfo {
                    id: "call_1".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "hi"}),
                }]);
                m
            },
            {
                let mut m = msg(ChatRole::Tool, "tool output");
                m.tool_call_id = Some("call_1".into());
                m
            },
        ];
        let out = OpenAiProvider::convert_messages(&messages);
        assert_eq!(out[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(out[1], json!({"role": "user", "content": "hello"}));
        assert_eq!(
            out[2],
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{\"text\":\"hi\"}"}
                }]
            })
        );
        assert_eq!(
            out[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "tool output"})
        );
    }

    #[test]
    fn convert_tools_maps_function_format() {
        let tools = vec![ToolDefinition {
            name: "echo".into(),
            description: "Echo a message".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let out = OpenAiProvider::convert_tools(&tools);
        assert_eq!(
            out[0],
            json!({
                "type": "function",
                "function": {
                    "name": "echo",
                    "description": "Echo a message",
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        );
    }

    #[test]
    fn build_request_serializes_expected_shape() {
        let config = GenerateConfig {
            model: "gpt-5".into(),
            temperature: Some(0.7),
            max_tokens: Some(1024),
            stop_sequences: Some(vec!["STOP".into()]),
        };
        let messages = vec![msg(ChatRole::User, "hi")];
        let req = OpenAiProvider::build_request("gpt-5", &messages, &[], &config, Some(true));
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["model"], "gpt-5");
        assert_eq!(value["stream"], true);
        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["stop"], json!(["STOP"]));
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            stop_reason_from_openai("tool_calls"),
            ProviderStopReason::ToolUse
        );
        assert_eq!(stop_reason_from_openai("stop"), ProviderStopReason::EndTurn);
        assert_eq!(
            stop_reason_from_openai("length"),
            ProviderStopReason::MaxTokens
        );
        assert_eq!(
            stop_reason_from_openai("weird"),
            ProviderStopReason::EndTurn
        );
    }

    #[test]
    fn parse_chat_response_maps_content_and_tool_calls() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "doing it",
                    "tool_calls": [
                        {"id": "call_a", "type": "function",
                         "function": {"name": "echo", "arguments": "{\"x\": 1}"}},
                        {"id": "call_b", "type": "function",
                         "function": {"name": "ls", "arguments": "{}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let parsed: OpenAiChatResponse = serde_json::from_value(body).expect("deserialize");
        let msg = parsed.choices[0].message.to_chat_message();
        assert_eq!(msg.content, "doing it");
        let tcs = msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].id, "call_a");
        assert_eq!(tcs[0].name, "echo");
        assert_eq!(tcs[0].arguments, json!({"x": 1}));
        assert_eq!(tcs[1].arguments, json!({}));
    }

    #[test]
    fn auth_header_bearer_or_none() {
        assert_eq!(OpenAiProvider::auth_header("sk-abc"), Some("Bearer sk-abc".to_string()));
        assert_eq!(OpenAiProvider::auth_header(""), None);
    }
}
```

注意：上面 `tcs[0].arguments` 的占位断言在 Step 3 实现后改为：

```rust
        assert_eq!(tcs[0].arguments, json!({"x": 1}));
```

（测试体中的 `json!({"x": 1})` 与响应里 `"{\"x\": 1}"` 对应；body 里的 arguments 字符串写 `"``{\"x\": 1}``"`——见 Step 1 实际代码，把 `"{\"x\": 5 - 5 + 0}"` 写成 `"``{\"x\": 1}``"`。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers 2>&1 | Select-String -Pattern "error"`
Expected: 编译错误（openai 模块不存在）。

- [ ] **Step 3: 实现 openai.rs（本任务范围：结构体、映射、非流式 chat；chat_stream/list_models 留 Task 5）**

```rust
use async_trait::async_trait;
use parrot_config::ModelEntry;
use parrot_core::error::ProviderError;
use parrot_core::provider::{ChatStream, LlmProvider, ProviderStopReason};
use parrot_core::tool::ToolDefinition;
use parrot_core::types::{ChatMessage, ChatRole, GenerateConfig, ModelInfo, ToolCallInfo};
use parrot_protocol::types::Usage;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct OpenAiProvider {
    provider_id: String,
    api_key: String,
    base_url: String,
    config_models: Vec<ModelEntry>,
    client: Client,
}

#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiFunctionCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunctionCall {
    id: String,
    function: OpenAiFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunction {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        config_models: Vec<ModelEntry>,
    ) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        Self {
            provider_id,
            api_key,
            base_url,
            config_models,
            client: Client::new(),
        }
    }

    /// api_key 为空 ⇒ 不带鉴权 header（Ollama 等本地服务）。
    fn auth_header(api_key: &str) -> Option<String> {
        (!api_key.is_empty()).then(|| format!("Bearer {api_key}"))
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<Value> {
        messages
            .iter()
            .filter_map(|m| match m.role {
                ChatRole::System => Some(json!({"role": "system", "content": m.content})),
                ChatRole::User => Some(json!({"role": "user", "content": m.content})),
                ChatRole::Assistant => {
                    let mut obj = json!({"role": "assistant", "content": m.content});
                    if let Some(tool_calls) = &m.tool_calls {
                        let calls: Vec<Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments.to_string(),
                                    }
                                })
                            })
                            .collect();
                        obj["tool_calls"] = Value::Array(calls);
                    }
                    Some(obj)
                }
                ChatRole::Tool => {
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    Some(json!({"role": "tool", "tool_call_id": id, "content": m.content}))
                }
            })
            .collect()
    }

    fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect()
    }

    fn build_request(
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
        stream: Option<bool>,
    ) -> OpenAiRequest {
        OpenAiRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            tools: Self::convert_tools(tools),
            stream,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            stop: config.stop_sequences.clone(),
        }
    }

    async fn send_request(
        &self,
        request: &OpenAiRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        crate::retry::with_retry(|| self.send_request_once(request)).await
    }

    async fn send_request_once(
        &self,
        request: &OpenAiRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut builder = self.client.post(&url).header("content-type", "application/json");
        if let Some(auth) = Self::auth_header(&self.api_key) {
            builder = builder.header("authorization", auth);
        }
        let response = builder
            .json(request)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5000);
            return Err(ProviderError::RateLimited {
                retry_after_ms: retry_after,
            });
        }
        if status.is_client_error() || status.is_server_error() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }
}

fn stop_reason_from_openai(reason: &str) -> ProviderStopReason {
    match reason {
        "tool_calls" => ProviderStopReason::ToolUse,
        "length" => ProviderStopReason::MaxTokens,
        _ => ProviderStopReason::EndTurn,
    }
}

impl OpenAiResponseMessage {
    fn to_chat_message(&self) -> ChatMessage {
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        if let Some(calls) = &self.tool_calls {
            for tc in calls {
                let arguments = tc
                    .function
                    .arguments
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                tool_calls.push(ToolCallInfo {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments,
                });
            }
        }
        ChatMessage {
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            ..ChatMessage::new(ChatRole::Assistant, self.content.clone().unwrap_or_default())
        }
    }
}
```

注意：上文的响应结构体名按测试统一为 `OpenAiResponseMessage`，即把 `OpenAiResponseMessage` 定义为：

```rust
#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiFunctionCall>>,
}
```

`OpenAiChoice` 里 `message: OpenAiResponseMessage`。

同时实现 trait 的**非流式部分**（`chat_stream` 与 `list_models` 本任务先返回占位错误/配置列表，Task 5 补全）：

```rust
#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // Task 5 换成远端拉取+合并；先按配置列表直接构造（fail-open 语义一致）
        Ok(self
            .config_models
            .iter()
            .map(|m| crate::models::entry_to_model_info(m, &self.provider_id))
            .collect())
    }

    async fn chat_stream(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        // Task 5 实现
        Err(ProviderError::Api {
            status: 501,
            body: "chat_stream not implemented yet".into(),
        })
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        let request = Self::build_request(model, messages, tools, config, None);
        let response = self.send_request(&request).await?;
        let body: OpenAiChatResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let choice = body.choices.into_iter().next().ok_or_else(|| {
            ProviderError::Api {
                status: 200,
                body: "empty choices".into(),
            }
        })?;
        Ok(choice.message.to_chat_message())
    }
}
```

注意 Step 1 测试中 `tcs[0].arguments` 断言为 `json!({"x": 1})`（arguments 字符串反序列化为 Value）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/openai.rs crates/parrot-providers/src/lib.rs
git commit -m "feat(providers): OpenAI 兼容适配器——消息/工具映射与非流式 chat"
```

---

### Task 5: openai.rs — SSE 流解析 + chat_stream + list_models 远端拉取

**Files:**
- Modify: `crates/parrot-providers/src/openai.rs`

**Interfaces:**
- Consumes: Task 4 的 `OpenAiProvider` 结构与 `send_request`、`crate::models::merge_models`
- Produces: `OpenAiStreamAggregator { fn new() -> Self, fn feed(&mut self, chunk: &Value) -> Vec<ProviderStreamEvent>, fn finish(&mut self) -> Vec<ProviderStreamEvent> }`；`chat_stream` 完整实现；`list_models` 远端拉取 + fail-open

- [ ] **Step 1: 写失败测试（追加到 openai.rs 测试模块）**

```rust
    #[test]
    fn aggregator_streams_text_then_finish() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"content":"Hel"}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"content":"lo"}}]})));
        events.extend(agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2}})));
        events.extend(agg.finish());
        assert_eq!(
            events,
            vec![
                ProviderStreamEvent::TextDelta { delta: "Hel".into() },
                ProviderStreamEvent::TextDelta { delta: "lo".into() },
                ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::EndTurn,
                    usage: Usage { input_tokens: 7, output_tokens: 2 },
                }
            ]
        );
    }

    #[test]
    fn aggregator_aggregates_parallel_tool_calls() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"echo","arguments":""}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"x\":"}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":1,"id":"call_b","function":{"name":"ls","arguments":"{\"path\":\"."}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"1}}"}},
            {"index":1,"function":{"arguments":",\"depth\":2}}"}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        events.extend(agg.finish());
        assert_eq!(
            events,
            vec![
                ProviderStreamEvent::ToolCallStart { id: "call_a".into(), name: "echo".into() },
                ProviderStreamEvent::ToolCallDelta { id: "call_a".into(), args_delta: "{\"x\":".into() },
                ProviderStreamEvent::ToolCallStart { id: "call_b".into(), name: "ls".into() },
                ProviderStreamEvent::ToolCallDelta { id: "call_b".into(), args_delta: "{\"path\":\".".into() },
                ProviderStreamEvent::ToolCallDelta { id: "call_a".into(), args_delta: "1}}".into() },
                ProviderStreamEvent::ToolCallDelta { id: "call_b".into(), args_delta: ",\"depth\":2}}".into() },
                ProviderStreamEvent::ToolCallEnd {
                    id: "call_a".into(),
                    arguments: json!({"x": 1}),
                },
                ProviderStreamEvent::ToolCallEnd {
                    id: "call_b".into(),
                    arguments: json!({"path": ".", "depth": 2}),
                },
                ProviderStreamEvent::Finish {
                    stop_reason: ProviderStopReason::ToolUse,
                    usage: Usage { input_tokens: 0, output_tokens: 0 },
                }
            ]
        );
    }

    #[test]
    fn aggregator_parses_bad_arguments_as_empty_object() {
        let mut agg = OpenAiStreamAggregator::new();
        let mut events = agg.feed(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"f","arguments":"not-json"}}
        ]}}]}));
        events.extend(agg.feed(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})));
        events.extend(agg.finish());
        assert!(events.iter().any(|e| matches!(
            e,
            ProviderStreamEvent::ToolCallEnd { arguments, .. } if arguments.is_object() && arguments.as_object().unwrap().is_empty()
        )));
    }

    #[test]
    fn parse_openai_models_extracts_data_list() {
        let json = json!({"object":"list","data":[{"id":"gpt-5"},{"id":"deepseek-chat","object":"model"}]});
        let models = parse_openai_models(&json);
        assert_eq!(models, vec![("gpt-5".to_string(), "gpt-5".to_string()),
                               ("deepseek-chat".to_string(), "deepseek-chat".to_string())]);
    }

    #[test]
    fn stream_error_detection() {
        let err = json!({"error": {"message": "rate limit exceeded", "type": "rate_limit_error"}});
        assert_eq!(
            extract_stream_error(&err),
            Some("rate limit exceeded".to_string())
        );
        let ok = json!({"choices":[{"delta":{"content":"x"}}]});
        assert_eq!(extract_stream_error(&ok), None);
        // error 无 message 字段时给兜底文案
        let bare = json!({"error": {"code": 500}});
        assert_eq!(
            extract_stream_error(&bare),
            Some("unknown stream error".to_string())
        );
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers 2>&1 | Select-String -Pattern "error"`
Expected: 编译错误（`OpenAiStreamAggregator`、`parse_openai_models` 未定义）。

- [ ] **Step 3: 实现聚合器与流式管线**

追加类型与函数（openai.rs）：

```rust
#[derive(Debug)]
struct OpenToolCall {
    index: usize,
    id: String,
    name: String,
    args: String,
}

/// OpenAI Chat Completions SSE 分片聚合器（spec §3.2）。
/// feed 逐 chunk 产出事件；finish 在流结束时收尾（关未闭合的
/// tool_calls、发 Finish）。usage 从任意带 usage 的 chunk 累积，
/// 取不到则 0/0。
#[derive(Debug)]
struct OpenAiStreamAggregator {
    open_calls: Vec<OpenToolCall>,
    stop_reason: Option<ProviderStopReason>,
    usage: Usage,
}

impl OpenAiStreamAggregator {
    fn new() -> Self {
        Self {
            open_calls: Vec::new(),
            stop_reason: None,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        }
    }

    fn end_event(call: OpenToolCall) -> ProviderStreamEvent {
        let arguments = serde_json::from_str::<Value>(&call.args)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        ProviderStreamEvent::ToolCallEnd {
            id: call.id,
            arguments,
        }
    }

    fn close_all(&mut self) -> Vec<ProviderStreamEvent> {
        std::mem::take(&mut self.open_calls)
            .into_iter()
            .map(Self::end_event)
            .collect()
    }

    fn feed(&mut self, chunk: &Value) -> Vec<ProviderStreamEvent> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            self.usage.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
        }
        if let Some(reason) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
        {
            self.stop_reason = Some(stop_reason_from_openai(reason));
            events.extend(self.close_all());
        }
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return events;
        };
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                events.push(ProviderStreamEvent::TextDelta {
                    delta: text.to_string(),
                });
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in calls {
                let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let slot = self.open_calls.iter().position(|c| c.index == index);
                let slot_idx = match slot {
                    Some(i) => i,
                    None => {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        events.push(ProviderStreamEvent::ToolCallStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        self.open_calls.push(OpenToolCall {
                            index,
                            id,
                            name,
                            args: String::new(),
                        });
                        self.open_calls.len() - 1
                    }
                };
                if let Some(args) = tc
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    let call = &mut self.open_calls[slot_idx];
                    call.args.push_str(args);
                    events.push(ProviderStreamEvent::ToolCallDelta {
                        id: call.id.clone(),
                        args_delta: args.to_string(),
                    });
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<ProviderStreamEvent> {
        let mut events = self.close_all();
        events.push(ProviderStreamEvent::Finish {
            stop_reason: self.stop_reason.take().unwrap_or(ProviderStopReason::EndTurn),
            usage: self.usage,
        });
        events
    }
}
```

`chat_stream` 替换 Task 4 的占位实现：

```rust
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        config: &GenerateConfig,
    ) -> Result<ChatStream, ProviderError> {
        let request = Self::build_request(model, messages, tools, config, Some(true));
        tracing::info!(
            "openai chat_stream: provider={}, model={}, messages={}",
            self.provider_id,
            model,
            messages.len()
        );
        let response = self.send_request(&request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            if let Err(e) = parse_sse_stream(response, tx).await {
                tracing::error!("openai SSE stream error: {}", e);
            }
        });
        Ok(ChatStream { inner: rx })
    }
```

SSE 解析任务（文件级自由函数）：

```rust
async fn parse_sse_stream(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
) -> Result<(), ProviderError> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut aggregator = OpenAiStreamAggregator::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::StreamError(e.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(event_end) = buffer.find("\n\n") {
            let event_text = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            for line in event_text.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(json) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(msg) = extract_stream_error(&json) {
                    tracing::error!("OpenAI stream error: {}", msg);
                    return Ok(());
                }
                for event in aggregator.feed(&json) {
                    let _ = tx.send(event).await;
                }
            }
        }
    }

    for event in aggregator.finish() {
        let _ = tx.send(event).await;
    }
    Ok(())
}
```

`list_models` 替换 Task 4 的实现：

```rust
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        match self.fetch_remote_models().await {
            Ok(remote) => {
                Ok(crate::models::merge_models(remote, &self.config_models, &self.provider_id))
            }
            Err(e) => {
                tracing::warn!(
                    "openai list_models 远端拉取失败({e})，回退配置 models"
                );
                Ok(self
                    .config_models
                    .iter()
                    .map(|m| crate::models::entry_to_model_info(m, &self.provider_id))
                    .collect())
            }
        }
    }

    async fn fetch_remote_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = crate::retry::with_retry(|| self.get_models_page(&url)).await?;
        let json: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        Ok(parse_openai_models(&json)
            .into_iter()
            .map(|(id, name)| ModelInfo {
                id,
                name,
                provider: self.provider_id.clone(),
                context_window: 0,
                max_output_tokens: 0,
            })
            .collect())
    }

    async fn get_models_page(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let mut builder = self.client.get(url);
        if let Some(auth) = Self::auth_header(&self.api_key) {
            builder = builder.header("authorization", auth);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5000);
            return Err(ProviderError::RateLimited {
                retry_after_ms: retry_after,
            });
        }
        if status.is_client_error() || status.is_server_error() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }
```

纯解析函数（文件级自由函数）：

```rust
/// 解析 GET /models 响应：(id, name) 列表。openai 无 display name，name 取 id。
fn parse_openai_models(json: &Value) -> Vec<(String, String)> {
    json.get("data")
        .and_then(|v| v.as_array())
        .map(|data| {
            data.iter()
                .filter_map(|m| {
                    m.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| (id.to_string(), id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 识别流中的 error 事件体（openai 部分兼容服务流中发 JSON error）。
fn extract_stream_error(json: &Value) -> Option<String> {
    let error = json.get("error")?;
    Some(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown stream error")
            .to_string(),
    )
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/openai.rs
git commit -m "feat(providers): OpenAI SSE 流解析（并行 tool_calls 聚合）+ list_models 远端拉取"
```

---

### Task 6: register_all 按 protocol 分发

**Files:**
- Modify: `crates/parrot-providers/src/lib.rs`

**Interfaces:**
- Consumes: Task 3/4/5 的两个 provider 构造签名
- Produces: `register_all(registry: &ProviderRegistry, config: &AppConfig)`：`protocol == "anthropic"` → AnthropicProvider；`protocol == "openai"` → OpenAiProvider；未知值 warn + 跳过

- [ ] **Step 1: 写失败测试（lib.rs `#[cfg(test)]`）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use parrot_config::ProviderConfig;

    fn provider(id: &str, protocol: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            protocol: protocol.to_string(),
            api_key: String::new(),
            default_model: format!("{id}-model"),
            base_url: None,
            models: vec![format!("{id}-model").into()],
        }
    }

    #[tokio::test]
    async fn openai_protocol_registers_openai_provider() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("ds", "openai"));
        register_all(&registry, &config).await;
        let p = registry.resolve("ds-model").await.expect("resolved");
        assert_eq!(p.provider_id(), "ds");
    }

    #[tokio::test]
    async fn anthropic_protocol_registers_anthropic_provider() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("claude-proxy", "anthropic"));
        register_all(&registry, &config).await;
        let p = registry.resolve("claude-proxy-model").await.expect("resolved");
        assert_eq!(p.provider_id(), "claude-proxy");
    }

    #[tokio::test]
    async fn unknown_protocol_is_skipped() {
        let registry = ProviderRegistry::new();
        let mut config = AppConfig::default_config();
        config.providers.push(provider("x", "gemini"));
        register_all(&registry, &config).await;
        assert!(registry.provider_ids().await.is_empty());
    }
}
```

lib.rs 头部 import 需要：

```rust
use parrot_config::AppConfig;
```

（已有。测试模块需要 `ProviderRegistry` 与 `ProviderConfig` 导入。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-providers 2>&1 | Select-String -Pattern "openai_protocol|error"`
Expected: 编译错误或 `openai_protocol_registers_openai_provider` 失败（当前 openai 分支仍 warn 跳过）。

- [ ] **Step 3: 实现（替换 register_all 全部 match）**

```rust
pub async fn register_all(registry: &ProviderRegistry, config: &AppConfig) {
    for provider_config in &config.providers {
        let models = if provider_config.models.is_empty() {
            vec![provider_config.default_model.clone()]
        } else {
            provider_config
                .models
                .iter()
                .map(|m| m.id().to_string())
                .collect()
        };
        match provider_config.protocol.as_str() {
            "anthropic" => {
                let provider = crate::anthropic::AnthropicProvider::new(
                    provider_config.id.clone(),
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.models.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
            "openai" => {
                let provider = crate::openai::OpenAiProvider::new(
                    provider_config.id.clone(),
                    provider_config.api_key.clone(),
                    provider_config.base_url.clone(),
                    provider_config.models.clone(),
                );
                registry.register(Arc::new(provider), models).await;
            }
            other => {
                tracing::warn!("Unknown protocol: {}, skipping provider {}", other, provider_config.id);
            }
        }
    }
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-providers && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-providers/src/lib.rs
git commit -m "feat(providers): register_all 按 protocol 分发（anthropic|openai）"
```

---

### Task 7: engine.rs — context_window=0 视为未知

**Files:**
- Modify: `crates/parrot-core/src/engine.rs:319-328`

**Interfaces:**
- Consumes: `ModelInfo`（现有）
- Produces: `model_context_window(models: &[ModelInfo], model: &str) -> Option<u32>`（纯函数，私有）；`resolve_model_context_window` 委托它

- [ ] **Step 1: 写失败测试（engine.rs 末尾追加 `#[cfg(test)]` 模块）**

```rust
#[cfg(test)]
mod context_window_tests {
    use super::*;

    fn model(id: &str, cw: u32) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            provider: "p".into(),
            context_window: cw,
            max_output_tokens: 0,
        }
    }

    #[test]
    fn known_window_returned() {
        let models = vec![model("m1", 200_000)];
        assert_eq!(model_context_window(&models, "m1"), Some(200_000));
    }

    #[test]
    fn zero_window_treated_as_unknown() {
        let models = vec![model("m1", 0)];
        assert_eq!(model_context_window(&models, "m1"), None);
    }

    #[test]
    fn unknown_model_returns_none() {
        let models = vec![model("m1", 200_000)];
        assert_eq!(model_context_window(&models, "m2"), None);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p parrot-core 2>&1 | Select-String -Pattern "model_context_window|error"`
Expected: 编译错误（函数不存在）。

- [ ] **Step 3: 实现（engine.rs:319-328 替换）**

```rust
    /// 模型的 context window（由其 provider 上报），未知时为 None。
    /// `context_window == 0` 表示远端/配置均未知，视为未收录，
    /// 保持配置预算不变（spec §3.5）。
    async fn resolve_model_context_window(&self) -> Option<u32> {
        let provider = self.provider_registry.resolve(&self.config.model).await?;
        let models = provider.list_models().await.ok()?;
        model_context_window(&models, &self.config.model)
    }
```

同文件（`resolve_model_context_window` 上方，文件级私有函数）：

```rust
fn model_context_window(models: &[ModelInfo], model: &str) -> Option<u32> {
    models
        .iter()
        .find(|m| m.id == model && m.context_window > 0)
        .map(|m| m.context_window)
}
```

注意 `ModelInfo` import 已存在（engine.rs 现有 `use crate::types::...`——若未引入 `ModelInfo` 则补进现有 use 列表）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p parrot-core && cargo build --workspace`
Expected: 全 PASS、编译通过。

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-core/src/engine.rs
git commit -m "feat(engine): context_window=0 视为未知，保持配置预算"
```

---

### Task 8: 全量验证

**Files:** 无新改动（纯验证；若发现回归则修复后重跑）

- [ ] **Step 1: 全量测试**

Run: `cargo test --workspace`
Expected: 全 PASS（含 e2e / phase15 / cassette，无需网络）。

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: 无警告。

- [ ] **Step 3: Format**

Run: `cargo fmt --all -- --check`
Expected: 无 diff。如有 diff 则 `cargo fmt --all` 后重跑 Step 1-3。

- [ ] **Step 4: spec 核对**

对照 `docs/superpowers/specs/2026-09-12-multi-provider-design.md` §3.1-§3.6 与 §4 逐条确认已实现；确认 spec §5 文件清单与实际改动一致。

- [ ] **Step 5: 如有修复则 Commit**

```bash
git add -A
git commit -m "fix: 多 provider 接入全量验证修复"
```

（无修复则跳过。）
