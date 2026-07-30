# Parrot External Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `[[hooks.external]]` to `parrot.toml` so users can write hooks in any language — daemon forks a child process per event, writes a JSON envelope to stdin, reads a JSON action from stdout's last line, and applies it as a standard `HookAction`.

**Architecture:** One new type `ExternalHook` impls the existing `Hook` trait, registered alongside the three built-in hooks in `build_registry`. All waterfall aggregation, telemetry, and engine integration stay untouched. Two surgical crate-core changes relax constraints: `Hook::id` returns `&str` instead of `&'static str`, and `AgentError` gains an `ExternalHook` variant for failure-path telemetry.

**Tech Stack:** Rust 2021, tokio (process + io-util + time), serde_json, async-trait, thiserror, toml

## Global Constraints

- Per spec §1: external process — any language, fork per event, full trust, no sandbox.
- Per spec §1: only 5 existing `HookAction` variants supported; unknown actions fail-open telemetry.
- Per spec §1: `[[hooks.external]]` listed ⇒ enabled; `enabled` list governs only built-in hooks.
- Per spec §4.3: trait `fn id(&self) -> &'static str` → `fn id(&self) -> &str`.
- Per spec §4.4: `AgentError::ExternalHook { hook_id, detail }` carries failure path.
- Per spec §6.1: every failure emits `Err(AgentError::ExternalHook{...})` so `HookExecution` records `result_kind = "error"` (not silent `noop`).
- Per spec §6.2: stderr truncated to 4KB and written via `tracing::warn!(target = "parrotd::ext_hook", ...)`.
- Per spec §6.3: per-event timeout via `tokio::time::timeout`, stdin write goes through a `tokio::spawn` writer task; on timeout `child.kill().await` + `child.wait()` reaps.
- Per spec §5.1: payload JSON envelope `{event, config, hook_id, working_dir, timeout_ms}`, newline-terminated.
- Per spec §5.2: stdout parse only the last non-empty line; empty stdout ⇒ `Ok(NoOp)` silent allow.
- Per spec §5.3: `WireAction` serde enum (tag = `"action"`, snake_case) → `HookAction` (tag = `"kind"`); wire uses `action` to match hook author intuition.
- Per spec §9: `crates/parrot-hooks/Cargo.toml` adds `tokio` (deps, not just dev-deps) and `serde_json` (deps, not just dev-deps).
- Per spec §7.4: cross-platform mock bins under `crates/parrot-hooks/tests/fixtures/`, accessed via `env!("CARGO_BIN_EXE_<name>")`.
- Per spec §7.6: NO daemon-level e2e — registry-level tests sufficient.
- Errors crossing crate boundaries use `thiserror` enums (`AGENTS.md`).
- `parrot-core` has zero IO: no `tokio::fs`, no `reqwest`. Child process spawning lives in `parrot-hooks`, not `parrot-core`.
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
- Branch: `feat/lifecycle-hooks` (continues from existing branch)

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/parrot-core/src/hooks.rs` | Modify | Relax `fn id` signature `&'static str` → `&str` |
| `crates/parrot-core/src/error.rs` | Modify | Add `AgentError::ExternalHook` variant |
| `crates/parrot-hooks/Cargo.toml` | Modify | Add deps `tokio`, `serde_json` |
| `crates/parrot-hooks/src/external.rs` | Create | `ExternalHook` impl + `WireAction` + pure `parse_action` + pure `parse_points` + tests |
| `crates/parrot-hooks/src/lib.rs` | Modify | Re-export `ExternalHook`, extend `build_registry` to loop external configs |
| `crates/parrot-hooks/src/shell_denylist.rs` | Modify | `fn id` signature relax |
| `crates/parrot-hooks/src/redact_secrets.rs` | Modify | `fn id` signature relax |
| `crates/parrot-hooks/src/dangerous_command_blocker.rs` | Modify | `fn id` signature relax |
| `crates/parrot-config/src/config.rs` | Modify | `ExternalHookConfig` struct + `HooksConfig.external` field + Default impl |
| `crates/parrot-config/tests/config_test.rs` | Modify | Parse tests for `[[hooks.external]]` |
| `crates/parrot-hooks/tests/fixtures/mock_hook_block.rs` | Create | Mock bin emits `{"action":"block","reason":"test"}` |
| `crates/parrot-hooks/tests/fixtures/mock_hook_noop.rs` | Create | Mock bin emits `{"action":"noop"}` |
| `crates/parrot-hooks/tests/fixtures/mock_hook_silent.rs` | Create | Mock bin emits empty stdout |
| `crates/parrot-hooks/tests/fixtures/mock_hook_exit1.rs` | Create | Mock bin emits action JSON, exits 1 |
| `crates/parrot-hooks/tests/fixtures/mock_hook_sleep.rs` | Create | Mock bin sleeps to trigger timeout |
| `crates/parrot-hooks/tests/fixtures/mock_hook_unknown.rs` | Create | Mock bin emits `{"action":"record_kv"}` |
| `crates/parrot-hooks/tests/fixtures/Cargo.toml` | Modify | Declare bin targets |
| `crates/parrot-hooks/tests/external_hook_test.rs` | Create | Integration tests using mock bins |

---

### Task 1: Relax `Hook::id` signature `&'static str` → `&str`

**Goal:** Allow `Hook` impls to return runtime `String`-backed ids, prerequisite for `ExternalHook`.

**Files:**
- Modify: `crates/parrot-core/src/hooks.rs:198-207` (trait)
- Modify: `crates/parrot-hooks/src/shell_denylist.rs` (impl signature)
- Modify: `crates/parrot-hooks/src/redact_secrets.rs` (impl signature)
- Modify: `crates/parrot-hooks/src/dangerous_command_blocker.rs` (impl signature)

**Interfaces:**
- Consumes: existing trait `Hook`
- Produces: trait signature change rippling to 3 built-in impls; all current `h.id().to_string()` / `h.id()` compare sites unchanged (verify with `cargo build --workspace`).

