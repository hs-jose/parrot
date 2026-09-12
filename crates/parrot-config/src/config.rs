use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub daemon: DaemonConfig,
    pub providers: Vec<ProviderConfig>,
    pub tools: ToolsConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(rename = "session")]
    pub session: SessionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub host: String,
    pub port: u16,
    pub auth_token_file: String,
}

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
    /// 思考模式扩展点（anthropic 协议）。请求期注入 anthropic 请求体。
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    /// effort 扩展点（openai 协议）。请求期注入 openai 请求体。
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
    /// 会话默认请求 max_tokens（daemon 用第一个 provider 的该值构造
    /// GenerateConfig；缺省 8192）。thinking budget 需小于此值。
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub shell_allowed: bool,
    pub file_write_allowed: bool,
    pub web_allowed: bool,
    pub max_file_size_mb: u64,
    pub sandbox: SandboxConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub working_dir: String,
    pub allowlist: Vec<String>,
    pub require_confirmation: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_mcp_startup_timeout() -> u64 {
    30
}
fn default_mcp_call_timeout() -> u64 {
    120
}
fn default_compaction_threshold() -> f32 {
    0.9
}
fn default_keep_recent_tokens() -> u32 {
    20_000
}
fn default_summary_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub data_dir: String,
    pub max_history_tokens: u32,
    pub keep_recent_turns: u32,
    /// 结构化摘要压缩总开关(spec §5)。
    #[serde(default = "default_true")]
    pub compaction: bool,
    /// 估算/budget 触发比例。
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    /// 切点保留预算(token 估算)。
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u32,
    /// 摘要调用 max_tokens 上限。
    #[serde(default = "default_summary_max_tokens")]
    pub summary_max_tokens: u32,
}

fn default_hook_timeout() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_seconds: u64,
    /// Per-hook config subtables for built-in hooks. Populated from
    /// `[hooks.<id>]` TOML tables via `#[serde(flatten)]`. Each hook owns
    /// its typed config struct and deserializes its entry from this map.
    #[serde(default, flatten)]
    pub configs: HashMap<String, toml::Value>,
    /// External (fork-based) hook entries from `[[hooks.external]]`
    /// subtables. Listed ⇒ enabled; orthogonal to `enabled` above which
    /// only governs built-in hook ids.
    #[serde(default)]
    pub external: Vec<ExternalHookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: vec![
                "shell_denylist".to_string(),
                "dangerous_command_blocker".to_string(),
            ],
            timeout_seconds: default_hook_timeout(),
            configs: HashMap::new(),
            external: Vec::new(),
        }
    }
}

/// One `[[hooks.external]]` entry. Daemon spawns `command` per event,
/// feeds the serialized `HookEvent` (as JSON envelope) on stdin, and
/// parses the last non-empty stdout line as a `HookAction` JSON.
/// Failure to spawn / non-zero exit / non-JSON / unknown action ⇒ fail-open NoOp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalHookConfig {
    pub id: String,
    pub command: Vec<String>,
    pub events: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default = "default_external_config")]
    pub config: toml::Value,
}

