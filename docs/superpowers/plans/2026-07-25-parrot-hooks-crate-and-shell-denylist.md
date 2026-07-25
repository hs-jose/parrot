# Parrot-Hooks Crate + Shell-Denylist Hook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract built-in hooks into a new `parrot-hooks` crate, add per-hook config container support to `HooksConfig`, implement a `shell_denylist` hook, and simplify `ShellExecTool` by removing its inline denylist.

**Architecture:** New `crates/parrot-hooks/` crate depends on `parrot-core` (Hook trait) + `parrot-config` (HooksConfig). `parrot-daemon` drops its `src/hooks/` module and imports `parrot_hooks::build_registry` instead. `HooksConfig` gains a `#[serde(flatten)] configs: HashMap<String, toml::Value>` field so each hook owns its typed config struct. `ShellExecTool` loses its `denylist` field → pure execution. `SandboxConfig.denylist` field is removed.

**Tech Stack:** Rust 2021, workspace crate, serde + toml, async-trait, regex, bitflags

## Global Constraints

- Errors crossing crate boundaries use `thiserror` enums. `anyhow` only inside binaries.
- `parrot-core` has zero IO: no `reqwest`, no `tokio::fs`, no `std::env`.
- WS messages are tagged enums (`#[serde(tag = "type")]`).
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
- Branch: `feat/lifecycle-hooks`

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/parrot-hooks/Cargo.toml` | Create | Crate manifest: deps on parrot-core, parrot-config, async-trait, regex, serde, toml, tracing |
| `crates/parrot-hooks/src/lib.rs` | Create | Re-exports + `build_registry()` function |
| `crates/parrot-hooks/src/dangerous_command_blocker.rs` | Move from daemon | Existing hook, made `pub` |
| `crates/parrot-hooks/src/redact_secrets.rs` | Move from daemon | Existing hook (as-is, no #2 expansion), made `pub` |
| `crates/parrot-hooks/src/shell_denylist.rs` | Create | New hook + `ShellDenylistConfig` |
| `Cargo.toml` (workspace root) | Modify | Add `parrot-hooks` to members + dev-dependencies |
| `crates/parrot-config/src/config.rs` | Modify | Add `configs: HashMap<String, toml::Value>` to `HooksConfig`, update `Default` |
| `crates/parrot-tools/src/shell_exec.rs` | Modify | Remove `denylist` field + `is_denied` method |
| `crates/parrot-tools/src/lib.rs` | Modify | Stop passing denylist to `ShellExecTool::new` |
| `crates/parrot-daemon/Cargo.toml` | Modify | Add `parrot-hooks` dep, remove `regex` dep |
| `crates/parrot-daemon/src/lib.rs` | Modify | Remove `pub mod hooks` |
| `crates/parrot-daemon/src/runtime.rs` | Modify | Import `build_registry` from `parrot_hooks` |

---

### Task 1: Scaffold `parrot-hooks` Crate

**Files:**
- Create: `crates/parrot-hooks/Cargo.toml`
- Create: `crates/parrot-hooks/src/lib.rs`
- Modify: `Cargo.toml` (workspace root, lines 8-16 members + dev-dependencies)

**Interfaces:**
- Produces: empty `parrot_hooks` crate that compiles as part of the workspace

- [ ] **Step 1: Create `crates/parrot-hooks/Cargo.toml`**

```toml
[package]
name = "parrot-hooks"
version.workspace = true
edition.workspace = true

[dependencies]
parrot-core    = { path = "../parrot-core" }
parrot-config  = { path = "../parrot-config" }
async-trait    = { workspace = true }
regex          = { workspace = true }
serde          = { workspace = true }
toml           = { workspace = true }
tracing        = { workspace = true }