- [ ] **Step 1: Update trait signature**

In `crates/parrot-core/src/hooks.rs`, change:

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &'static str;
    fn supported(&self) -> HookPoints;
    async fn handle(
        &self,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookAction, AgentError>;
}
```

to:

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &str;
    fn supported(&self) -> HookPoints;
    async fn handle(
        &self,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookAction, AgentError>;
}
```

- [ ] **Step 2: Update built-in hook impls**

In `crates/parrot-hooks/src/shell_denylist.rs`, `redact_secrets.rs`, `dangerous_command_blocker.rs`, change each:

```rust
fn id(&self) -> &'static str {
    "shell_denylist"     // or "redact_secrets", "dangerous_command_blocker"
}
```

to:

```rust
fn id(&self) -> &str {
    "shell_denylist"
}
```

- [ ] **Step 3: Verify build + lint**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: PASS (string literal `&'static str` coerces to `&str`; no call sites break).

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-core/src/hooks.rs crates/parrot-hooks/src/shell_denylist.rs crates/parrot-hooks/src/redact_secrets.rs crates/parrot-hooks/src/dangerous_command_blocker.rs
git commit -m "refactor: relax Hook::id from &'static str to &str"
```

---

### Task 2: Add `AgentError::ExternalHook` variant

**Goal:** Carry external-hook failure details through `HookRegistry::call_with_timeout` into `HookExecution{result_kind:"error"}`.

**Files:**
- Modify: `crates/parrot-core/src/error.rs:3-19`

**Interfaces:**
- Produces: `AgentError::ExternalHook { hook_id: String, detail: String }`

- [ ] **Step 1: Add variant**

In `crates/parrot-core/src/error.rs`, extend the `AgentError` enum:

```rust
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
    #[error("external hook {hook_id} failed: {detail}")]
    ExternalHook { hook_id: String, detail: String },
}
```

- [ ] **Step 2: Verify build + lint**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Expected: PASS (variant unused for now; no clippy warning because `dead_code` lint is suppressed by `pub` variant of public enum).

- [ ] **Step 3: Commit**

```bash
git add crates/parrot-core/src/error.rs
git commit -m "feat: add AgentError::ExternalHook variant for external hook failures"
```

---

### Task 3: Add `ExternalHookConfig` to `parrot-config`

**Goal:** Parse `[[hooks.external]]` subtables from TOML.

**Files:**
- Modify: `crates/parrot-config/src/config.rs:62-86`
- Modify: `crates/parrot-config/tests/config_test.rs`

**Interfaces:**
- Produces: `ExternalHookConfig { id, command, events, timeout_seconds, config }`
- Produces: `HooksConfig.external: Vec<ExternalHookConfig>` field
- Produces: Updated `Default for HooksConfig` initializing `external: Vec::new()`

- [ ] **Step 1: Write the failing test**

In `crates/parrot-config/tests/config_test.rs`, append a test:

```rust
#[test]
fn hooks_external_parses_full_subtable() {
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/parrot/token"

[[providers]]
id = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-x"

[tools]
shell_allowed = true
file_write_allowed = true
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6

[[hooks.external]]
id = "compliance-log"
command = ["python", "/home/me/hook.py"]
events = ["tool_call", "tool_result"]
timeout_seconds = 2

[hooks.external.config]
sink = "stderr"
"#;
    let cfg = parrot_config::AppConfig::load_from_str(toml_str).unwrap();
    assert_eq!(cfg.hooks.external.len(), 1);
    let ext = &cfg.hooks.external[0];
    assert_eq!(ext.id, "compliance-log");
    assert_eq!(ext.command, vec!["python".to_string(), "/home/me/hook.py".to_string()]);
    assert_eq!(ext.events, vec!["tool_call".to_string(), "tool_result".to_string()]);
    assert_eq!(ext.timeout_seconds, Some(2));
    assert!(ext.config.as_table().map(|t| t.contains_key("sink")).unwrap_or(false));
}

#[test]
fn hooks_external_multi_entries_and_optional_fields() {
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/parrot/token"

[[providers]]
id = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-x"

[tools]
shell_allowed = true
file_write_allowed = true
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6

[[hooks.external]]
id = "audit"
command = ["node", "/etc/parrot/audit.js"]
events = ["agent_start", "agent_end"]

[[hooks.external]]
id = "blocker"
command = ["./blocker.sh"]
events = ["tool_call"]
"#;
    let cfg = parrot_config::AppConfig::load_from_str(toml_str).unwrap();
    assert_eq!(cfg.hooks.external.len(), 2);
    assert_eq!(cfg.hooks.external[0].id, "audit");
    assert_eq!(cfg.hooks.external[0].timeout_seconds, None);
    assert_eq!(cfg.hooks.external[0].config, toml::Value::Table(toml::value::Table::new()));
    assert_eq!(cfg.hooks.external[1].id, "blocker");
}
```

(Note: if `AppConfig::load_from_str` doesn't exist yet, check `config.rs` — if only `load_from(path)` exists, define a small inline `toml::from_str` helper inside the test instead.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package parrot-config --test config_test`
Expected: FAIL — `no field external` or `cannot deserialize HooksConfig`.

- [ ] **Step 3: Add `ExternalHookConfig` struct + extend `HooksConfig`**

In `crates/parrot-config/src/config.rs`, after `HooksConfig` struct definition (line ~73), add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalHookConfig {
    pub id: String,
    pub command: Vec<String>,
    pub events: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub config: toml::Value,
}
```

And extend `HooksConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_seconds: u64,
    #[serde(default, flatten)]
    pub configs: HashMap<String, toml::Value>,
    #[serde(default)]
    pub external: Vec<ExternalHookConfig>,
}
```

Update `Default for HooksConfig`:

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
            external: Vec::new(),
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --package parrot-config --test config_test`
Expected: PASS — both new tests + existing tests (verify `external` field is backwards-compatible when absent)

