use parrot_config::AppConfig;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 子进程守护：`parrot` 启动 `parrotd` 时持有此结构体，`parrot` 退出时
/// `Drop` 终止子进程，避免遗留长期占用端口的 daemon。
pub(crate) struct DaemonChild {
    inner: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl DaemonChild {
    /// 终止子进程并 wait 收尸。幂等：可多次调用。
    pub fn kill(&self) {
        if let Some(child) = self.inner.lock().unwrap().as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self.inner.lock().unwrap() = None;
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 启动一个 `parrotd` 子进程，监听本机一个空闲随机端口，直到它可接受
/// TCP 连接为止。返回 `(ws://127.0.0.1:<port>, DaemonChild)`。
///
/// 子进程的 CWD 继承自当前进程，因此 `parrotd` 加载 `parrot.toml` 的行为
/// 与 `parrot` 自身一致。子进程通过环境变量 `PARROTD_PORT` 接收应绑定的
/// 端口（由 parrot 这边先占用 `127.0.0.1:0` 拿到一个空闲端口再释放，
/// 立刻交给 parrotd；本地回环上竞争窗口极小）。
///
/// `parrot` 退出时 `DaemonChild` 经 Drop 终止子进程，因此根除了"长期
/// 复用固定端口单例 daemon"导致的多项目串配置问题（参考 opencode 的
/// 进程内 server + 退出即死的进程模型）。
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
                return Ok(());
            }
            sleep(DAEMON_POLL_INTERVAL).await;
        }
    })
    .await;

    match wait_result {
        Ok(Ok(())) => Ok((connect_url, guard)),
        Ok(Err(e)) => Err(e),
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

/// Returns the path for daemon logs. 使用平台用户数据目录，避免项目根被污染。
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
        // 应位于用户数据目录下；特定路径因平台/用户而异。
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
