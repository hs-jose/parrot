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
    #[serde(rename = "session")]
    pub session: SessionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub host: String,
    pub port: u16,
    pub auth_token_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub api_key: String,
    pub default_model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
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
}