- [ ] **Step 5: Verify build + lint + fmt**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/parrot-config/src/config.rs crates/parrot-config/tests/config_test.rs
git commit -m "feat(config): add [[hooks.external]] subtable parsing"
```

---

### Task 4: Add raw deps to `parrot-hooks/Cargo.toml`

**Goal:** `tokio` (process feature) + `serde_json` move from dev-deps to deps for `ExternalHook::handle` runtime use.

**Files:**
- Modify: `crates/parrot-hooks/Cargo.toml`

- [ ] **Step 1: Move tokio + serde_json from dev-deps to deps**

Edit `crates/parrot-hooks/Cargo.toml` — final file:

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
serde_json     = { workspace = true }
tokio          = { workspace = true, features = ["process", "io-util", "time", "rt"] }
toml           = { workspace = true }
tracing        = { workspace = true }

[dev-dependencies]
uuid       = { workspace = true }
parrot-protocol = { path = "../parrot-protocol" }
```

Note: `serde_json` was dev-only; moved up to deps. `tokio` was dev-only with default workspace features; now in deps with narrowed features (`process`, `io-util`, `time`, `rt`).

- [ ] **Step 2: Verify build**

Run:
```bash
cargo build --package parrot-hooks
```

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/parrot-hooks/Cargo.toml
git commit -m "chore(parrot-hooks): promote tokio+serde_json to runtime deps for ExternalHook"
```

---

### Task 5: Create `external.rs` with `ExternalHook` struct + pure-fn parsers + tests

**Goal:** Skeleton file with config → struct construction, payload-event → JSON envelope helpers, and stdout last-line → `HookAction` parser. Each pure function independently testable. The `impl Hook::handle` body will be filled in Task 6; in Task 5 we stub it to `Ok(HookAction::NoOp)` so build stays green.

**Files:**
- Create: `crates/parrot-hooks/src/external.rs`
- Modify: `crates/parrot-hooks/src/lib.rs` (add `pub mod external;`)

**Interfaces:**
- Produces: `ExternalHook { id, command, points, timeout_override, config }`
- Produces: `ExternalHook::new(cfg: &ExternalHookConfig, global_timeout: Duration) -> Result<Self, String>`
- Produces: `pub(crate) fn parse_points(events: &[String]) -> Result<HookPoints, String>` (pure)
- Produces: `pub(crate) fn parse_action(last_line: &str) -> Result<HookAction, String>` (pure)
- Produces: pure `fn build_envelope(event: &HookEvent<'_>, config: &serde_json::Value, hook_id: &str, working_dir: &Path, timeout_ms: u64) -> serde_json::Value` (pure; used in Task 6)
- Produces: `impl Hook for ExternalHook` (stub `handle` returns `Ok(HookAction::NoOp)` Task 5, real impl Task 6)

- [ ] **Step 1: Create `external.rs` file with struct + pure functions**

`crates/parrot-hooks/src/external.rs`:

```rust
//! ExternalHook: invokes a user-supplied child process per hook event.
//!
//! Lifecycle: spawn child → write JSON envelope to stdin (tokio writer task)
//! → await stdout with timeout → parse last non-empty line as JSON action →
//! fail-open `Err(AgentError::ExternalHook{...})` on any failure.
//!
//! Strict separation of concerns:
//! - `parse_points` and `parse_action` are pure fns, unit-tested below.
//! - `build_envelope` is pure; serialization happens in `handle`.

use crate::error::IntoHookError;
use parrot_config::ExternalHookConfig;
use parrot_core::error::AgentError;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// Discriminator enum for the wire JSON returned by the child process.
/// Tag is `action` (not `kind`, which is HookAction's serde tag).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WireAction {
    Noop,
    Block { reason: String },
    InjectMessages { messages: Vec<parrot_core::types::ChatMessage> },
    ReplaceResult { content: String, is_error: bool },
    ReplaceContext { messages: Vec<parrot_core::types::ChatMessage> },
}

impl WireAction {
    fn into_hook_action(self) -> HookAction {
        match self {
            WireAction::Noop => HookAction::NoOp,
            WireAction::Block { reason } => HookAction::Block { reason },
            WireAction::InjectMessages { messages } => HookAction::InjectMessages { messages },
            WireAction::ReplaceResult { content, is_error } => {
                HookAction::ReplaceResult { content, is_error }
            }
            WireAction::ReplaceContext { messages } => HookAction::ReplaceContext { messages },
        }
    }
}

pub struct ExternalHook {
    id: String,
    command: Vec<String>,
    points: HookPoints,
    timeout_override: Option<Duration>,
    config: serde_json::Value,
}

impl ExternalHook {
    /// Build an `ExternalHook` from config. Unknown `events` strings return
    /// `Err`; the caller (`build_registry`) converts that to a warn + skip.
    pub fn new(cfg: &ExternalHookConfig, _global_timeout: Duration) -> Result<Self, String> {
        let points = parse_points(&cfg.events)?;
        let timeout_override = cfg
            .timeout_seconds
            .map(|s| Duration::from_secs(s.max(1)));
        let config = toml_to_json(&cfg.config)?;
        Ok(Self {
            id: cfg.id.clone(),
            command: cfg.command.clone(),
            points,
            timeout_override,
            config,
        })
    }

    fn resolve_timeout(&self, ctx: &HookCtx<'_>) -> Duration {
        self.timeout_override.unwrap_or(ctx.timeout)
    }
}

#[async_trait::async_trait]
impl Hook for ExternalHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn supported(&self) -> HookPoints {
        self.points
    }

    async fn handle(
        &self,
        _event: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookAction, AgentError> {
        // Filled in Task 6.
        Ok(HookAction::NoOp)
    }
}

/// Convert `toml::Value` to `serde_json::Value`. Datetime variants serialize
/// as ISO strings via the two-step `toml::to_string` → `serde_json::from_str`.
fn toml_to_json(v: &toml::Value) -> Result<serde_json::Value, String> {
    let s = toml::to_string(v).map_err(|e| format!("toml->string: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("string->json: {e}"))
}

/// Pure: parse HookPoints bitflag from event-kind strings.
pub(crate) fn parse_points(events: &[String]) -> Result<HookPoints, String> {
    let mut p = HookPoints::empty();
    for ev in events {
        let bit = match ev.as_str() {
            "agent_start" => HookPoints::AGENT_START,
            "agent_end" => HookPoints::AGENT_END,
            "turn_start" => HookPoints::TURN_START,
            "tool_call" => HookPoints::TOOL_CALL,
            "tool_execution_start" => HookPoints::TOOL_EXECUTION_START,
            "tool_result" => HookPoints::TOOL_RESULT,
            "context_ready" => HookPoints::CONTEXT_READY,
            other => return Err(format!("unknown event kind: {other}")),
        };
        p |= bit;
    }
    Ok(p)
}

/// Pure: parse the last non-empty line of stdout into a HookAction.
/// Empty / whitespace-only input ⇒ `Ok(HookAction::NoOp)` (silent allow).
pub(crate) fn parse_action(last_line: &str) -> Result<HookAction, String> {
    let trimmed = last_line.trim();
    if trimmed.is_empty() {
        return Ok(HookAction::NoOp);
    }
    let wire: WireAction =
        serde_json::from_str(trimmed).map_err(|e| format!("parse error: {e}"))?;
    Ok(wire.into_hook_action())
}

/// Pure: build the JSON envelope written to the child process's stdin.
/// Returned `Value` is `to_string`'d upstream; no IO here.
pub(crate) fn build_envelope(
    event: &HookEvent<'_>,
    config: &serde_json::Value,
    hook_id: &str,
    working_dir: &Path,
    timeout_ms: u64,
) -> serde_json::Value {
    let event_json = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "event": event_json,
        "config": config,
        "hook_id": hook_id,
        "working_dir": working_dir.display().to_string(),
        "timeout_ms": timeout_ms,
    })
}

/// Helper: take last non-empty line from a byte buffer. Pure.
fn last_non_empty_line(stdout: &[u8]) -> &str {
    let s = std::str::from_utf8(stdout).unwrap_or("");
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_core::types::{ChatMessage, ChatRole};

    #[test]
    fn parse_points_known_events() {
        let p = parse_points(&["tool_call".into(), "tool_result".into()]).unwrap();
        assert_eq!(p, HookPoints::TOOL_CALL | HookPoints::TOOL_RESULT);
    }

    #[test]
    fn parse_points_all_seven() {
        let all = [
            "agent_start", "agent_end", "turn_start", "tool_call",
            "tool_execution_start", "tool_result", "context_ready",
        ];
        let p = parse_points(&all.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        assert_eq!(p, HookPoints::all());
    }

    #[test]
    fn parse_points_unknown_errors() {
        let err = parse_points(&["foo_bar".into()]).unwrap_err();
        assert!(err.contains("unknown event kind: foo_bar"));
    }

    #[test]
    fn parse_points_empty_is_empty_flag() {
        let p = parse_points(&[]).unwrap();
        assert_eq!(p, HookPoints::empty());
    }

    #[test]
    fn parse_action_block() {
        let a = parse_action(r#"{"action":"block","reason":"x"}"#).unwrap();
        assert_eq!(a, HookAction::Block { reason: "x".into() });
    }

    #[test]
    fn parse_action_inject_messages() {
        let msg = ChatMessage {
            role: ChatRole::System,
            content: "hi".into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        };
        let body = format!(
            r#"{{"action":"inject_messages","messages":[{}]}}"#,
            serde_json::json!(msg)
        );
        let a = parse_action(&body).unwrap();
        match a {
            HookAction::InjectMessages { messages } => assert_eq!(messages.len(), 1),
            other => panic!("expected InjectMessages, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_replace_result() {
        let a = parse_action(r#"{"action":"replace_result","content":"y","is_error":true}"#)
            .unwrap();
        assert_eq!(
            a,
            HookAction::ReplaceResult { content: "y".into(), is_error: true }
        );
    }

    #[test]
    fn parse_action_replace_context() {
        let msg = ChatMessage {
            role: ChatRole::System,
            content: "ctx".into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        };
        let body = format!(
            r#"{{"action":"replace_context","messages":[{}]}}"#,
            serde_json::json!(msg)
        );
        let a = parse_action(&body).unwrap();
        match a {
            HookAction::ReplaceContext { messages } => assert_eq!(messages.len(), 1),
            other => panic!("expected ReplaceContext, got {other:?}"),
        }
    }

    #[test]
    fn parse_action_noop_explicit() {
        let a = parse_action(r#"{"action":"noop"}"#).unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    #[test]
    fn parse_action_empty_is_silent_allow() {
        let a = parse_action("").unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    #[test]
    fn parse_action_whitespace_only_is_noop() {
        let a = parse_action("   \n  \t ").unwrap();
        assert_eq!(a, HookAction::NoOp);
    }

    #[test]
    fn parse_action_unknown_action_errors() {
        let err = parse_action(r#"{"action":"record_kv"}"#).unwrap_err();
        assert!(err.contains("parse error") || err.contains("unknown variant"));
    }

    #[test]
    fn parse_action_missing_field_errors() {
        let err = parse_action(r#"{"action":"block"}"#).unwrap_err();
        assert!(err.contains("parse error") || err.contains("missing field"));
    }

    #[test]
    fn parse_action_non_json_errors() {
        let err = parse_action("this is just a debug line").unwrap_err();
        assert!(err.contains("parse error"));
    }

    #[test]
    fn build_envelope_carries_hook_id_and_path() {
        let event = HookEvent::AgentStart {
            session_id: uuid::Uuid::nil(),
            model: "claude-x",
            provider: "anthropic",
        };
        let cfg = serde_json::json!({"sink": "stderr"});
        let env =
            build_envelope(&event, &cfg, "my-hook", Path::new("/tmp/proj"), 3000);
        assert_eq!(env["hook_id"], "my-hook");
        assert_eq!(env["working_dir"], "/tmp/proj");
        assert_eq!(env["timeout_ms"], 3000);
        assert_eq!(env["config"]["sink"], "stderr");
        assert_eq!(env["event"]["type"], "agent_start");
    }

    #[test]
    fn last_non_empty_line_picks_trailing_json() {
        let stdout = b"debug info\nmore log\n{\"action\":\"noop\"}\n";
        let s = last_non_empty_line(stdout);
        assert_eq!(s, "{\"action\":\"noop\"}");
    }

    #[test]
    fn last_non_empty_line_empty_buffer() {
        assert_eq!(last_non_empty_line(b""), "");
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/parrot-hooks/src/lib.rs`, add at the top among module decls:

```rust
pub mod dangerous_command_blocker;
pub mod external;
pub mod redact_secrets;
pub mod shell_denylist;
```

- [ ] **Step 3: Verify build + clippy**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: build clean. (Some `WireAction`/`build_envelope` may have `dead_code` warnings — they are consumed in Task 6; `pub(crate)` mitigates this. Run `cargo build` to confirm.)

- [ ] **Step 4: Run unit tests**

Run: `cargo test --package parrot-hooks --lib`
Expected: PASS — all `parse_points`, `parse_action`, `build_envelope`, `last_non_empty_line` tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-hooks/src/external.rs crates/parrot-hooks/src/lib.rs
git commit -m "feat(parrot-hooks): ExternalHook skeleton + pure-fn parsers + unit tests"
```

---

### Task 6: Wire `ExternalHook::handle` — spawn child, IPC, parse, fail-open

**Goal:** Replace the stub `Ok(NoOp)` with full child-process invocation, timeout, stderr log, fail-open via `Err(AgentError::ExternalHook)`.

**Files:**
- Modify: `crates/parrot-hooks/src/external.rs` — replace `handle` body

**Interfaces:**
- Consumes: pure helpers from Task 5 (`parse_action`, `build_envelope`, `last_non_empty_line`)
- Consumes: `AgentError::ExternalHook` from Task 2
- Produces: full working `impl Hook::handle`

- [ ] **Step 1: Replace `handle` body**

In `crates/parrot-hooks/src/external.rs`, replace the `handle` fn:

```rust
async fn handle(
    &self,
    event: HookEvent<'_>,
    ctx: &HookCtx<'_>,
) -> Result<HookAction, AgentError> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let timeout = self.resolve_timeout(ctx);
    let timeout_ms = timeout.as_millis() as u64;
    let envelope = build_envelope(&event, &self.config, &self.id, ctx.working_dir, timeout_ms);
    let envelope_str =
        serde_json::to_string(&envelope).map_err(|e| AgentError::ExternalHook {
            hook_id: self.id.clone(),
            detail: format!("envelope serialize: {e}"),
        })?;

    if self.command.is_empty() {
        return Err(AgentError::ExternalHook {
            hook_id: self.id.clone(),
            detail: "empty command".into(),
        });
    }

    let mut child = Command::new(&self.command[0])
        .args(&self.command[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AgentError::ExternalHook {
            hook_id: self.id.clone(),
            detail: format!("spawn: {e}"),
        })?;

    // writer task: feed stdin then close (drop)
    let stdin = child.stdin.take().expect("piped");
    let envelope_bytes = format!("{}\n", envelope_str).into_bytes();
    let writer_handle: tokio::task::JoinHandle<std::io::Result<()>> =
        tokio::spawn(async move {
            let mut stdin = stdin;
            stdin.write_all(&envelope_bytes).await
        });

    // await output with timeout
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            // wait_with_output spawns the wait, inheriting kill-on-drop on Err path
            self.log_stderr(Vec::new());
            return Err(AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: format!("wait: {e}"),
            });
        }
        Err(_) => {
            // Kill child explicitly to reap; drop would otherwise kill but
            // we want deterministic reaping order.
            let _ = child.start_kill();
            let _ = child.wait().await;
            self.log_stderr(Vec::new());
            return Err(AgentError::ExternalHook {
                hook_id: self.id.clone(),
                detail: "timeout".into(),
            });
        }
    };

    // ignore writer task's result (best-effort)
    let _ = writer_handle.await;

    self.log_stderr(output.stderr.clone());

    if !output.status.success() {
        return Err(AgentError::ExternalHook {
            hook_id: self.id.clone(),
            detail: format!("exit={}", output.status.code().unwrap_or(-1)),
        });
    }

    let last_line = last_non_empty_line(&output.stdout);
    parse_action(last_line).map_err(|detail| AgentError::ExternalHook {
        hook_id: self.id.clone(),
        detail,
    })
}
```

Add the `log_stderr` helper inside `impl ExternalHook` (near `resolve_timeout`):

```rust
fn log_stderr(&self, stderr: Vec<u8>) {
    let stderr = String::from_utf8_lossy(&stderr);
    if stderr.is_empty() {
        return;
    }
    let trimmed = if stderr.len() > 4096 {
        format!("{}...(truncated {} bytes total)", &stderr[..4096], stderr.len())
    } else {
        stderr.to_string()
    };
    tracing::warn!(
        target: "parrotd::ext_hook",
        hook_id = %self.id,
        stderr = %trimmed,
        "external hook produced stderr"
    );
}
```

- [ ] **Step 2: Remove unused stub steps from Task 5 if surfaced by clippy**

Re-check: there should be no `[allow(dead_code)]` left; `WireAction`, `build_envelope` are consumed.

- [ ] **Step 3: Verify build + clippy + fmt**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-hooks/src/external.rs
git commit -m "feat(parrot-hooks): ExternalHook::handle full impl (spawn + stdin/stdout + timeout + fail-open)"
```

