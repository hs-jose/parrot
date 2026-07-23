use parrot_config::AppConfig;
use parrot_daemon::run;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("parrotd=info".parse()?))
        .init();

    let mut config = AppConfig::load().map_err(|e| format!("Config error: {e}"))?;
    // 允许父进程（CLI）通过 env 覆盖端口，避免多个 parrotd 抢同一固定端口
    // 造成跨项目串配置。CLI 起子进程前会先占 `127.0.0.1:0` 拿一个空闲
    // 端口，再传入这里。
    if let Ok(port_str) = std::env::var("PARROTD_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            config.daemon.port = port;
        }
    }
    run(config).await
}
