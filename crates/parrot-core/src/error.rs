use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool execution failed: {tool} - {message}")]
    ToolExecution { tool: String, message: String },
    #[error("session not found: {0}")]
    SessionNotFound(uuid::Uuid),
    #[error("context window exceeded")]
    ContextWindowExceeded,
    #[error("config error: {0}")]
    Config(String),
    #[error("session aborted")]
    Aborted,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("timeout after {0}ms")]
    Timeout(u64),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("stream error: {0}")]
    StreamError(String),
}