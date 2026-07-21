# Parrot Single-Command Auto-Start & Local Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `parrot` auto-starting `parrotd` before any daemon-requiring command, while leaving the daemon running after exit, and add local distribution packaging/install scripts.

**Architecture:** Add a `src/cli/daemon.rs` lifecycle manager that TCP-probes the configured port and spawns `parrotd` from the same directory as `parrot` when needed. Call it from every CLI entry point that opens a WebSocket connection. Add PowerShell/Bash scripts under `scripts/` to build, package, and install the binary pair.

**Tech Stack:** Rust, `tokio::net::TcpStream`, `std::process::Command`, Windows `CREATE_NO_WINDOW`, PowerShell 7+, Bash.

## Global Constraints

- `parrot-core` must remain zero-IO; all process spawning and file IO lives in the CLI/daemon binaries.
- The auth token file path is read from `parrot.toml` (`daemon.auth_token_file`) and created by the daemon on first start.
- The daemon WebSocket address is read from `parrot.toml` (`daemon.host`, `daemon.port`).
- Existing behavior when the daemon is already running must be unchanged.
- The daemon process outlives the CLI process that started it.
- Packaging scripts must not require external Rust tooling beyond `cargo`.

---

### Task 1: Create `src/cli/daemon.rs` lifecycle manager

**Files:**
- Create: `src/cli/daemon.rs`
- Modify: `src/cli/main.rs` (add `mod daemon;`)
- Test: unit tests inside `src/cli/daemon.rs`

**Interfaces:**
- Consumes: `parrot_config::AppConfig`, `tokio::net::TcpStream`, `std::process::Command`
- Produces:
  - `pub async fn ensure_running(config: &AppConfig) -> Result<(), Box<dyn std::error::Error>>`
  - `pub fn daemon_binary_path() -> Option<PathBuf>`
  - `pub fn daemon_log_path(config: &AppConfig) -> PathBuf`

- [ ] **Step 1: Create `src/cli/daemon.rs` with helper functions and failing unit tests**

```rust
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

    let daemon_path = daemon_binary_path()
        .ok_or("Failed to locate parrotd executable next to parrot")?;

    if !daemon_path.exists() {
        return Err(format!(
            "parrotd executable not found at {}",
            daemon_path.display()
        )
        .into());
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
    let name = if cfg!(windows) { "parrotd.exe" } else { "parrotd" };
    Some(exe.join(name))
}

/// Returns the path for daemon logs.
pub fn daemon_log_path(config: &AppConfig) -> PathBuf {
    Path::new(&config.session.data_dir).join("daemon.log")
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
```

- [ ] **Step 2: Run the new unit tests to verify they fail where expected**

Run:

```bash
cargo test --bin parrot daemon::
```

Expected: tests compile; `probe_port_detects_closed_port` passes; path tests pass.

- [ ] **Step 3: Register the new module in `src/cli/main.rs`**

Modify `src/cli/main.rs`:

```rust
mod conn;
mod daemon;
mod stream;
#[path = "../tui/mod.rs"]
mod tui;
```

- [ ] **Step 4: Run unit tests again**

Run:

```bash
cargo test --bin parrot daemon::
```

Expected: all 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cli/daemon.rs src/cli/main.rs
git commit -m "feat(cli): add daemon lifecycle manager for auto-start"
```

---

### Task 2: Wire `ensure_running` into CLI entry points

**Files:**
- Modify: `src/cli/main.rs`
- Test: manual integration test described in Task 5

**Interfaces:**
- Consumes: `crate::daemon::ensure_running`
- Produces: every daemon-requiring command now auto-starts `parrotd`

- [ ] **Step 1: Call `ensure_running` in each entry point**

Modify `src/cli/main.rs` so that the following functions call `daemon::ensure_running(config).await?` immediately after loading config and before creating a connection:

- `run_default`
- `run_sessions`
- `run_models`
- `run_tools`

Example for `run_default`:

```rust
async fn run_default(cli: Cli, config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    crate::daemon::ensure_running(config).await?;
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| config.daemon.auth_token_file.clone());
    let mut conn = connect_with_token(&cli.connect, &token_path).await?;
    // ... rest unchanged
}
```

Apply the same two-line insertion (`ensure_running` + unchanged `token_path`) to the other three functions.

- [ ] **Step 2: Build to verify compilation**

Run:

```bash
cargo build --bin parrot --bin parrotd
```

Expected: compiles without warnings.

- [ ] **Step 3: Commit**

```bash
git add src/cli/main.rs
git commit -m "feat(cli): auto-start parrotd before all daemon-requiring commands"
```

---

### Task 3: Add packaging scripts and default config template

**Files:**
- Create: `scripts/parrot.toml.template`
- Create: `scripts/package.ps1`
- Create: `scripts/package.sh`
- Modify: `README.md` (optional, document packaging)

**Interfaces:**
- Consumes: `cargo build --release`, binaries in `target/release/`
- Produces: `dist/parrot-<target>/` directory, `dist/parrot-<target>.zip`, `dist/parrot-<target>.tar.gz`

- [ ] **Step 1: Create default config template**

Create `scripts/parrot.toml.template`:

```toml
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "{USER_DATA_DIR}/parrot/token"

