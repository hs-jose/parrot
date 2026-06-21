use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    FileNotFound(String),
    #[error("failed to parse config: {0}")]
    ParseError(#[from] toml::de::Error),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("environment variable not found: {0}")]
    EnvVarNotFound(String),
}