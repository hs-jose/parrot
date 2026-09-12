# Parrot

A Rust LLM agent with multi-provider support, local/cloud hybrid deployment, and daemon+thin-client architecture.

## Architecture

Parrot uses a **daemon + thin client** architecture:
- **parrotd** — The agent daemon that manages sessions, LLM providers, and tool execution
- **parrot** — A thin CLI client that connects to the daemon via WebSocket

Agent core logic lives in `parrot-core` (zero-IO), communication in `parrot-protocol` (pure data), transport in `parrot-transport` (WS abstraction), and configuration in `parrot-config`.

## Quick Start

```bash
# Set your API key
export ANTHROPIC_API_KEY=sk-...

# Start the daemon
cargo run --bin parrotd

# Connect with the CLI (in another terminal)
cargo run --bin parrot

# Or send a single message
cargo run --bin parrot -- --message "List the files in the current directory"
```

## Configuration

Create `parrot.toml` in the current directory or `~/.config/parrot/parrot.toml`:

```toml
[daemon]
host = "127.0.0.1"
port = 9876

[provider]
id = "anthropic"
protocol = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10
```

## Crate Structure

| Crate | Purpose |
|-------|---------|
| `parrot-core` | Orchestrator, Tool/Provider traits, Session management |
| `parrot-protocol` | WebSocket message types (pure serde) |
| `parrot-transport` | Transport trait + WS implementation |
| `parrot-config` | Configuration file parsing |

## Design Document

See [docs/superpowers/specs/2026-06-21-parrot-design.md](docs/superpowers/specs/2026-06-21-parrot-design.md) for the full technical specification.

## License

MIT
