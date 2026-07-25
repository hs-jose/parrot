use crate::error::AgentError;
use crate::types::ChatMessage;
use async_trait::async_trait;
use parrot_protocol::types::ToolOutput;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
    pub struct HookPoints: u8 {
        const AGENT_START            = 0b0000_0001;
        const AGENT_END              = 0b0000_0010;
        const TURN_START             = 0b0000_0100;
        const TOOL_CALL              = 0b0000_1000;
        const TOOL_EXECUTION_START   = 0b0001_0000;
        const TOOL_RESULT            = 0b0010_0000;
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEvent<'a> {
    AgentStart {
        session_id: Uuid,
        model: &'a str,
        provider: &'a str,
    },
    AgentEnd {
        session_id: Uuid,
    },
    TurnStart {
        session_id: Uuid,
        turn_id: Uuid,
        user_message: &'a str,
    },
    ToolCall {
        session_id: Uuid,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a serde_json::Value,
    },
    ToolExecutionStart {
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        arguments: &'a serde_json::Value,
    },
    ToolResult {
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        input: &'a serde_json::Value,
        result: &'a ToolOutput,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookResult {
    NoOp,
    InjectMessages { messages: Vec<ChatMessage> },
    Block { reason: String },
    ReplaceResult { content: String, is_error: bool },
}

/// Outcome of a hook dispatch when it failed (timeout or returned `Err`).
/// Used to drive the `error`/`timeout` `HookFired` emissions without
/// collapsing the `AgentError` detail to a string literal.
#[derive(Debug)]
pub enum HookFailure {
    Timeout,
    Error(String),
}

pub struct HookCtx<'a> {
    pub session_id: Uuid,
    pub working_dir: &'a std::path::Path,
    pub timeout: Duration,
}

