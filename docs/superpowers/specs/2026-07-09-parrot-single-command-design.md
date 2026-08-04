# Parrot Single-Command Auto-Start & Local Distribution Design

> **⚠ SUPERSEDED (部分).** 此 spec 描述的是 v1 方案:固定端口 9876 单例
> daemon,CLI 探测复用,daemon 跨调用存活。该方案被 commit `57bca3e`
> 取代 —— 改为 opencode 式 per-invocation 子进程,绑随机端口,CLI 退出即
> kill,根除多项目并发串配置的问题。打包 / 安装脚本部分(spec §3-§5)
> 仍大体适用,但已扩展为支持 `-Target <triple>` 交叉编译(commit `d0f5f53`)。
> **当前实现以代码为准,本 spec 仅作历史参考。**

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `parrot` should be the only command a user needs: it automatically ensures `parrotd` is running before any operation that talks to the daemon, and the daemon stays alive after the CLI exits. Additionally, provide local distribution artifacts (directory, zip, install script) for the two binaries.

**Architecture:** Introduce a small daemon-lifecycle manager in the CLI crate (`src/cli/daemon.rs`) that probes the configured WebSocket endpoint and, if unreachable, spawns `parrotd` from the same directory as the `parrot` binary with output redirected to a log file. All connection entry points call this manager before reading the token and connecting. Packaging is handled by shell/PowerShell scripts that build a release binary tree, create an archive, and optionally install to a user-local directory.

**Tech Stack:** Rust, `std::process::Command`, `tokio::time` for polling, `parrot-config` for daemon host/port/data paths, crossterm/Windows `CREATE_NO_WINDOW` for hidden daemon spawn, PowerShell/Bash for packaging.

## Global Constraints

- `parrot-core` must remain zero-IO; all process spawning and file IO lives in the CLI/daemon binaries.
- The auth token file path is read from `parrot.toml` (`daemon.auth_token_file`) and created by the daemon on first start.
- The daemon WebSocket address is read from `parrot.toml` (`daemon.host`, `daemon.port`).
- Existing behavior when the daemon is already running must be unchanged.
- The daemon process outlives the CLI process that started it.
- Packaging scripts must not require external Rust tooling beyond `cargo`.

---

## 1. Daemon Lifecycle Manager

### 1.1 Responsibility

`src/cli/daemon.rs` owns:

- Probing whether the daemon is reachable on its configured WS endpoint.
- Locating the `parrotd` executable next to the running `parrot` binary.
- Spawning `parrotd` as a hidden background process.
- Waiting for the daemon to become reachable (with timeout).
- Redirecting daemon stdout/stderr to a log file in the configured data directory.

It does **not** kill the daemon when the CLI exits.

### 1.2 Public Interface

```rust
use parrot_config::AppConfig;
use std::path::PathBuf;

/// Ensures the parrotd process is running and reachable.
///
/// Returns Ok(()) if the daemon is already running or was successfully started.
/// Returns an error if the daemon could not be found, spawned, or did not become
/// reachable within the timeout.
pub async fn ensure_running(config: &AppConfig) -> Result<(), Box<dyn std::error::Error>>;

/// Returns the path to the parrotd executable that lives next to the current
/// parrot binary, if it exists.
pub fn daemon_binary_path() -> Option<PathBuf>;

/// Returns the path used for daemon stdout/stderr logs.
pub fn daemon_log_path(config: &AppConfig) -> PathBuf;
```

### 1.3 Probe Behavior

`ensure_running` performs a lightweight reachability check before attempting to spawn:

1. Construct the address from `config.daemon.host` and `config.daemon.port`.
2. Perform a lightweight TCP connect to the daemon port. We only need to know the daemon is listening; a full WebSocket handshake or auth is unnecessary.
3. If the TCP connect succeeds, return immediately.
4. If the connect fails, proceed to spawn.

A TCP probe avoids requiring the auth token before the daemon has created it, and it is faster than a full WS handshake.

### 1.4 Spawn Behavior

If the probe fails:

1. Resolve `parrotd` via `daemon_binary_path()`:
   - `std::env::current_exe()` gives the `parrot` executable path.
   - Return the sibling file named `parrotd` (or `parrotd.exe` on Windows).
2. If the sibling does not exist, return an error instructing the user to run `parrotd` manually or ensure both binaries are in the same directory.
3. Ensure the daemon data directory exists (`config.session.data_dir` parent and `daemon.auth_token_file` parent). This avoids first-start races where the daemon tries to create nested dirs.
4. Open the log file at `daemon_log_path(config)` for append.
5. Spawn `parrotd` with:
   - Current working directory set to the `parrot` binary directory.
   - stdout/stderr redirected to the log file.
   - On Windows: use `CREATE_NO_WINDOW` so no console window appears.
   - On Unix: use `std::process::Command::spawn` with output redirected; the process is automatically reparented.
