use parrot_config::AppConfig;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 子进程守护：Drop 时终止 parrotd，避免遗留孤儿进程占用端口。
pub(crate) struct DaemonChild {
    inner: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl DaemonChild {
    /// 终止子进程。幂等，可多次调用。
    pub fn kill(&self) {
        let mut slot = self.inner.lock().unwrap();
        if let Some(child) = slot.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *slot = None;
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 起一个 parrotd 子进程绑本机随机空闲端口，轮询直到它能接受连接。
/// 返回连接地址和 kill-on-drop 守卫。端口由 CLI 先 `bind 127.0.0.1:0`
/// 拿到再通过 `PARROTD_PORT` env 传给子进程。CWD 继承自当前进程，所以
/// parrotd 和 parrot 找到同一份 parrot.toml。
pub(crate) async fn ensure_running(
    _config: &AppConfig,
) -> Result<(String, DaemonChild), Box<dyn std::error::Error>> {
    let port = pick_free_port()?;
    let connect_url = format!("ws://127.0.0.1:{port}");
    let addr = format!("127.0.0.1:{port}");

    let daemon_path =
        daemon_binary_path().ok_or("Failed to locate parrotd executable next to parrot")?;
    if !daemon_path.exists() {
        return Err(format!("parrotd executable not found at {}", daemon_path.display()).into());
    }

    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let mut cmd = std::process::Command::new(&daemon_path);
    cmd.stdout(log_file.try_clone()?).stderr(log_file);
    cmd.env("PARROTD_PORT", port.to_string());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to start parrotd ({}): {e}. Log: {}",
            daemon_path.display(),
            log_path.display()
        )
    })?;

    let guard = DaemonChild {
        inner: Arc::new(std::sync::Mutex::new(Some(child))),
    };

    let wait_result = timeout(DAEMON_START_TIMEOUT, async {
        loop {
            if probe_port(&addr).await {
                return;
            }
            sleep(DAEMON_POLL_INTERVAL).await;
        }
    })
    .await;

    match wait_result {
        Ok(()) => Ok((connect_url, guard)),
        Err(_) => Err(format!(
            "parrotd did not become reachable within {:?}. Check log: {}",
            DAEMON_START_TIMEOUT,
            log_path.display()
        )
        .into()),
    }
}

/// 绑一个 `127.0.0.1:0`，立即丢弃，让 OS 把该端口交回。
fn pick_free_port() -> Result<u16, std::io::Error> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// TCP 探测 daemon 端口是否已开。
async fn probe_port(addr: &str) -> bool {
    TcpStream::connect(addr).await.is_ok()
}

/// 返回与当前 parrot 二进制同目录的 parrotd 可执行文件路径。
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

/// Daemon 日志路径，落在用户数据目录（Windows 是 %LOCALAPPDATA%）。
pub fn daemon_log_path() -> PathBuf {
    dirs::data_dir()
        .map(|b| b.join("parrot"))
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
    fn daemon_log_path_landed_in_user_data_dir() {
        let log = daemon_log_path();
        assert_eq!(log.file_name().unwrap(), "daemon.log");
        let s = log.to_string_lossy();
        assert!(s.contains("parrot"), "log path should include parrot: {s}");
    }

    #[test]
    fn pick_free_port_returns_in_ephemeral_range() {
        let p = pick_free_port().expect("bind 127.0.0.1:0");
        assert!(p > 1024, "ephemeral port should be > 1024: {p}");
    }

    #[tokio::test]
    async fn probe_port_detects_closed_port() {
        // 64738 is extremely unlikely to be open in a test environment.
        assert!(!probe_port("127.0.0.1:64738").await);
    }

    #[test]
    fn daemon_child_kill_is_idempotent_and_safe() {
        let guard = DaemonChild {
            inner: Arc::new(std::sync::Mutex::new(None)),
        };
        guard.kill();
        guard.kill();
    }
}