---

### Task 7: Wire `build_registry` to register external hooks

**Goal:** `build_registry` now iterates `cfg.external`, constructs `ExternalHook`, registers on `Ok`, warn + skip on `Err`.

**Files:**
- Modify: `crates/parrot-hooks/src/lib.rs:16-43`
- Modify: `crates/parrot-hooks/src/lib.rs` test module (add coverage)

**Interfaces:**
- Consumes: `ExternalHook::new` from Task 5
- Consumes: `HooksConfig.external: Vec<ExternalHookConfig>` from Task 3
- Produces: `build_registry` with mixed internal+external registration

- [ ] **Step 1: Update build_registry body**

In `crates/parrot-hooks/src/lib.rs`, edit `build_registry`:

```rust
use parrot_config::HooksConfig;
use parrot_core::hooks::HookRegistry;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let global_timeout = Duration::from_secs(cfg.timeout_seconds.max(1));
    let mut reg = HookRegistry::new(global_timeout);
    for id in &cfg.enabled {
        match id.as_str() {
            "dangerous_command_blocker" => {
                reg.register(Arc::new(dangerous_command_blocker::DangerousCommandBlocker))
            }
            "redact_secrets" => {
                let hook_cfg = cfg
                    .configs
                    .get("redact_secrets")
                    .and_then(|v| redact_secrets::RedactSecretsConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(redact_secrets::RedactSecrets::new(hook_cfg)))
            }
            "shell_denylist" => {
                let hook_cfg = cfg
                    .configs
                    .get("shell_denylist")
                    .and_then(|v| shell_denylist::ShellDenylistConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(shell_denylist::ShellDenylist::new(hook_cfg)))
            }
            other => warn!(hook_id = other, "unknown hook id in [hooks].enabled; skipping"),
        }
    }
    for ext in &cfg.external {
        match external::ExternalHook::new(ext, global_timeout) {
            Ok(h) => {
                info!(
                    hook_id = %h.id(),
                    points = ?h.supported(),
                    "registered external hook"
                );
                reg.register(Arc::new(h));
            }
            Err(detail) => warn!(
                hook_id = %ext.id,
                error = %detail,
                "failed to build external hook; skipping"
            ),
        }
    }
    Arc::new(reg)
}
```

