# AGENTS.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

Tradeoff: These guidelines bias toward caution over speed. For trivial tasks, use judgment.

1. Think Before Coding
Don't assume. Don't hide confusion. Surface tradeoffs.

Before implementing:

State your assumptions explicitly. If uncertain, ask.
If multiple interpretations exist, present them - don't pick silently.
If a simpler approach exists, say so. Push back when warranted.
If something is unclear, stop. Name what's confusing. Ask.
2. Simplicity First
Minimum code that solves the problem. Nothing speculative.

No features beyond what was asked.
No abstractions for single-use code.
No "flexibility" or "configurability" that wasn't requested.
No error handling for impossible scenarios.
If you write 200 lines and it could be 50, rewrite it.
Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

3. Surgical Changes
Touch only what you must. Clean up only your own mess.

When editing existing code:

Don't "improve" adjacent code, comments, or formatting.
Don't refactor things that aren't broken.
Match existing style, even if you'd do it differently.
If you notice unrelated dead code, mention it - don't delete it.
When your changes create orphans:

Remove imports/variables/functions that YOUR changes made unused.
Don't remove pre-existing dead code unless asked.
The test: Every changed line should trace directly to the user's request.

4. Goal-Driven Execution
Define success criteria. Loop until verified.

Transform tasks into verifiable goals:

"Add validation" → "Write tests for invalid inputs, then make them pass"
"Fix the bug" → "Write a test that reproduces it, then make it pass"
"Refactor X" → "Ensure tests pass before and after"
For multi-step tasks, state a brief plan:

1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

These guidelines are working if: fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

Build, test, and lint commands for the Parrot workspace. Read this before running any verification step.

## Build

```bash
cargo build --workspace
```

## Test

```bash
cargo test --workspace
```

Includes unit tests (per-crate `#[cfg(test)]` modules), integration tests under `tests/`, and the e2e tests (`tests/integration/e2e_test.rs` and `tests/integration/phase15_test.rs`) which spin up a daemon with a mock provider injected in place of Anthropic. The `tests/cassette_test.rs` target drives the engine via JSON cassettes under `tests/cassettes/anthropic/`. No network, no API key required.

## Lint

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Run locally

```bash
export ANTHROPIC_API_KEY=sk-...
cargo run --bin parrotd          # daemon
cargo run --bin parrot            # interactive CLI (another terminal)
cargo run --bin parrot -- -m "list files in the current directory"
```

## Architecture quick reference

- `parrot-core` — zero-IO orchestration: `Tool`/`LlmProvider` traits, ReAct engine, session state, event log
- `parrot-protocol` — pure serde WS message types
- `parrot-transport` — `TransportServer` / `TransportClient` traits + tokio-tungstenite impl
- `parrot-config` — TOML + `dirs` path resolution
- `src/daemon/` — `parrotd` binary: WS server, Anthropic adapter, built-in tool implementations, auth (token + Origin check)
- `src/cli/` — `parrot` binary: thin WS client

The daemon is the only component allowed to do IO. Clients talk to it exclusively via `parrot-protocol` messages over `parrot-transport`.

## Design document

See `docs/superpowers/specs/2026-06-21-parrot-design.md` for the full technical spec, including Phase 1 implementation status, the `ConfirmToolCall` flow (Phase 1.5), and the cassette test framework design.

## Conventions

- Errors crossing crate boundaries use `thiserror` enums (`AgentError`, `ProviderError`, `TransportError`, `ConfigError`, `ProtocolError`). `anyhow` is allowed only inside binaries.
- `parrot-core` has zero IO: no `reqwest`, no `tokio::fs`, no `std::env`. Tool and provider implementations live in `src/daemon/`.
- WS messages are tagged enums (`#[serde(tag = "type")]`). When adding a new message, update both `client_message.rs` / `server_message.rs` and the roundtrip tests in `crates/parrot-protocol/tests/roundtrip.rs`.
- When changing the `StreamEvent` ↔ `ServerMessage` mapping, update `src/daemon/session_adapter.rs` and the table in the design doc §3.
- Auth token file is `0600` on Unix (see `src/daemon/auth.rs`); preserve this when touching auth.