[dev-dependencies]
uuid       = { workspace = true }
tokio      = { workspace = true }
parrot-protocol = { path = "../parrot-protocol" }
```

- [ ] **Step 2: Create `crates/parrot-hooks/src/lib.rs`**

```rust
// Built-in hook implementations + registry builder.
// Hooks live here rather than in parrot-daemon so they can be reused
// and tested independently of the daemon binary.
```

- [ ] **Step 3: Add `parrot-hooks` to workspace members**

In `Cargo.toml` (workspace root), add `"crates/parrot-hooks"` to the `members` array after `"crates/parrot-tools"`:

```toml
members = [
    "crates/parrot-protocol",
    "crates/parrot-config",
    "crates/parrot-transport",
    "crates/parrot-core",
    "crates/parrot-providers",
    "crates/parrot-tools",
    "crates/parrot-hooks",
    "crates/parrot-daemon",
]
```

In the same file, add to `[dev-dependencies]`:

```toml
parrot-hooks      = { path = "crates/parrot-hooks" }
```

- [ ] **Step 4: Verify build**

Run: `cargo build --workspace`
Expected: PASS (empty crate compiles)

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-hooks/ Cargo.toml
git commit -m "scaffold: parrot-hooks crate"
```

---

### Task 2: Add `configs` HashMap to `HooksConfig`

**Files:**
- Modify: `crates/parrot-config/src/config.rs:62-77`
- Test: `crates/parrot-config/src/config.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `HooksConfig.configs: HashMap<String, toml::Value>` populated via `#[serde(flatten)]`
- Produces: updated `HooksConfig::default()` with `configs: HashMap::new()`

**Known risk:** `#[serde(flatten)]` + `HashMap<String, toml::Value>` may not parse correctly with `toml` 0.8. If the test in Step 3 fails, use the fallback: replace `#[serde(flatten)]` with a manual post-parse step in `AppConfig::load` / `load_from` that extracts subtables. The fallback is documented in spec §3.

- [ ] **Step 1: Write the failing test**

Add to `crates/parrot-config/src/config.rs` in the `#[cfg(test)] mod tests` section, after the existing `hooks_parse_from_toml` test:

```rust
    #[test]
    fn hooks_parse_per_hook_configs() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
denylist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6

[hooks]
enabled = ["shell_denylist", "redact_secrets"]
timeout_seconds = 5

[hooks.shell_denylist]
patterns = ["rm -rf /", "sudo", "chmod 777"]

[hooks.redact_secrets]
extra_patterns = ["CUSTOM-\\d+"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(c.hooks.enabled, vec!["shell_denylist", "redact_secrets"]);
        assert!(c.hooks.configs.contains_key("shell_denylist"));
        assert!(c.hooks.configs.contains_key("redact_secrets"));
    }

    #[test]
    fn hooks_missing_configs_defaults_empty() {
        let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
denylist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6

[hooks]
enabled = ["dangerous_command_blocker"]
"#;
        let c: AppConfig = toml::from_str(toml).unwrap();
        assert!(c.hooks.configs.is_empty(), "configs should be empty when no [hooks.<id>] subtables present");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p parrot-config -- hooks_parse_per_hook_configs`
Expected: FAIL with "no field `configs`" or compile error

- [ ] **Step 3: Add `configs` field to `HooksConfig`**

In `crates/parrot-config/src/config.rs`, add `use std::collections::HashMap;` at the top (after existing `use` lines), then modify `HooksConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_seconds: u64,
    /// Per-hook config subtables. Populated from `[hooks.<id>]` TOML tables
    /// via `#[serde(flatten)]`. Each hook owns its typed config struct and
    /// deserializes its entry from this map.
    #[serde(default, flatten)]
    pub configs: HashMap<String, toml::Value>,
}
```

Update `Default` impl:

```rust
impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: Vec::new(),
            timeout_seconds: default_hook_timeout(),
            configs: HashMap::new(),
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p parrot-config -- hooks_parse_per_hook_configs hooks_missing_configs_defaults_empty`
Expected: PASS

> **If FAIL:** `#[serde(flatten)]` is incompatible with toml. Fallback: remove `#[serde(flatten)]`, keep `#[serde(default)]`, and add a manual extraction in `AppConfig::load_from` after `toml::from_str`:
> ```rust
> // Fallback: manually parse [hooks.<id>] subtables
> if let Ok(raw) = toml::Value::from_str(&content) {
>     if let Some(hooks_tbl) = raw.get("hooks").and_then(|v| v.as_table()) {
>         for (key, val) in hooks_tbl {
>             if key != "enabled" && key != "timeout_seconds" {
>                 config.hooks.configs.insert(key.clone(), val.clone());
>             }
>         }
>     }
> }
> ```
> Place this after `let mut config: AppConfig = toml::from_str(&content)?;` in both `load` and `load_from`.