- [ ] **Step 2: Add unit tests for new branch**

In the `#[cfg(test)] mod tests` block in `crates/parrot-hooks/src/lib.rs`, append:

```rust
#[test]
fn build_registry_unknown_external_event_warns_and_skips() {
    let cfg = HooksConfig {
        enabled: vec![],
        timeout_seconds: 5,
        configs: HashMap::new(),
        external: vec![ExternalHookConfig {
            id: "bad".into(),
            command: vec!["echo".into()],
            events: vec!["unknown_event".into()],
            timeout_seconds: None,
            config: toml::Value::Table(toml::value::Table::new()),
        }],
    };
    let _reg = build_registry(&cfg);
    // registry is created; failed external hook skipped, no panic.
    // (No observability expose-path here — see integration tests for stderr log.)
}

#[test]
fn build_registry_external_with_empty_events_ok() {
    let cfg = HooksConfig {
        enabled: vec![],
        timeout_seconds: 5,
        configs: HashMap::new(),
        external: vec![ExternalHookConfig {
            id: "noop-listener".into(),
            command: vec!["echo".into()],
            events: vec![],
            timeout_seconds: None,
            config: toml::Value::Table(toml::value::Table::new()),
        }],
    };
    let _reg = build_registry(&cfg);
    // empty HookPoints means hook never fires; construction succeeds.
}
```