[session]
data_dir = "{USER_DATA_DIR}/parrot/sessions"
max_history_tokens = 100000
keep_recent_turns = 6

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
denylist = ["rm -rf /", "sudo", "chmod 777"]
require_confirmation = ["git push", "rm"]

[[providers]]
# id = "anthropic"
# api_key = "sk-..."
# default_model = "claude-sonnet-4-6"
```

- [ ] **Step 2: Create Windows packaging script**

Create `scripts/package.ps1`:

```powershell
#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$target = "x86_64-pc-windows-msvc"
$packageName = "parrot-$target"
$distDir = Join-Path $projectRoot "dist"
$stageDir = Join-Path $distDir $packageName

Write-Host "Building release binaries..."
& cargo build --release --workspace --manifest-path (Join-Path $projectRoot "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host "Staging distribution..."
if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
New-Item -ItemType Directory -Path (Join-Path $stageDir "bin") -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stageDir "config") -Force | Out-Null

Copy-Item (Join-Path $projectRoot "target" "release" "parrot.exe") (Join-Path $stageDir "bin")
Copy-Item (Join-Path $projectRoot "target" "release" "parrotd.exe") (Join-Path $stageDir "bin")
Copy-Item (Join-Path $projectRoot "README.md") $stageDir

$userDataDir = "$env:LOCALAPPDATA"
(Get-Content (Join-Path $projectRoot "scripts" "parrot.toml.template")) `
    -replace '\{USER_DATA_DIR\}', $userDataDir.Replace('\', '/') `
    | Set-Content (Join-Path $stageDir "config" "parrot.toml")

$zipPath = Join-Path $distDir "$packageName.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath }
Compress-Archive -Path "$stageDir\*" -DestinationPath $zipPath

Write-Host "Done:"
Write-Host "  Stage:  $stageDir"
Write-Host "  Zip:    $zipPath"
```

- [ ] **Step 3: Create Unix packaging script**

Create `scripts/package.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$(rustc -vV | sed -n 's|host: ||p')"
PACKAGE_NAME="parrot-$TARGET"
DIST_DIR="$PROJECT_ROOT/dist"
STAGE_DIR="$DIST_DIR/$PACKAGE_NAME"

echo "Building release binaries..."
cargo build --release --workspace --manifest-path "$PROJECT_ROOT/Cargo.toml"

echo "Staging distribution..."
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/config"

cp "$PROJECT_ROOT/target/release/parrot" "$STAGE_DIR/bin/"
cp "$PROJECT_ROOT/target/release/parrotd" "$STAGE_DIR/bin/"
cp "$PROJECT_ROOT/README.md" "$STAGE_DIR/"

USER_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
sed "s|{USER_DATA_DIR}|$USER_DATA_DIR|g" "$PROJECT_ROOT/scripts/parrot.toml.template" \
    > "$STAGE_DIR/config/parrot.toml"

TARBALL="$DIST_DIR/$PACKAGE_NAME.tar.gz"
rm -f "$TARBALL"
tar -czf "$TARBALL" -C "$DIST_DIR" "$PACKAGE_NAME"