- [ ] **Step 5: Verify existing config tests still pass**

Run: `cargo test -p parrot-config`
Expected: All PASS

- [ ] **Step 6: Commit**

```bash
git add crates/parrot-config/src/config.rs
git commit -m "config: add per-hook configs HashMap to HooksConfig"
```

---

### Task 3: Move `dangerous_command_blocker` to `parrot-hooks`

**Files:**
- Move: `crates/parrot-daemon/src/hooks/dangerous_command_blocker.rs` → `crates/parrot-hooks/src/dangerous_command_blocker.rs`
- Modify: `crates/parrot-hooks/src/lib.rs`
- Modify: `crates/parrot-daemon/src/hooks/mod.rs` (will be deleted in Task 9, for now just remove the `mod` declaration)

**Interfaces:**
- Produces: `parrot_hooks::dangerous_command_blocker::DangerousCommandBlocker` (pub struct)

- [ ] **Step 1: Copy file to new location**

Copy `crates/parrot-daemon/src/hooks/dangerous_command_blocker.rs` to `crates/parrot-hooks/src/dangerous_command_blocker.rs`. The content is identical — no import changes needed (it already imports from `parrot_core::hooks::*`).

- [ ] **Step 2: Register module in `parrot-hooks/src/lib.rs`**

```rust
pub mod dangerous_command_blocker;
```

- [ ] **Step 3: Remove the module from parrot-daemon temporarily**

In `crates/parrot-daemon/src/hooks/mod.rs`, comment out or remove the `mod dangerous_command_blocker;` line and the `DangerousCommandBlocker` usage in `build_registry`. Replace with a temporary stub:

```rust
use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

mod redact_secrets;

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let mut reg = HookRegistry::new(Duration::from_secs(cfg.timeout_seconds.max(1)));
    for id in &cfg.enabled {
        match id.as_str() {
            "redact_secrets" => reg.register(Arc::new(redact_secrets::RedactSecrets)),
            other => warn!("unknown hook id in [hooks].enabled: {other} (skipping)"),
        }
    }
    Arc::new(reg)
}
```

Also remove the `dangerous_command_blocker` from the test `build_registry_picks_up_known_hooks`:

```rust
    #[test]
    fn build_registry_picks_up_known_hooks() {
        let cfg = HooksConfig {
            enabled: vec!["redact_secrets".into()],
            timeout_seconds: 5,
            configs: std::collections::HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }
```

And update `build_registry_unknown_id_warns_and_skips`:

```rust
    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: std::collections::HashMap::new(),
        };
        let reg = build_registry(&cfg);
        let _ = reg;
    }
```

- [ ] **Step 4: Verify build + tests**

Run: `cargo build --workspace && cargo test -p parrot-hooks -p parrot-daemon`
Expected: PASS (dangerous_command_blocker tests now run from parrot-hooks)

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-hooks/src/dangerous_command_blocker.rs crates/parrot-hooks/src/lib.rs crates/parrot-daemon/src/hooks/mod.rs
git commit -m "move dangerous_command_blocker to parrot-hooks"
```

---

### Task 4: Move `redact_secrets` to `parrot-hooks` (as-is)

**Files:**
- Move: `crates/parrot-daemon/src/hooks/redact_secrets.rs` → `crates/parrot-hooks/src/redact_secrets.rs`
- Modify: `crates/parrot-hooks/src/lib.rs`
- Delete: `crates/parrot-daemon/src/hooks/mod.rs` (entire module goes away now)

**Interfaces:**
- Produces: `parrot_hooks::redact_secrets::RedactSecrets` (pub struct, no config yet — #2 expansion adds config)

- [ ] **Step 1: Copy file to new location**

Copy `crates/parrot-daemon/src/hooks/redact_secrets.rs` to `crates/parrot-hooks/src/redact_secrets.rs`. Content is identical — imports already use `parrot_core::hooks::*`.

- [ ] **Step 2: Register module in `parrot-hooks/src/lib.rs`**

```rust
pub mod dangerous_command_blocker;
pub mod redact_secrets;
```

- [ ] **Step 3: Delete `parrot-daemon/src/hooks/` module**

Delete `crates/parrot-daemon/src/hooks/mod.rs` and `crates/parrot-daemon/src/hooks/dangerous_command_blocker.rs` (the original, already copied in Task 3).

In `crates/parrot-daemon/src/lib.rs`, remove `pub mod hooks;`:

```rust
pub mod auth;
pub mod runtime;
pub mod session_store;