6. Poll the WS endpoint every 200 ms, up to 10 seconds.
7. If reachable, return `Ok(())`.
8. If timeout expires, return an error that includes the log file path.

### 1.5 Log Path

```text
Windows:   %LOCALAPPDATA%\parrot\daemon.log
Unix:      $XDG_DATA_HOME/parrot/daemon.log  (fallback ~/.local/share/parrot/daemon.log)
```

These paths are derived from `config.session.data_dir` if it is an absolute path; otherwise they fall back to platform-appropriate user data directories.

## 2. CLI Integration Points

All commands that open a connection must call `daemon::ensure_running(config).await?` before `connect_with_token`.

Entry points to update:

- `run_default` (`src/cli/main.rs:253`)
- `run_sessions` (`src/cli/main.rs:293`)
- `run_models` (`src/cli/main.rs:389`)
- `run_tools` (`src/cli/main.rs:413`)

`connect_with_token` itself should **not** call `ensure_running`, to keep connection code pure and avoid recursion.

Example updated flow for `run_default`:

```rust
async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    crate::daemon::ensure_running(config).await?;
    let mut conn = connect_with_token(&cli.connect, &token_path).await?;
    // ... existing logic
}
```

The `--connect` CLI flag remains available for users who want to target a remote or manually managed daemon. `ensure_running` uses the configured `host:port`, not the `--connect` URL, because `--connect` may point to a remote host where auto-start is impossible.

## 3. Local Distribution Packaging

### 3.1 Directory Layout

After running the packaging script:

```text
dist/
  parrot-<target>/
    bin/
      parrot[.exe]
      parrotd[.exe]
    config/
      parrot.toml
    README.md
  parrot-<target>.zip      # Windows
  parrot-<target>.tar.gz   # Unix
```

The packaged `parrot.toml` should be a minimal default that uses user-local data paths:

```toml
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "{user_data_dir}/parrot/token"

[session]
data_dir = "{user_data_dir}/parrot/sessions"

[providers]
# Users fill in their provider config here.
```

`{user_data_dir}` is replaced by the packaging script with the platform-specific path (e.g., `%LOCALAPPDATA%` on Windows, `$HOME/.local/share` on Linux).

### 3.2 Scripts

- `scripts/package.ps1` — Windows packaging.
- `scripts/package.sh` — Unix packaging (Linux/macOS).

Both scripts:

1. Run `cargo build --release --workspace`.
2. Create the directory layout under `dist/`.
3. Copy binaries, config, and README.
4. Create the archive (`zip` or `tar.gz`).
5. Print the resulting paths.

### 3.3 Install Scripts

- `scripts/install.ps1` — installs to `%USERPROFILE%\.parrot` and prints PATH instructions.
- `scripts/install.sh` — installs to `$HOME/.parrot` or `$HOME/.local/bin` and prints PATH instructions.

Both scripts:

1. Detect target triple and pick the correct archive, or build from source if no archive matches.
2. Extract binaries to the install bin directory.
3. Write a default `parrot.toml` if none exists.
4. Print instructions to add the bin directory to PATH.

## 4. Error Handling

- Daemon binary not found: clear error message showing where the CLI looked.
- Daemon spawn failed: include OS error and log path.
- Daemon did not become reachable: include timeout, log path, and suggestion to check the log.
- Probe connection succeeds but auth later fails: this is an existing error path and is unchanged.

## 5. Testing

### 5.1 Unit Tests (where feasible)

- `daemon_binary_path` resolves the sibling executable on the current platform.
- `daemon_log_path` returns a platform-appropriate absolute path.

### 5.2 Manual/Integration Tests

1. Build release binaries.
2. Ensure no daemon is running.
3. Run `parrot sessions list` — verify `parrotd` starts in the background and the command succeeds.
4. Exit `parrot` and verify `parrotd` is still running.
5. Run `parrot -m "hello"` again — verify it reuses the existing daemon.
6. Run the packaging script and verify the archive contents.
7. Run the install script in a clean VM/container and verify `parrot` is on PATH.

## 6. Backwards Compatibility

- Users who already run `parrotd` manually are unaffected: the probe succeeds and no new process is spawned.
- Users who pass `--connect` to a remote daemon are unaffected: auto-start only applies to the locally configured host/port.
- The auth token file creation remains the daemon's responsibility.

## 7. Future Work (out of scope)

- `parrot daemon stop` / `parrot daemon status` subcommands.
- Windows service registration or macOS `launchd` plist.
- `cargo-dist` cross-platform GitHub Releases pipeline.