Add `use parrot_config::ExternalHookConfig;` to the `tests` module imports.

- [ ] **Step 3: Verify build + clippy + fmt**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-hooks/src/lib.rs
git commit -m "feat(parrot-hooks): build_registry registers external hooks from config"
```

---

### Task 8: Create cross-platform mock bin Cargo.toml + first mock bin

**Goal:** Set up `crates/parrot-hooks/tests/fixtures/Cargo.toml` declaring bin targets and create `mock_hook_block`.

**Files:**
- Create: `crates/parrot-hooks/tests/fixtures/Cargo.toml`
- Create: `crates/parrot-hooks/tests/fixtures/mock_hook_block.rs`

- [ ] **Step 1: Create fixtures Cargo.toml**

`crates/parrot-hooks/tests/fixtures/Cargo.toml`:

```toml
[package]
name = "parrot-hooks-fixtures"
version = "0.0.0"
edition.workspace = true
publish = false

[[bin]]
name = "mock_hook_block"
path = "mock_hook_block.rs"

[[bin]]
name = "mock_hook_noop"
path = "mock_hook_noop.rs"

[[bin]]
name = "mock_hook_silent"
path = "mock_hook_silent.rs"

[[bin]]
name = "mock_hook_exit1"
path = "mock_hook_exit1.rs"

[[bin]]
name = "mock_hook_sleep"
path = "mock_hook_sleep.rs"

[[bin]]
name = "mock_hook_unknown"
path = "mock_hook_unknown.rs"
```

This package is consumed by `[dev-dependencies]` parrot-hooks = { path = ".." } (added next task) OR via `env!("CARGO_BIN_EXE_mock_hook_block")` — the latter requires the bins live in the *same* crate. Since `parrot-hooks` itself shouldn't ship these bins in its production binary set, they live in a tiny fixture package built only when running tests.

**Decision:** Use `env!("CARGO_BIN_EXE_<bin>")` — this requires the bins be part of a *separate* test crate OR the main crate. For `parrot-hooks`, we add them via `[[bin]]` directly in the parrot-hooks Cargo.toml's `[lib]` + bins — but that ships them. To avoid shipping, they live in a *separate* test-only crate `parrot-hooks-fixtures` and integration tests read its path relative to the workspace.

Actually, **the canonical cargo pattern** is `env!("CARGO_BIN_EXE_<name>")` — this works when the bin is declared in the same crate that runs the test. Since integration tests live in `crates/parrot-hooks/tests/external_hook_test.rs`, and that's the *parrot-hooks* crate, the bins must be declared in `parrot-hooks/Cargo.toml`. They become part of the parrot-hooks package as published bins.

**Trade-off accepted:** Adding `[[bin]]` to `parrot-hooks/Cargo.toml` ships small mock bins in the published crate. This is a minor size cost (~few KB each), and the alternative (separate fixture crate + relative path plumbing) is significantly more complex. Accept.

**Final decision:** Add the bins to `crates/parrot-hooks/Cargo.toml` directly.

Revert the fixtures Cargo.toml step — instead, edit `crates/parrot-hooks/Cargo.toml`:

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
serde_json     = { workspace = true }
tokio          = { workspace = true, features = ["process", "io-util", "time", "rt"] }
toml           = { workspace = true }
tracing        = { workspace = true }

[dev-dependencies]
uuid       = { workspace = true }
parrot-protocol = { path = "../parrot-protocol" }

[[bin]]
name = "mock_hook_block"
path = "tests/fixtures/mock_hook_block.rs"

[[bin]]
name = "mock_hook_noop"
path = "tests/fixtures/mock_hook_noop.rs"

[[bin]]
name = "mock_hook_silent"
path = "tests/fixtures/mock_hook_silent.rs"

[[bin]]
name = "mock_hook_exit1"
path = "tests/fixtures/mock_hook_exit1.rs"

[[bin]]
name = "mock_hook_sleep"
path = "tests/fixtures/mock_hook_sleep.rs"

[[bin]]
name = "mock_hook_unknown"
path = "tests/fixtures/mock_hook_unknown.rs"
```

