use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub daemon: DaemonConfig,
    pub providers: Vec<ProviderConfig>,
    pub tools: ToolsConfig,
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
    pub denylist: Vec<String>,
    pub require_confirmation: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub data_dir: String,
    pub max_history_tokens: u32,
    pub keep_recent_turns: u32,
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
            if let Some(config_dir) = dirs::config_dir() {
                self.daemon.auth_token_file = config_dir.join("parrot").join("token").to_string_lossy().to_string();
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
                    denylist: vec!["rm -rf /".into(), "sudo".into(), "chmod 777".into()],
                    require_confirmation: vec!["git push".into(), "rm".into()],
                },
            },
            session: SessionConfig {
                data_dir: String::new(),
                max_history_tokens: 100_000,
                keep_recent_turns: 6,
            },
        }
    }
}