impl<'a> HookEvent<'a> {
    /// Lowercase ascii kind name matching the wire `event_kind` field of
    /// `AgentEvent::HookFired`. Single source of truth for both engine and
    /// registry emit paths.
    pub fn kind(&self) -> &'static str {
        match self {
            HookEvent::AgentStart { .. } => "agent_start",
            HookEvent::AgentEnd { .. } => "agent_end",
            HookEvent::TurnStart { .. } => "turn_start",
            HookEvent::ToolCall { .. } => "tool_call",
            HookEvent::ToolExecutionStart { .. } => "tool_execution_start",
            HookEvent::ToolResult { .. } => "tool_result",
        }
    }
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &'static str;
    fn supported(&self) -> HookPoints;
    async fn dispatch(
        &self,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookResult, AgentError>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnStartDecision {
    Continue { injected_messages: Vec<ChatMessage> },
    Blocked { hook_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallDecision {
    Continue,
    Blocked { hook_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultDecision {
    Continue,
    Replace {
        hook_id: String,
        content: String,
        is_error: bool,
    },
}

pub struct HookRegistry {
    handlers: Vec<Arc<dyn Hook>>,
    timeout: Duration,
}

/// Drive the non-Block `HookFired` emission from inside the registry's
/// dispatch helpers. `Block` is intentionally NOT emitted here — the engine
/// computes Block emission itself (it knows the surrounding context, e.g.
/// the matching `TurnEnd{BlockedHook}` / `ToolEnd{blocked: ...}` follows).
/// Caller passes `&outcome` so non-moved arms can read fields.
fn fire_non_block(
    emit: &mut impl FnMut(&str, &str, &str, Option<String>),
    hook_id: &str,
    event_kind: &str,
    outcome: &Result<HookResult, HookFailure>,
) {
    match outcome {
        Ok(HookResult::NoOp) => emit(hook_id, event_kind, "noop", None),
        Ok(HookResult::InjectMessages { messages }) => emit(
            hook_id,
            event_kind,
            "inject_messages",
            Some(format!("{} messages", messages.len())),
        ),
        Ok(HookResult::ReplaceResult { content, is_error }) => emit(
            hook_id,
            event_kind,
            "replace_result",
            Some(format!("{} bytes, is_error={}", content.len(), is_error)),
        ),
        Ok(HookResult::Block { .. }) => {}
        Err(HookFailure::Timeout) => emit(hook_id, event_kind, "timeout", None),
        Err(HookFailure::Error(detail)) => emit(hook_id, event_kind, "error", Some(detail.clone())),
    }
}

impl HookRegistry {
    pub fn new(timeout: Duration) -> Self {
        Self {
            handlers: Vec::new(),
            timeout,
        }
    }

    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.handlers.push(hook);
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn mk_ctx<'a>(&self, session_id: Uuid, working_dir: &'a std::path::Path) -> HookCtx<'a> {
        HookCtx {
            session_id,
            working_dir,
            timeout: self.timeout,
        }
    }

    async fn bounded(
        &self,
        hook: &Arc<dyn Hook>,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookResult, HookFailure> {
        match tokio::time::timeout(self.timeout, hook.dispatch(event, ctx)).await {
            Ok(Ok(o)) => Ok(o),
            Ok(Err(e)) => Err(HookFailure::Error(e.to_string())),
            Err(_) => Err(HookFailure::Timeout),
        }
    }

    fn interested(&self, point: HookPoints) -> Vec<Arc<dyn Hook>> {
        self.handlers
            .iter()
            .filter(|h| h.supported().contains(point))
            .cloned()
            .collect()
    }

    pub async fn on_agent_start(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
        model: &str,
        provider: &str,
    ) {
        let hs = self.interested(HookPoints::AGENT_START);
        if hs.is_empty() {
            return;
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        for h in hs {
            let ev = HookEvent::AgentStart {
                session_id,
                model,
                provider,
            };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
        }
    }

    pub async fn on_agent_end(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
    ) {
        let hs = self.interested(HookPoints::AGENT_END);
        if hs.is_empty() {
            return;
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        for h in hs {
            let ev = HookEvent::AgentEnd { session_id };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
        }
    }

    pub async fn on_tool_execution_start(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) {
        let hs = self.interested(HookPoints::TOOL_EXECUTION_START);
        if hs.is_empty() {
            return;
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        for h in hs {
            let ev = HookEvent::ToolExecutionStart {
                session_id,
                turn_id,
                tool_call_id,
                tool_name,
                arguments,
            };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
        }
    }

    pub async fn on_turn_start(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
        turn_id: Uuid,
        user_message: &str,
    ) -> TurnStartDecision {
        let hs = self.interested(HookPoints::TURN_START);
        if hs.is_empty() {
            return TurnStartDecision::Continue {
                injected_messages: Vec::new(),
            };
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        let mut injected = Vec::new();
        for h in hs {
            let ev = HookEvent::TurnStart {
                session_id,
                turn_id,
                user_message,
            };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
            match outcome {
                Ok(HookResult::Block { reason }) => {
                    return TurnStartDecision::Blocked {
                        hook_id: h.id().to_string(),
                        reason,
                    }
                }
                Ok(HookResult::InjectMessages { messages }) => injected.extend(messages),
                _ => {}
            }
        }
        TurnStartDecision::Continue {
            injected_messages: injected,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn on_tool_call(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> ToolCallDecision {
        let hs = self.interested(HookPoints::TOOL_CALL);
        if hs.is_empty() {
            return ToolCallDecision::Continue;
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        for h in hs {
            let ev = HookEvent::ToolCall {
                session_id,
                turn_id,
                parent_message_id,
                tool_call_id,
                tool_name,
                arguments,
            };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
            if let Ok(HookResult::Block { reason }) = outcome {
                return ToolCallDecision::Blocked {
                    hook_id: h.id().to_string(),
                    reason,
                };
            }
        }
        ToolCallDecision::Continue
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn on_tool_result(
        &self,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
        result: &ToolOutput,
    ) -> ToolResultDecision {
        let hs = self.interested(HookPoints::TOOL_RESULT);
        if hs.is_empty() {
            return ToolResultDecision::Continue;
        }
        let ctx = self.mk_ctx(session_id, std::path::Path::new("."));
        let mut current = result.clone();
        let mut changed = false;
        let mut last_hook_id = String::new();
        for h in hs {
            let ev = HookEvent::ToolResult {
                session_id,
                turn_id,
                tool_call_id,
                tool_name,
                input,
                result: &current,
            };
            let kind = ev.kind();
            let outcome = self.bounded(&h, ev, &ctx).await;
            fire_non_block(emit, h.id(), kind, &outcome);
            if let Ok(HookResult::ReplaceResult { content, is_error }) = outcome {
                current = ToolOutput { content, is_error };
                changed = true;
                last_hook_id = h.id().to_string();
            }
        }
        if changed {
            ToolResultDecision::Replace {
                hook_id: last_hook_id,
                content: current.content,
                is_error: current.is_error,
            }
        } else {
            ToolResultDecision::Continue
        }
    }
}

impl HookRegistry {
    pub fn empty() -> Self {
        Self::new(Duration::from_secs(5))
    }
}
