use parrot_config::AppConfig;
use parrot_daemon::run;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("parrotd=info".parse()?))
        .init();

    let config = AppConfig::load().map_err(|e| format!("Config error: {e}"))?;
    run(config).await
}