fn default_external_config() -> toml::Value {
    toml::Value::Table(toml::value::Table::new())
}

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

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let config_paths = Self::config_paths();
        for path in &config_paths {
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                let mut config: AppConfig = toml::from_str(&content)?;
                config.resolve_env_vars()?;
                config.resolve_data_dir()?;
                config.resolve_token_path()?;
                return Ok(config);
            }
        }
        let mut config = Self::default_config();
        config.resolve_data_dir()?;
        config.resolve_token_path()?;
        Ok(config)
    }

    pub fn load_from(path: &PathBuf) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let mut config: AppConfig = toml::from_str(&content)?;
        config.resolve_env_vars()?;
        config.resolve_data_dir()?;
        config.resolve_token_path()?;
        Ok(config)
    }

    fn config_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        paths.push(PathBuf::from("parrot.toml"));
        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("parrot").join("parrot.toml"));
        }
        paths
    }

    pub fn resolve_env_vars(&mut self) -> Result<(), ConfigError> {
        for provider in &mut self.providers {
            if provider.api_key.starts_with("${") && provider.api_key.ends_with('}') {
                let var_name = &provider.api_key[2..provider.api_key.len() - 1];
                if let Ok(value) = std::env::var(var_name) {
                    provider.api_key = value;
                }
            }
        }
        Ok(())
    }

    fn resolve_data_dir(&mut self) -> Result<(), ConfigError> {
        if self.session.data_dir.is_empty() {
            if let Some(data_dir) = dirs::data_dir() {
                self.session.data_dir = data_dir.join("parrot").to_string_lossy().to_string();
            }
        }
        Ok(())
    }

    fn resolve_token_path(&mut self) -> Result<(), ConfigError> {
        if self.daemon.auth_token_file.is_empty() {
            // 跟 resolve_data_dir / 打包模板对齐到 `dirs::data_dir()`
            // （Windows 上是 %LOCALAPPDATA%）。之前用 config_dir()(Roaming)
            // 会让 dev 和打包安装的 token 文件落到两个不同目录，握手失败。
            if let Some(data_dir) = dirs::data_dir() {
                self.daemon.auth_token_file = data_dir
                    .join("parrot")
                    .join("token")
                    .to_string_lossy()
                    .to_string();
            }
        }
        Ok(())
    }

    pub fn default_config() -> Self {
        Self {
            daemon: DaemonConfig {
                host: "127.0.0.1".into(),
                port: 9876,
                auth_token_file: String::new(),
            },
            providers: vec![],
            tools: ToolsConfig {
                shell_allowed: false,
                file_write_allowed: false,
                web_allowed: true,
                max_file_size_mb: 10,
                sandbox: SandboxConfig {
                    working_dir: ".".into(),
                    allowlist: vec![],
                    require_confirmation: vec!["git push".into(), "rm".into()],
                },
            },
            session: SessionConfig {
                data_dir: String::new(),
                max_history_tokens: 100_000,
                keep_recent_turns: 6,
                compaction: true,
                compaction_threshold: 0.9,
                keep_recent_tokens: 20_000,
                summary_max_tokens: 4096,
            },
            hooks: HooksConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_default_is_empty() {
        let c = HooksConfig::default();
        assert_eq!(
            c.enabled,
            vec!["shell_denylist", "dangerous_command_blocker"]
        );
        assert_eq!(c.timeout_seconds, 5);
    }

    #[test]
    fn sandbox_config_has_no_denylist() {
        let c = AppConfig::default_config();
        assert!(
            serde_json::to_value(&c.tools.sandbox)
                .unwrap()
                .get("denylist")
                .is_none(),
            "SandboxConfig must not expose a denylist field"
        );
    }

    #[test]
    fn hooks_default_enabled_includes_shell_denylist_and_blocker() {
        let c = HooksConfig::default();
        assert!(c.enabled.contains(&"shell_denylist".to_string()));
        assert!(c.enabled.contains(&"dangerous_command_blocker".to_string()));
    }

    #[test]
    fn hooks_parse_from_toml() {
        let toml = r#"
[hooks]
enabled = ["dangerous_command_blocker"]
timeout_seconds = 3

[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

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
        assert_eq!(c.hooks.enabled, vec!["dangerous_command_blocker"]);
        assert_eq!(c.hooks.timeout_seconds, 3);
    }

    #[test]
    fn hooks_parse_per_hook_configs() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

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

[hooks]
enabled = ["shell_denylist", "redact_secrets"]
timeout_seconds = 5

[hooks.shell_denylist]
patterns = ["rm -rf /", "sudo", "chmod 777"]

[hooks.redact_secrets]
extra_patterns = ["CUSTOM-\\d+"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.hooks.enabled, vec!["shell_denylist", "redact_secrets"]);
        assert!(c.hooks.configs.contains_key("shell_denylist"));
        assert!(c.hooks.configs.contains_key("redact_secrets"));
    }

    #[test]
    fn hooks_missing_configs_defaults_empty() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

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

[hooks]
enabled = ["dangerous_command_blocker"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert!(
            c.hooks.configs.is_empty(),
            "configs should be empty when no [hooks.<id>] subtables present"
        );
    }

    #[test]
    fn hooks_parse_redact_secrets_extra_patterns() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
protocol = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

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

[hooks]
enabled = ["redact_secrets"]

[hooks.redact_secrets]
extra_patterns = ["CUSTOM-\\d+", "MY-TOKEN-[a-z]+"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.hooks.enabled, vec!["redact_secrets"]);
        assert!(c.hooks.configs.contains_key("redact_secrets"));
        let rs_cfg = c.hooks.configs.get("redact_secrets").unwrap();
        let extra = rs_cfg
            .get("extra_patterns")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(extra.len(), 2);
    }

    #[test]
    fn mcp_servers_parse_from_toml() {
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
id = "anthropic"
protocol = "anthropic"
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
        assert!(
            c.mcp.servers.is_empty(),
            "无 [mcp] 段 ⇒ servers 为空，零行为变化"
        );
    }

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
    fn provider_max_tokens_parse_and_default() {
        let base = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

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
        let with_knob = format!(
            r#"{base}
[[providers]]
id = "a"
protocol = "anthropic"
api_key = "x"
default_model = "m"
max_tokens = 72000
"#
        );
        let c: AppConfig = toml::from_str(&with_knob).unwrap();
        assert_eq!(c.providers[0].max_tokens, Some(72_000));

        let without_knob = format!(
            r#"{base}
[[providers]]
id = "a"
protocol = "anthropic"
api_key = "x"
default_model = "m"
"#
        );
        let c: AppConfig = toml::from_str(&without_knob).unwrap();
        assert_eq!(
            c.providers[0].max_tokens, None,
            "缺省 None ⇒ runtime 回退 8192"
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
  { id = "deepseek-v4-flash[1m]", context_window = 1000000, max_output_tokens = 8192, thinking = { budget_tokens = 64000 } },
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
}
