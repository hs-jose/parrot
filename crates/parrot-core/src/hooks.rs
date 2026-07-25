use crate::error::AgentError;
use crate::types::ChatMessage;
use async_trait::async_trait;
use parrot_protocol::types::ToolOutput;
use std::path::Path;
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

#[derive(Debug, Clone, Copy, serde::Serialize)]
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

impl<'a> HookEvent<'a> {
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

    pub fn point(&self) -> HookPoints {
        match self {
            HookEvent::AgentStart { .. } => HookPoints::AGENT_START,
            HookEvent::AgentEnd { .. } => HookPoints::AGENT_END,
            HookEvent::TurnStart { .. } => HookPoints::TURN_START,
            HookEvent::ToolCall { .. } => HookPoints::TOOL_CALL,
            HookEvent::ToolExecutionStart { .. } => HookPoints::TOOL_EXECUTION_START,
            HookEvent::ToolResult { .. } => HookPoints::TOOL_RESULT,
        }
    }

    pub fn session_id(&self) -> Uuid {
        match self {
            HookEvent::AgentStart { session_id, .. }
            | HookEvent::AgentEnd { session_id }
            | HookEvent::TurnStart { session_id, .. }
            | HookEvent::ToolCall { session_id, .. }
            | HookEvent::ToolExecutionStart { session_id, .. }
            | HookEvent::ToolResult { session_id, .. } => *session_id,
        }
    }
}

/// Per-hook return value (what a single hook decides to do).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookAction {
    NoOp,
    InjectMessages { messages: Vec<ChatMessage> },
    Block { reason: String },
    ReplaceResult { content: String, is_error: bool },
}

/// Aggregated result returned by [`HookRegistry::run`] after running all
/// interested hooks in waterfall order.
#[derive(Debug, Clone, PartialEq)]
pub enum HookResult {
    /// No hook intervened; caller continues normally.
    Continue,
    /// A hook blocked the operation (bail-on-first).
    Block { hook_id: String, reason: String },
    /// One or more TurnStart hooks injected messages (accumulated across all hooks).
    Inject { messages: Vec<ChatMessage> },
    /// A ToolResult hook replaced the output (last-write-wins).
    Replace { hook_id: String, content: String, is_error: bool },
}

/// Internal: a hook call timed out or returned `Err`.
#[derive(Debug)]
enum HookFailure {
    Timeout,
    Error(String),
}

pub struct HookCtx<'a> {
    pub session_id: Uuid,
    pub working_dir: &'a Path,
    pub timeout: Duration,
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &'static str;
    fn supported(&self) -> HookPoints;
    async fn handle(&self, event: HookEvent<'_>, ctx: &HookCtx<'_>) -> Result<HookAction, AgentError>;
}

pub struct HookRegistry {
    handlers: Vec<Arc<dyn Hook>>,
    timeout: Duration,
}

impl HookRegistry {
    pub fn new(timeout: Duration) -> Self {
        Self { handlers: Vec::new(), timeout }
    }

    pub fn empty() -> Self {
        Self::new(Duration::from_secs(5))
    }

    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.handlers.push(hook);
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn interested(&self, point: HookPoints) -> Vec<Arc<dyn Hook>> {
        self.handlers
            .iter()
            .filter(|h| h.supported().contains(point))
            .cloned()
            .collect()
    }

    async fn call_with_timeout(
        &self,
        hook: &Arc<dyn Hook>,
        event: HookEvent<'_>,
        ctx: &HookCtx<'_>,
    ) -> Result<HookAction, HookFailure> {
        match tokio::time::timeout(self.timeout, hook.handle(event, ctx)).await {
            Ok(Ok(a)) => Ok(a),
            Ok(Err(e)) => Err(HookFailure::Error(e.to_string())),
            Err(_) => Err(HookFailure::Timeout),
        }
    }

    /// Run all hooks interested in `event` in waterfall order.
    ///
    /// Emits a `HookFired` for every outcome (including `Block`) via `emit`.
    /// The caller does not need to emit anything extra.
    ///
    /// Waterfall semantics:
    /// - `Block` → bail immediately, return `HookResult::Block`.
    /// - `InjectMessages` → accumulate across all hooks.
    /// - `ReplaceResult` → last write wins.
    /// - Timeout / Error → emitted as telemetry, hook is skipped.
    pub async fn run(
        &self,
        event: HookEvent<'_>,
        working_dir: &Path,
        emit: &mut (impl FnMut(&str, &str, &str, Option<String>) + Send),
    ) -> HookResult {
        let hooks = self.interested(event.point());
        if hooks.is_empty() {
            return HookResult::Continue;
        }

        let ctx = HookCtx {
            session_id: event.session_id(),
            working_dir,
            timeout: self.timeout,
        };

        let mut inject_acc: Vec<ChatMessage> = Vec::new();
        let mut replace_last: Option<(String, String, bool)> = None; // (hook_id, content, is_error)

        for hook in hooks {
            let kind = event.kind();
            let outcome = self.call_with_timeout(&hook, event, &ctx).await;
            match &outcome {
                Ok(HookAction::Block { reason }) => {
                    emit(hook.id(), kind, "block", Some(reason.clone()));
                    return HookResult::Block { hook_id: hook.id().into(), reason: reason.clone() };
                }
                Ok(HookAction::InjectMessages { messages }) => {
                    emit(hook.id(), kind, "inject_messages", Some(format!("{} messages", messages.len())));
                    inject_acc.extend(messages.clone());
                }
                Ok(HookAction::ReplaceResult { content, is_error }) => {
                    emit(hook.id(), kind, "replace_result", Some(format!("{} bytes, is_error={}", content.len(), is_error)));
                    replace_last = Some((hook.id().to_string(), content.clone(), *is_error));
                }
                Ok(HookAction::NoOp) => emit(hook.id(), kind, "noop", None),
                Err(HookFailure::Timeout) => emit(hook.id(), kind, "timeout", None),
                Err(HookFailure::Error(detail)) => emit(hook.id(), kind, "error", Some(detail.clone())),
            }
        }

        if let Some((hook_id, content, is_error)) = replace_last {
            return HookResult::Replace { hook_id, content, is_error };
        }
        if !inject_acc.is_empty() {
            return HookResult::Inject { messages: inject_acc };
        }
        HookResult::Continue
    }
}