pub use runtime::{run, run_with, run_with_confirm_timeout};
```

In `crates/parrot-daemon/src/runtime.rs`, change the import:

```rust
use parrot_hooks::build_registry;
```

(Remove `use crate::hooks::build_registry;`)

- [ ] **Step 4: Add `parrot-hooks` dependency to `parrot-daemon/Cargo.toml`**

```toml
[dependencies]
parrot-core      = { path = "../parrot-core" }
parrot-protocol  = { path = "../parrot-protocol" }
parrot-config    = { path = "../parrot-config" }
parrot-transport = { path = "../parrot-transport" }
parrot-providers = { path = "../parrot-providers" }
parrot-tools     = { path = "../parrot-tools" }
parrot-hooks     = { path = "../parrot-hooks" }
async-trait     = { workspace = true }
tokio            = { workspace = true }
serde            = { workspace = true }
serde_json       = { workspace = true }
tracing          = { workspace = true }
uuid             = { workspace = true }
chrono           = { workspace = true }
rand             = { workspace = true }
hex              = { workspace = true }
```

(Removed `regex = { workspace = true }` since only hooks used it.)

- [ ] **Step 5: Verify build + tests**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS (all existing tests, including e2e hook test)

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "move redact_secrets to parrot-hooks, remove daemon hooks module"
```

---

### Task 5: Move `build_registry` to `parrot-hooks`

**Files:**
- Create: `crates/parrot-hooks/src/lib.rs` (replace placeholder)
- Test: `crates/parrot-hooks/src/lib.rs` (inline tests)

**Interfaces:**
- Produces: `parrot_hooks::build_registry(cfg: &HooksConfig) -> Arc<HookRegistry>`
- Consumes: `parrot_config::HooksConfig` (with `configs` field from Task 2)

- [ ] **Step 1: Write the failing test**

Add to `crates/parrot-hooks/src/lib.rs`:

```rust
pub mod dangerous_command_blocker;
pub mod redact_secrets;

use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let mut reg = HookRegistry::new(Duration::from_secs(cfg.timeout_seconds.max(1)));
    for id in &cfg.enabled {
        match id.as_str() {
            "dangerous_command_blocker" => {
                reg.register(Arc::new(dangerous_command_blocker::DangerousCommandBlocker))
            }
            "redact_secrets" => reg.register(Arc::new(redact_secrets::RedactSecrets)),
            other => warn!("unknown hook id in [hooks].enabled: {other} (skipping)"),
        }
    }
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let reg = build_registry(&cfg);
        let _ = reg;
    }

    #[test]
    fn build_registry_picks_up_known_hooks() {
        let cfg = HooksConfig {
            enabled: vec!["dangerous_command_blocker".into(), "redact_secrets".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p parrot-hooks`
Expected: PASS

- [ ] **Step 3: Verify `parrot-daemon` still builds**

`runtime.rs` already uses `parrot_hooks::build_registry` from Task 4.

Run: `cargo build --workspace`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-hooks/src/lib.rs
git commit -m "move build_registry to parrot-hooks"
```

---

### Task 6: Implement `shell_denylist` Hook

**Files:**
- Create: `crates/parrot-hooks/src/shell_denylist.rs`
- Modify: `crates/parrot-hooks/src/lib.rs` (add module + `build_registry` arm)

**Interfaces:**
- Produces: `parrot_hooks::shell_denylist::ShellDenylist` (pub struct)
- Produces: `parrot_hooks::shell_denylist::ShellDenylistConfig` (pub struct with `Default`)

- [ ] **Step 1: Write the failing tests**

Create `crates/parrot-hooks/src/shell_denylist.rs`:

```rust
use async_trait::async_trait;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ShellDenylistConfig {
    /// Substring patterns (case-insensitive). A command containing any
    /// pattern as a substring is blocked.
    #[serde(default = "ShellDenylistConfig::default_patterns")]
    pub patterns: Vec<String>,
}

