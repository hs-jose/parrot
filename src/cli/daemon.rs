use parrot_config::AppConfig;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Ensures the parrotd process is running and reachable.
pub async fn ensure_running(config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", config.daemon.host, config.daemon.port);

    if probe_port(&addr).await {
        return Ok(());
    }

    let daemon_path =
        daemon_binary_path().ok_or("Failed to locate parrotd executable next to parrot")?;

    if !daemon_path.exists() {
        return Err(format!("parrotd executable not found at {}", daemon_path.display()).into());
    }

    let log_path = daemon_log_path(config);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let mut cmd = std::process::Command::new(&daemon_path);
    cmd.stdout(log_file.try_clone()?)
        .stderr(log_file)
        .current_dir(daemon_path.parent().unwrap_or_else(|| Path::new(".")));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let _child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to start parrotd ({}): {e}. Log: {}",
            daemon_path.display(),
            log_path.display()
        )
    })?;

    let wait_result = timeout(DAEMON_START_TIMEOUT, async {
        loop {
            if probe_port(&addr).await {
                return Ok(());
            }
            sleep(DAEMON_POLL_INTERVAL).await;
        }
    })
    .await;

    match wait_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "parrotd did not become reachable within {:?}. Check log: {}",
            DAEMON_START_TIMEOUT,
            log_path.display()
        )
        .into()),
    }
}

/// TCP probe to check if the daemon port is open.
async fn probe_port(addr: &str) -> bool {
    TcpStream::connect(addr).await.is_ok()
}

/// Returns the path to the parrotd executable next to the current parrot binary.
pub fn daemon_binary_path() -> Option<PathBuf> {
    let mut exe = std::env::current_exe().ok()?;
    exe.pop();
    let name = if cfg!(windows) {
        "parrotd.exe"
    } else {
        "parrotd"
    };
    Some(exe.join(name))
}

/// Returns the path for daemon logs.
///
/// If `config.session.data_dir` is empty or relative, falls back to the
/// platform-appropriate user data directory (e.g. `%LOCALAPPDATA%/parrot`
/// on Windows, `$XDG_DATA_HOME/parrot` on Unix).
pub fn daemon_log_path(config: &AppConfig) -> PathBuf {
    let base = if config.session.data_dir.is_empty() {
        dirs::data_dir()
    } else {
        let p = Path::new(&config.session.data_dir);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            dirs::data_dir()
        }
    };
    base.map(|b| b.join("parrot"))
        .unwrap_or_else(|| PathBuf::from("parrot"))
        .join("daemon.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_binary_path_resolves_sibling() {
        let path = daemon_binary_path().expect("current_exe should resolve");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name == "parrotd" || name == "parrotd.exe");
    }

    #[test]
    fn daemon_log_path_uses_data_dir() {
        let config = AppConfig::default_config();
        let log = daemon_log_path(&config);
        assert_eq!(log.file_name().unwrap(), "daemon.log");
    }

    #[tokio::test]
    async fn probe_port_detects_closed_port() {
        // 64738 is extremely unlikely to be open in a test environment.
        assert!(!probe_port("127.0.0.1:64738").await);
    }
}
