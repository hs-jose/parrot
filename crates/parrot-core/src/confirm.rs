//! Routes `ClientMessage::ConfirmToolCall` responses to the session task
//! waiting for them.
//!
//! When a tool call matches `require_confirmation`, the engine creates a
//! `oneshot::channel`, hands the `Sender` to the `ConfirmRouter` keyed by
//! `(session_id, tool_id)`, and awaits the `Receiver`. The daemon's
//! connection handler looks up the `Sender` when a `ConfirmToolCall` arrives
//! from the client and resolves it with the client's `ConfirmDecision`.
//!
//! Entries live only for the duration of an in-flight confirmation, so the
//! map is normally empty — zero overhead on the hot path. See
//! `2026-06-21-parrot-phase-1.5.md` §3.5 for the full flow.
//!
//! Lives in `parrot-core` (not the daemon) so the engine can hold a handle
//! without crossing the core↔daemon dependency boundary. The daemon imports
//! this type and shares one `Arc<ConfirmRouter>` across all connection
//! handlers + engine tasks.

use parrot_protocol::types::ConfirmDecision;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use uuid::Uuid;

type Key = (Uuid, String);

#[derive(Clone)]
pub struct ConfirmRouter {
    pending: Arc<RwLock<HashMap<Key, oneshot::Sender<ConfirmDecision>>>>,
}

impl ConfirmRouter {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a pending confirmation. The engine calls this just before
    /// emitting `AgentEvent::ToolConfirmRequired` and then awaits
    /// `rx`. If the client responds before the timeout, `resolve` consumes
    /// the sender and `rx` yields the decision; otherwise the engine's
    /// `tokio::time::timeout` fires and the sender is dropped (a stale
    /// late client response then no-ops in `resolve`).
    pub async fn register(
        &self,
        session_id: Uuid,
        tool_id: String,
        sender: oneshot::Sender<ConfirmDecision>,
    ) {
        self.pending
            .write()
            .await
            .insert((session_id, tool_id), sender);
    }

    /// Remove a pending entry without resolving it. Called by the engine
    /// after it has either received a decision or timed out — ensures a
    /// late client response doesn't find a stale sender. Safe to call even
    /// if the entry was already removed by `resolve`.
    pub async fn unregister(&self, session_id: &Uuid, tool_id: &str) {
        self.pending
            .write()
            .await
            .remove(&(*session_id, tool_id.to_string()));
    }

    /// Deliver a client's decision to the waiting session task. Returns
    /// `true` if a waiter was found and notified, `false` if no pending
    /// confirmation exists for this `(session_id, tool_id)` (either it was
    /// already resolved, already timed out, or there was never a
    /// confirmation request — e.g. a buggy client).
    pub async fn resolve(
        &self,
        session_id: Uuid,
        tool_id: &str,
        decision: ConfirmDecision,
    ) -> bool {
        let mut map = self.pending.write().await;
        match map.remove(&(session_id, tool_id.to_string())) {
            Some(sender) => {
                // `send` fails if the receiver was dropped (engine task
                // died). That's fine — we just discard the decision.
                let _ = sender.send(decision);
                true
            }
            None => false,
        }
    }
}

impl Default for ConfirmRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_resolve_delivers_decision() {
        let router = ConfirmRouter::new();
        let (tx, rx) = oneshot::channel();
        let sid = Uuid::new_v4();
        router.register(sid, "tc_1".to_string(), tx).await;

        assert!(
            router.resolve(sid, "tc_1", ConfirmDecision::Approve).await,
            "resolve should find the pending entry"
        );

        let decision = rx.await.expect("receiver should yield");
        assert_eq!(decision, ConfirmDecision::Approve);
    }

    #[tokio::test]
    async fn resolve_unknown_key_returns_false() {
        let router = ConfirmRouter::new();
        assert!(
            !router
                .resolve(Uuid::new_v4(), "nope", ConfirmDecision::Reject)
                .await,
            "resolve on empty router should return false"
        );
    }

    #[tokio::test]
    async fn unregister_makes_subsequent_resolve_noop() {
        let router = ConfirmRouter::new();
        let (tx, _rx) = oneshot::channel::<ConfirmDecision>();
        let sid = Uuid::new_v4();
        router.register(sid, "tc_1".to_string(), tx).await;
        router.unregister(&sid, "tc_1").await;

        assert!(
            !router.resolve(sid, "tc_1", ConfirmDecision::Approve).await,
            "resolve after unregister should return false"
        );
    }

    #[tokio::test]
    async fn resolve_after_receiver_dropped_does_not_panic() {
        let router = ConfirmRouter::new();
        let (tx, rx) = oneshot::channel();
        let sid = Uuid::new_v4();
        router.register(sid, "tc_1".to_string(), tx).await;
        drop(rx);

        // resolve should still return true (entry existed) even though the
        // receiver is gone — the send just silently fails.
        assert!(router.resolve(sid, "tc_1", ConfirmDecision::Reject).await);
    }
}
