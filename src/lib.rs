//! Parrot — library facade exposing daemon internals for integration tests.
//!
//! The real entrypoints are the binaries (`parrotd`, `parrot`). This crate
//! exists so the integration-test target can spin up a daemon with an injected
//! mock provider without going through the config-driven Anthropic loader.
//!
//! The daemon modules live under `src/daemon/` (also the `parrotd` bin root),
//! so we pull them in by path rather than relying on the default module
//! resolution.

#[path = "daemon/auth.rs"]
pub mod auth;

#[path = "daemon/server.rs"]
pub mod server;

#[path = "daemon/session_store.rs"]
pub mod session_store;

#[path = "daemon/providers/mod.rs"]
pub mod providers;

#[path = "daemon/tools/mod.rs"]
pub mod tools;
