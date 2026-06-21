use parrot_config::AppConfig;
use tracing_subscriber::EnvFilter;

mod auth;
mod providers;
mod server;
mod session_adapter;
mod tools;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("parrotd=info".parse()?))
        .init();

    let config = AppConfig::load().map_err(|e| format!("Config error: {}", e))?;
    server::run(config).await
}