echo "Done:"
echo "  Stage:  $STAGE_DIR"
echo "  Tarball: $TARBALL"
```

- [ ] **Step 4: Make shell scripts executable (Unix)**

Run:

```bash
git add scripts/*.ps1 scripts/*.sh scripts/*.template
git update-index --chmod=+x scripts/package.sh
```

- [ ] **Step 5: Run the Windows packaging script**

Run:

```powershell
.\scripts\package.ps1
```

Expected: `dist/parrot-x86_64-pc-windows-msvc/` and `dist/parrot-x86_64-pc-windows-msvc.zip` are created.

- [ ] **Step 6: Commit**

```bash
git add scripts/
git commit -m "build: add local packaging scripts and default config template"
```

---

### Task 4: Add install scripts

**Files:**
- Create: `scripts/install.ps1`
- Create: `scripts/install.sh`

**Interfaces:**
- Consumes: packaged archive or source directory
- Produces: installed binaries in user-local directory + printed PATH instructions

- [ ] **Step 1: Create Windows install script**

Create `scripts/install.ps1`:

```powershell
#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$installDir = "$env:USERPROFILE\.parrot"
$binDir = "$installDir\bin"
$configDir = "$installDir\config"

New-Item -ItemType Directory -Path $binDir -Force | Out-Null
New-Item -ItemType Directory -Path $configDir -Force | Out-Null

$sourceDir = "$PSScriptRoot\..\dist\parrot-x86_64-pc-windows-msvc"
if (-not (Test-Path $sourceDir)) {
    throw "Distribution not found at $sourceDir. Run scripts/package.ps1 first."
}

Copy-Item "$sourceDir\bin\parrot.exe" $binDir -Force
Copy-Item "$sourceDir\bin\parrotd.exe" $binDir -Force

if (-not (Test-Path "$configDir\parrot.toml")) {
    Copy-Item "$sourceDir\config\parrot.toml" $configDir\parrot.toml
    Write-Host "Created default config at $configDir\parrot.toml"
}

Write-Host "Installed parrot to $binDir"
Write-Host "Add the following directory to your PATH:"
Write-Host "  $binDir"
```

- [ ] **Step 2: Create Unix install script**

Create `scripts/install.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${PARROT_INSTALL_DIR:-$HOME/.parrot}"
BIN_DIR="$INSTALL_DIR/bin"
CONFIG_DIR="$INSTALL_DIR/config"

mkdir -p "$BIN_DIR" "$CONFIG_DIR"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="$(rustc -vV | sed -n 's|host: ||p')"
SOURCE_DIR="$SCRIPT_DIR/../dist/parrot-$TARGET"

if [ ! -d "$SOURCE_DIR" ]; then
    echo "Distribution not found at $SOURCE_DIR. Run scripts/package.sh first." >&2
    exit 1
fi

cp "$SOURCE_DIR/bin/parrot" "$BIN_DIR/"
cp "$SOURCE_DIR/bin/parrotd" "$BIN_DIR/"

if [ ! -f "$CONFIG_DIR/parrot.toml" ]; then
    cp "$SOURCE_DIR/config/parrot.toml" "$CONFIG_DIR/parrot.toml"
    echo "Created default config at $CONFIG_DIR/parrot.toml"
fi

echo "Installed parrot to $BIN_DIR"
echo "Add the following directory to your PATH:"
echo "  $BIN_DIR"
```

- [ ] **Step 3: Make Unix install script executable**

Run:

```bash
git update-index --chmod=+x scripts/install.sh
```

- [ ] **Step 4: Run the Windows install script**

Run:

```powershell
.\scripts\install.ps1
```

Expected: binaries copied to `$HOME\.parrot\bin`, default config created if missing, PATH instruction printed.

- [ ] **Step 5: Commit**

```bash
git add scripts/install.ps1 scripts/install.sh
git commit -m "build: add local install scripts"
```

---

### Task 5: Verification

**Files:**
- All files modified/created above.

**Interfaces:**
- Consumes: full build, unit tests, manual end-to-end test
- Produces: passing verification checklist

- [ ] **Step 1: Run full workspace tests**

Run:

```bash
cargo test --workspace
```

Expected: all tests pass.

- [ ] **Step 2: Run clippy and fmt**

Run:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: no warnings, no formatting diffs.

- [ ] **Step 3: Manual end-to-end auto-start test**

1. Ensure no `parrotd` process is running (Task Manager or `Get-Process parrotd`).
2. From the project root, run:

```bash
cargo run --bin parrot -- sessions list
```

Expected: `parrotd` starts in the background, `parrot sessions list` succeeds and shows no sessions or an empty table.

3. Exit and verify `parrotd` is still running.
4. Run again:

```bash
cargo run --bin parrot -- sessions list
```

Expected: command succeeds without starting a second daemon process.

- [ ] **Step 4: Manual packaging test**

Run:

```powershell
.\scripts\package.ps1
```

Verify the zip contains:
- `bin/parrot.exe`
- `bin/parrotd.exe`
- `config/parrot.toml`
- `README.md`

- [ ] **Step 5: Commit any final fixes**

```bash
git add -A
git commit -m "chore: verify single-command auto-start and packaging"
```

---

## Spec Coverage Check

- Daemon auto-start before daemon-requiring commands: Task 1 + Task 2.
- Daemon outlives CLI: Task 1 (no kill on exit).
- Hidden background process: Task 1 (`CREATE_NO_WINDOW`, output redirected).
- TCP probe without token: Task 1 (`probe_port`).
- Local distribution directory + zip/tar.gz: Task 3.
- Install scripts: Task 4.
- Verification: Task 5.

## Placeholder Scan

No TBD/TODO/"implement later"/vague requirements. Every step contains exact code or commands.