impl ShellDenylistConfig {
    fn default_patterns() -> Vec<String> {
        vec![
            "rm -rf /".to_string(),
            "sudo".to_string(),
            "chmod 777".to_string(),
        ]
    }
}

impl Default for ShellDenylistConfig {
    fn default() -> Self {
        Self {
            patterns: Self::default_patterns(),
        }
    }
}

pub struct ShellDenylist {
    cfg: ShellDenylistConfig,
}

impl ShellDenylist {
    pub fn new(cfg: ShellDenylistConfig) -> Self {
        Self { cfg }
    }

    fn is_denied(&self, command: &str) -> bool {
        let command_lower = command.to_lowercase();
        for pattern in &self.cfg.patterns {
            if command_lower.contains(&pattern.to_lowercase()) {
                return true;
            }
        }
        false
    }
}

#[async_trait]
impl Hook for ShellDenylist {
    fn id(&self) -> &'static str {
        "shell_denylist"
    }
    fn supported(&self) -> HookPoints {
        HookPoints::TOOL_CALL
    }
    async fn handle(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookAction, parrot_core::AgentError> {
        if let HookEvent::ToolCall {
            tool_name,
            arguments,
            ..
        } = ev
        {
            if matches!(tool_name, "shell_exec" | "bash" | "shell") {
                if let Some(cmd) = arguments.get("command").and_then(|v| v.as_str()) {
                    if self.is_denied(cmd) {
                        return Ok(HookAction::Block {
                            reason: format!("command denied by shell_denylist: {}", cmd),
                        });
                    }
                }
            }
        }
        Ok(HookAction::NoOp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ctx() -> HookCtx<'static> {
        HookCtx {
            session_id: Uuid::nil(),
            working_dir: std::path::Path::new("."),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    fn tool_call_ev(cmd: &str) -> HookEvent<'static> {
        let args = serde_json::json!({"command": cmd});
        HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "shell_exec",
            arguments: Box::leak(args.into_boxed_json()),
        }
    }

    #[tokio::test]
    async fn blocks_rm_rf_root() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("rm -rf /"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn blocks_sudo() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("sudo apt update"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn blocks_case_insensitive() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("SUDO ls"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn passes_safe_command() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let out = h.handle(tool_call_ev("ls -la"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[tokio::test]
    async fn ignores_non_shell_tool() {
        let h = ShellDenylist::new(ShellDenylistConfig::default());
        let ev = HookEvent::ToolCall {
            session_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            parent_message_id: Uuid::nil(),
            tool_call_id: "x",
            tool_name: "read",
            arguments: &serde_json::Value::Null,
        };
        let out = h.handle(ev, &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[tokio::test]
    async fn custom_patterns_block() {
        let cfg = ShellDenylistConfig {
            patterns: vec!["DROP TABLE".to_string()],
        };
        let h = ShellDenylist::new(cfg);
        let out = h.handle(tool_call_ev("psql -c 'DROP TABLE users'"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::Block { .. }));
    }

    #[tokio::test]
    async fn empty_patterns_allows_all() {
        let cfg = ShellDenylistConfig { patterns: vec![] };
        let h = ShellDenylist::new(cfg);
        let out = h.handle(tool_call_ev("rm -rf /"), &ctx()).await.unwrap();
        assert!(matches!(out, HookAction::NoOp));
    }

    #[test]
    fn default_config_has_three_patterns() {
        let cfg = ShellDenylistConfig::default();
        assert_eq!(cfg.patterns.len(), 3);
        assert!(cfg.patterns.contains(&"rm -rf /".to_string()));
        assert!(cfg.patterns.contains(&"sudo".to_string()));
        assert!(cfg.patterns.contains(&"chmod 777".to_string()));
    }
}
```

> **Note on `Box::leak`:** The `tool_call_ev` helper uses `Box::leak` to turn `serde_json::Value` into `&'static serde_json::Value` for the test. This is acceptable for tests (memory lives forever but the process exits). The existing `dangerous_command_blocker` tests use `&args` via local variables — if those compile, it's because `HookEvent::ToolCall` borrows with lifetime `'a` tied to the local. Use whichever pattern the existing tests use; if `Box::leak` causes issues, fall back to the existing pattern:
> ```rust
> let args = serde_json::json!({"command": cmd});
> // use &args directly in the event, keeping args alive in the test fn
> ```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p parrot-hooks -- shell_denylist`
Expected: FAIL (module not registered in lib.rs)

- [ ] **Step 3: Register module and add `build_registry` arm**

In `crates/parrot-hooks/src/lib.rs`, add module declaration and `build_registry` arm. The full file:

```rust
pub mod dangerous_command_blocker;
pub mod redact_secrets;
pub mod shell_denylist;

use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let mut reg = HookRegistry::new(Duration::from_secs(cfg.timeout_seconds.max(1)));
    for id in &cfg.enabled {
        match id.as_str() {
            "dangerous_command_blocker" => {
                reg.register(Arc::new(dangerous_command_blocker::DangerousCommandBlocker))
            }
            "redact_secrets" => reg.register(Arc::new(redact_secrets::RedactSecrets)),
            "shell_denylist" => {
                let hook_cfg = cfg
                    .configs
                    .get("shell_denylist")
                    .and_then(|v| v.clone().try_deserialize::<shell_denylist::ShellDenylistConfig>())
                    .unwrap_or_default();
                reg.register(Arc::new(shell_denylist::ShellDenylist::new(hook_cfg)))
            }
            other => warn!("unknown hook id in [hooks].enabled: {other} (skipping)"),
        }
    }
    Arc::new(reg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn build_registry_unknown_id_warns_and_skips() {
        let cfg = HooksConfig {
            enabled: vec!["nonexistent".into()],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let reg = build_registry(&cfg);
        let _ = reg;
    }

    #[test]
    fn build_registry_picks_up_known_hooks() {
        let cfg = HooksConfig {
            enabled: vec![
                "dangerous_command_blocker".into(),
                "redact_secrets".into(),
                "shell_denylist".into(),
            ],
            timeout_seconds: 5,
            configs: HashMap::new(),
        };
        let _reg = build_registry(&cfg);
    }

    #[test]
    fn build_registry_shell_denylist_with_config() {
        let mut configs = HashMap::new();
        configs.insert(
            "shell_denylist".to_string(),
            toml::Value::Table({
                let mut t = toml::value::Table::new();
                t.insert(
                    "patterns".to_string(),
                    toml::Value::Array(vec![toml::Value::String("DROP TABLE".into())]),
                );
                t
            }),
        );
        let cfg = HooksConfig {
            enabled: vec!["shell_denylist".into()],
            timeout_seconds: 5,
            configs,
        };
        let _reg = build_registry(&cfg);
    }
}
```

> **`try_deserialize` note:** `toml::Value::try_deserialize::<T>()` is the toml 0.8 API for deserializing a `toml::Value` into a typed struct. If this method is not available, use `toml::Value::try_into::<T>()` or `serde::Deserialize::deserialize(v.clone())` instead. Check the toml 0.8 docs for the exact method name.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p parrot-hooks`
Expected: All PASS

- [ ] **Step 5: Verify workspace build**

Run: `cargo build --workspace`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/parrot-hooks/src/shell_denylist.rs crates/parrot-hooks/src/lib.rs
git commit -m "feat: shell_denylist hook + per-hook config in build_registry"
```

---

### Task 7: Simplify `ShellExecTool` — Remove Denylist

**Files:**
- Modify: `crates/parrot-tools/src/shell_exec.rs:7-25`
- Modify: `crates/parrot-tools/src/lib.rs:44-51`

**Interfaces:**
- Produces: `ShellExecTool::new(working_dir: PathBuf)` — no denylist param

- [ ] **Step 1: Modify `ShellExecTool`**

In `crates/parrot-tools/src/shell_exec.rs`, remove the `denylist` field, `is_denied` method, and the denylist check in `call`:

```rust
pub struct ShellExecTool;

impl ShellExecTool {
    pub fn new(_working_dir: std::path::PathBuf) -> Self {
        Self
    }
}
```

Remove lines 63-69 (the `is_denied` check block):
```rust
        // Check denylist
        if self.is_denied(command) {
            return Ok(ToolOutput {
                content: format!("Command denied by sandbox policy: {}", command),
                is_error: true,
            });
        }
```

Remove the entire `is_denied` method (lines 16-24).

- [ ] **Step 2: Update `register_all` in `parrot-tools/src/lib.rs`**

Change the `ShellExecTool::new` call to not pass denylist:

```rust
    if config.tools.shell_allowed {
        registry
            .register(std::sync::Arc::new(shell_exec::ShellExecTool::new(
                working_dir.clone(),
            )))
            .await;
    }
```

- [ ] **Step 3: Verify build + tests**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-tools/src/shell_exec.rs crates/parrot-tools/src/lib.rs
git commit -m "refactor: remove denylist from ShellExecTool (moved to shell_denylist hook)"
```

---

### Task 8: Remove `SandboxConfig.denylist` Field

**Files:**
- Modify: `crates/parrot-config/src/config.rs:43-49` (struct) + `:166-171` (default_config)
- Test: `crates/parrot-config/src/config.rs` (inline tests)

**Interfaces:**
- Produces: `SandboxConfig` without `denylist` field

- [ ] **Step 1: Write the failing test**

Add to `crates/parrot-config/src/config.rs` test module:

```rust
    #[test]
    fn sandbox_config_has_no_denylist() {
        let c = AppConfig::default_config();
        // denylist field is removed; only allowlist + require_confirmation remain
        assert!(c.tools.sandbox.allowlist.is_empty());
        assert!(c.tools.sandbox.require_confirmation.len() > 0);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p parrot-config -- sandbox_config_has_no_denylist`
Expected: FAIL (field still exists, or compile error if already removed)

- [ ] **Step 3: Remove `denylist` from `SandboxConfig`**

In `crates/parrot-config/src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub working_dir: String,
    pub allowlist: Vec<String>,
    pub require_confirmation: Vec<String>,
}
```

In `default_config()`, remove the `denylist` line:

```rust
            sandbox: SandboxConfig {
                working_dir: ".".into(),
                allowlist: vec![],
                require_confirmation: vec!["git push".into(), "rm".into()],
            },
```

- [ ] **Step 4: Fix all references to `denylist` in the codebase**

Search for remaining `.denylist` references. The main ones:
- `crates/parrot-tools/src/lib.rs` — already fixed in Task 7 (no longer reads `config.tools.sandbox.denylist`)
- Any test TOML strings that include `denylist = []` — remove those lines from test fixtures.

In `crates/parrot-config/src/config.rs` test `hooks_parse_from_toml`, remove `denylist = []` from the `[tools.sandbox]` section:

```toml
[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []
```

Do the same in `hooks_parse_per_hook_configs` and `hooks_missing_configs_defaults_empty` tests (from Task 2).

Check `parrot.toml` — it may have a `denylist` key under `[tools.sandbox]`. If so, remove it. (But do NOT commit `parrot.toml` changes — it has intentional local-only modifications.)

- [ ] **Step 5: Run all tests**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/parrot-config/src/config.rs
git commit -m "refactor: remove denylist field from SandboxConfig"
```

---

### Task 9: Update Default `enabled` List

**Files:**
- Modify: `crates/parrot-config/src/config.rs:70-77` (`HooksConfig::default`)

**Interfaces:**
- Produces: `HooksConfig::default().enabled == ["shell_denylist", "dangerous_command_blocker"]`

- [ ] **Step 1: Write the failing test**

Add to config.rs test module:

```rust
    #[test]
    fn hooks_default_enabled_includes_shell_denylist_and_blocker() {
        let c = HooksConfig::default();
        assert!(c.enabled.contains(&"shell_denylist".to_string()));
        assert!(c.enabled.contains(&"dangerous_command_blocker".to_string()));
        assert!(!c.enabled.contains(&"redact_secrets".to_string()));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p parrot-config -- hooks_default_enabled_includes`
Expected: FAIL (default enabled is empty)

- [ ] **Step 3: Update `HooksConfig::default`**

```rust
impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: vec![
                "shell_denylist".to_string(),
                "dangerous_command_blocker".to_string(),
            ],
            timeout_seconds: default_hook_timeout(),
            configs: HashMap::new(),
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p parrot-config`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-config/src/config.rs
git commit -m "config: default hooks enabled = [shell_denylist, dangerous_command_blocker]"
```

---

### Task 10: Final Verification + E2E Test

**Files:**
- Modify: `tests/integration/e2e_test.rs` (add `e2e_shell_denylist_blocks_rm_rf`)

- [ ] **Step 1: Add e2e test**

Add to `tests/integration/e2e_test.rs`, after `e2e_hook_blocks_tool_call`:

```rust
#[tokio::test]
async fn e2e_shell_denylist_blocks_rm_rf() {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = parrot_daemon::auth::Auth::new(&token_path)
        .await
        .expect("init auth");
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    let auth = Arc::new(auth);

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            Arc::new(DangerousShellExecProvider::new()) as Arc<dyn LlmProvider>,
            vec!["mock-model".to_string()],
        )
        .await;

    let shell_call_count = Arc::new(AtomicU32::new(0));
    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(CountingShellExec {
            call_count: Arc::clone(&shell_call_count),
        }) as Arc<dyn Tool>)
        .await;

    let mut config = test_config(port, &data_dir, &token_path);
    config.hooks.enabled = vec!["shell_denylist".to_string()];

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = client.connect(&url, &token).await.expect("client connect");

    expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::HelloAck { .. } = m {
                Some(())
            } else {
                None
            }
        },
        "HelloAck",
    )
    .await;

    conn.sender
        .send(ClientMessage::CreateSession {
            config: Some(SessionConfig {
                model: None,
                provider: None,
                system_prompt: None,
            }),
        })
        .await
        .expect("send CreateSession");

    let session_id = expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(*session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "please run rm -rf /".to_string(),
        })
        .await
        .expect("send Chat");

    let (hook_id, event_kind, result_kind) = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::HookFired {
                session_id: sid,
                hook_id,
                event_kind,
                result_kind,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((hook_id.clone(), event_kind.clone(), result_kind.clone()));
                }
            }
            None
        },
        "HookFired",
    )
    .await;
    assert_eq!(hook_id, "shell_denylist");
    assert_eq!(event_kind, "tool_call");
    assert_eq!(result_kind, "block");

    let (result_content, is_error) = expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some((result.content.clone(), result.is_error));
                }
            }
            None
        },
        "ToolEnd",
    )
    .await;
    assert!(is_error);
    assert!(
        result_content.starts_with("blocked:"),
        "ToolEnd content should start with 'blocked:', got: {result_content}"
    );

    expect_agent_event(
        &mut conn.receiver,
        |ev| {
            if let AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason: TurnStopReason::EndTurn,
                ..
            } = ev
            {
                if *sid == session_id {
                    return Some(());
                }
            }
            None
        },
        "TurnEnd(EndTurn)",
    )
    .await;

    assert_eq!(
        shell_call_count.load(Ordering::SeqCst),
        0,
        "CountingShellExec should NOT have been invoked (shell_denylist blocked before execute)"
    );

    daemon_handle.abort();
}
```

- [ ] **Step 2: Run e2e test**

Run: `cargo test --test e2e -- e2e_shell_denylist_blocks_rm_rf`
Expected: PASS

- [ ] **Step 3: Run full workspace verification**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: All PASS

- [ ] **Step 4: Commit**

```bash
git add tests/integration/e2e_test.rs
git commit -m "test: e2e shell_denylist blocks rm -rf /"
```

---

### Task 11: Sync Design Doc

**Files:**
- Modify: `docs/superpowers/specs/2026-07-25-parrot-hooks-crate-and-shell-denylist-design.md` (mark as implemented if needed)
- Modify: `docs/superpowers/specs/2026-07-25-parrot-lifecycle-hooks-design.md` (update "6 hook points" references if any mention denylist in ShellExecTool)

- [ ] **Step 1: Update docs if needed**

Check if the lifecycle-hooks design doc references `ShellExecTool.denylist` or `SandboxConfig.denylist`. If so, add a note that these were moved to the `shell_denylist` hook.

- [ ] **Step 2: Commit**

```bash
git add docs/
git commit -m "docs: sync for shell_denylist hook + ShellExecTool simplification"
```