- [ ] **Step 2: Create `mock_hook_block.rs`**

`crates/parrot-hooks/tests/fixtures/mock_hook_block.rs`:

```rust
// Mock external hook that emits a block action.
// Drains stdin (envelope) and writes one JSON line.
use std::io::{Read, Write};

fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let out = r#"{"action":"block","reason":"test-block"}"#;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}
```

- [ ] **Step 3: Verify build**

Run:
```bash
cargo build --package parrot-hooks --bins
```

Expected: PASS — builds `mock_hook_block` bin + the others (which don't exist yet, so this will fail)

Hmm — actually all six `[[bin]]` entries are declared but only one file exists. Either declare all up front with stub bodies, or declare one at a time. **Refactor:** declare all six bins AND create all six files in this task, with minimal-stub bodies — then later tasks improve their bodies individually for specific tests.

**Step 1b — revise:** Create all six rust files now. Use this step in the plan instead of iterating.

Create the following six files (copy `mock_hook_block.rs` body for block; the other 5 get stub bodies now, then each is finalized when its test is written):

`crates/parrot-hooks/tests/fixtures/mock_hook_noop.rs`:
```rust
use std::io::{Read, Write};
fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let _ = std::io::stdout().write_all(b"{\"action\":\"noop\"}\n");
}
```

`crates/parrot-hooks/tests/fixtures/mock_hook_silent.rs`:
```rust
use std::io::Read;
fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    // stdout stays empty
}
```

`crates/parrot-hooks/tests/fixtures/mock_hook_exit1.rs`:
```rust
use std::io::{Read, Write};
fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let _ = std::io::stdout().write_all(b"{\"action\":\"noop\"}\n");
    std::process::exit(1);
}
```

`crates/parrot-hooks/tests/fixtures/mock_hook_sleep.rs`:
```rust
use std::io::Read;
use std::time::Duration;
fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    std::thread::sleep(Duration::from_secs(10));
}
```

`crates/parrot-hooks/tests/fixtures/mock_hook_unknown.rs`:
```rust
use std::io::{Read, Write};
fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let _ = std::io::stdout().write_all(b"{\"action\":\"record_kv\"}\n");
}
```

- [ ] **Step 3: Verify build**

Run:
```bash
cargo build --package parrot-hooks --bins
```

Expected: PASS — all six mock bins build

- [ ] **Step 4: Commit**

```bash
git add crates/parrot-hooks/Cargo.toml crates/parrot-hooks/tests/fixtures/
git commit -m "test(parrot-hooks): cross-platform mock hook bins as test fixtures"
```

---

### Task 9: Integration tests for `ExternalHook::handle` via mock bins

**Goal:** Exercise the real `ExternalHook::handle` against the six mock bins, asserting result_kind and panic-safety.

**Files:**
- Create: `crates/parrot-hooks/tests/external_hook_test.rs`

**Interfaces:**
- Consumes: `parrot_hooks::ExternalHook`, `parrot_config::ExternalHookConfig` (need to re-export `ExternalHook` as `pub` from `parrot-hooks`)
- Consumes: mock bin paths via `env!("CARGO_BIN_EXE_mock_hook_*")`

- [ ] **Step 1: Re-export `ExternalHook` as `pub`**

In `crates/parrot-hooks/src/lib.rs`, change `mod external;` to `pub mod external;`.

- [ ] **Step 2: Write the integration tests**

`crates/parrot-hooks/tests/external_hook_test.rs`:

```rust
use parrot_config::ExternalHookConfig;
use parrot_core::hooks::{Hook, HookAction, HookCtx, HookEvent, HookPoints};
use parrot_hooks::external::ExternalHook;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

fn make_hook(command: Vec<String>) -> ExternalHook {
    let cfg = ExternalHookConfig {
        id: "test-hook".into(),
        command,
        events: vec!["tool_call".into()],
        timeout_seconds: Some(1),
        config: toml::Value::Table(toml::value::Table::new()),
    };
    ExternalHook::new(&cfg, Duration::from_secs(5)).unwrap()
}

fn ctx<'a>(timeout: Duration, working_dir: &'a Path) -> HookCtx<'a> {
    HookCtx {
        session_id: Uuid::nil(),
        working_dir,
        timeout,
    }
}

fn tool_call_event() -> HookEvent<'static> {
    // borrow static strings; for test simplicity use leaked constants via Box
    // HookEvent borrows references; test must own args. Use scoped args instead.
    unreachable!()
}

// Because HookEvent borrows, tests construct local args:
fn tool_call_event_with<'a>(args: &'a serde_json::Value) -> HookEvent<'a> {
    HookEvent::ToolCall {
        session_id: Uuid::nil(),
        turn_id: Uuid::nil(),
        parent_message_id: Uuid::nil(),
        tool_call_id: "tc_test",
        tool_name: "shell_exec",
        arguments: args,
    }
}

#[tokio::test]
async fn mock_block_returns_block() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_block").to_string()];
    let hook = make_hook(cmd);
    let args = serde_json::json!({"command":"rm -rf /"});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_secs(2), cwd);
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(out, HookAction::Block { reason: "test-block".into() });
}

#[tokio::test]
async fn mock_noop_returns_noop() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_noop").to_string()];
    let hook = make_hook(cmd);
    let args = serde_json::json!({"command":"ls"});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_secs(2), cwd);
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(out, HookAction::NoOp);
}

#[tokio::test]
async fn mock_silent_returns_noop_silent_allow() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_silent").to_string()];
    let hook = make_hook(cmd);
    let args = serde_json::json!({"command":"ls"});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_secs(2), cwd);
    let out = hook.handle(ev, &c).await.unwrap();
    assert_eq!(out, HookAction::NoOp);
}

#[tokio::test]
async fn mock_exit1_failopen_err() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_exit1").to_string()];
    let hook = make_hook(cmd);
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_secs(2), cwd);
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        parrot_core::error::AgentError::ExternalHook { detail, .. } => {
            assert!(detail.starts_with("exit="));
        }
        other => panic!("expected ExternalHook err, got {other:?}"),
    }
}

#[tokio::test]
async fn mock_timeout_failopen_err() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_sleep").to_string()];
    let mut cfg = ExternalHookConfig {
        id: "sleep".into(),
        command: cmd,
        events: vec!["tool_call".into()],
        timeout_seconds: Some(1),
        config: toml::Value::Table(toml::value::Table::new()),
    };
    cfg.timeout_seconds = Some(0); // clamp to 1s
    let _ = cfg.timeout_seconds.take(); // use ctx timeout below
    let hook = make_hook(vec![env!("CARGO_BIN_EXE_mock_hook_sleep").to_string()]);
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_millis(100), cwd);
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        parrot_core::error::AgentError::ExternalHook { detail, .. } => {
            assert_eq!(detail, "timeout");
        }
        other => panic!("expected timeout err, got {other:?}"),
    }
}

#[tokio::test]
async fn mock_unknown_action_failopen_err() {
    let cmd = vec![env!("CARGO_BIN_EXE_mock_hook_unknown").to_string()];
    let hook = make_hook(cmd);
    let args = serde_json::json!({});
    let ev = tool_call_event_with(&args);
    let cwd = Path::new(".");
    let c = ctx(Duration::from_secs(2), cwd);
    let err = hook.handle(ev, &c).await.unwrap_err();
    match err {
        parrot_core::error::AgentError::ExternalHook { detail, .. } => {
            assert!(detail.contains("unknown variant") || detail.contains("parse error"));
        }
        other => panic!("expected ExternalHook err, got {other:?}"),
    }
}

#[tokio::test]
async fn external_hook_id_supported() {
    let hook = make_hook(vec![env!("CARGO_BIN_EXE_mock_hook_noop").to_string()]);
    assert_eq!(hook.id(), "test-hook");
    assert_eq!(hook.supported(), HookPoints::TOOL_CALL);
}
```

- [ ] **Step 3: Run tests**

Run:
```bash
cargo test --package parrot-hooks --test external_hook_test
```

Expected: PASS — 7 tests across 6 mock scenarios + id/supported.

Note: `parse_action` for `mock_hook_unknown` may yield "unknown variant `record_kv`" — adjust assertion to match actual error message wording from serde. Check implementation; if the message is "unknown variant `record_kv`, expected one of `noop`, `block`, `inject_messages`, `replace_result`, `replace_context`", that satisfies the assertion.

- [ ] **Step 4: Verify build + clippy + fmt + full test**

Run:
```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Expected: PASS — 152+ tests green (was 145 + new tests).

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-hooks/src/lib.rs crates/parrot-hooks/tests/external_hook_test.rs
git commit -m "test(parrot-hooks): integration tests for ExternalHook::handle via mock bins"
```

---

## Plan Self-Review

**1. Spec coverage:**
- Spec §1 decisions: all enforced in Global Constraints
- Spec §4.1 `ExternalHookConfig`: Task 3 ✓
- Spec §4.2 `ExternalHook` struct + impl: Task 5 + 6 ✓
- Spec §4.3 trait `fn id -> &str`: Task 1 ✓
- Spec §4.4 `AgentError::ExternalHook`: Task 2 ✓
- Spec §4.5 `build_registry` extension: Task 7 ✓
- Spec §5 wire protocol (envelope, parse, fail-open matrix): Task 5 (pure fns) + Task 6 (handle) ✓
- Spec §6 failure/timeout/stderr: Task 6 handle body ✓
- Spec §7 tests: Task 3 (config), Task 5 (pure fns), Task 9 (integration) ✓
- Spec §8 backwards compat: Task 3 `Default::default()` keeps `external: Vec::new()` ✓
- Spec §9 deps: Task 4 ✓

**2. Placeholder scan:** No "TBD", "TODO", "appropriate". Notes in Task 5/6 acknowledge serde error message wording is approximate — those are honest, not placeholders; the assertion uses `contains` to be tolerant.

**3. Type consistency:**
- `ExternalHook::new(cfg: &ExternalHookConfig, global_timeout: Duration) -> Result<Self, String>` consistent across Task 5 ↔ Task 7 ✓
- `parse_action(&str) -> Result<HookAction, String>` consistent ✓
- `parse_points(&[String]) -> Result<HookPoints, String>` consistent ✓
- `AgentError::ExternalHook { hook_id, detail }` consistent across Task 2 ↔ Task 6 ✓
- Mock bin names `mock_hook_block` / `mock_hook_noop` / etc. consistent across Task 8 ↔ Task 9 ✓

**4. Scope check:** Single capability, 9 tasks; reasonable for SDD.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-30-parrot-external-hooks.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?