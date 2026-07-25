use parrot_core::hooks::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn empty_registry_returns_defaults_without_emit() {
    let reg = HookRegistry::new(Duration::from_secs(5));
    let sid = Uuid::new_v4();
    let tid = Uuid::new_v4();
    let mid = Uuid::new_v4();
    let args = serde_json::Value::Null;
    let out = parrot_protocol::types::ToolOutput {
        content: "x".into(),
        is_error: false,
    };

    assert_eq!(
        reg.on_turn_start(sid, tid, "hi").await,
        TurnStartDecision::Continue {
            injected_messages: vec![]
        }
    );
    assert_eq!(
        reg.on_tool_call(sid, tid, mid, "tc1", "echo", &args).await,
        ToolCallDecision::Continue
    );
    assert_eq!(
        reg.on_tool_result(sid, tid, "tc1", "echo", &args, &out)
            .await,
        ToolResultDecision::Continue
    );
    // fire-and-forget should just return without panic
    reg.on_agent_start(sid, "claude-x", "anthropic").await;
    reg.on_agent_end(sid).await;
    reg.on_tool_execution_start(sid, tid, "tc1", "echo", &args)
        .await;
}

struct RecordingHook {
    id: &'static str,
    points: HookPoints,
    outcomes: Mutex<Vec<HookResult>>,
    calls: Mutex<Vec<&'static str>>,
    order_log: Option<Arc<Mutex<Vec<String>>>>,
}

impl RecordingHook {
    fn new(id: &'static str, points: HookPoints, outcomes: Vec<HookResult>) -> Self {
        Self {
            id,
            points,
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(Vec::new()),
            order_log: None,
        }
    }
    fn with_order_log(
        id: &'static str,
        points: HookPoints,
        outcomes: Vec<HookResult>,
        order_log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            id,
            points,
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(Vec::new()),
            order_log: Some(order_log),
        }
    }
    fn pop(&self) -> HookResult {
        self.outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(HookResult::NoOp)
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Hook for RecordingHook {
    fn id(&self) -> &'static str {
        self.id
    }
    fn supported(&self) -> HookPoints {
        self.points
    }
    async fn dispatch(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookResult, parrot_core::AgentError> {
        let name = match ev {
            HookEvent::AgentStart { .. } => "agent_start",
            HookEvent::AgentEnd { .. } => "agent_end",
            HookEvent::TurnStart { .. } => "turn_start",
            HookEvent::ToolCall { .. } => "tool_call",
            HookEvent::ToolExecutionStart { .. } => "tool_execution_start",
            HookEvent::ToolResult { .. } => "tool_result",
        };
        self.calls.lock().unwrap().push(name);
        if let Some(log) = &self.order_log {
            log.lock().unwrap().push(self.id.to_string());
        }
        Ok(self.pop())
    }
}

#[tokio::test]
async fn tool_call_bail_on_first_block() {
    let order_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::with_order_log(
        "a",
        HookPoints::TOOL_CALL,
        vec![HookResult::Block {
            reason: "nope".into(),
        }],
        Arc::clone(&order_log),
    )));
    let b = Arc::new(RecordingHook::with_order_log(
        "b",
        HookPoints::TOOL_CALL,
        vec![HookResult::NoOp],
        Arc::clone(&order_log),
    ));
    let b_weak = Arc::clone(&b);
    reg.register(b);
    let out = reg
        .on_tool_call(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "tc",
            "echo",
            &serde_json::Value::Null,
        )
        .await;
    assert_eq!(
        out,
        ToolCallDecision::Blocked {
            hook_id: "a".into(),
            reason: "nope".into()
        }
    );
    // b never ran: bail short-circuited before reaching it.
    assert!(b_weak.calls().is_empty());
    assert_eq!(order_log.lock().unwrap().as_slice(), ["a"]);
}

#[tokio::test]
async fn tool_result_waterfall_last_wins() {
    let order_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    let h1 = Arc::new(RecordingHook::with_order_log(
        "a",
        HookPoints::TOOL_RESULT,
        vec![HookResult::ReplaceResult {
            content: "first".into(),
            is_error: false,
        }],
        Arc::clone(&order_log),
    ));
    let h2 = Arc::new(RecordingHook::with_order_log(
        "b",
        HookPoints::TOOL_RESULT,
        vec![HookResult::ReplaceResult {
            content: "second".into(),
            is_error: true,
        }],
        Arc::clone(&order_log),
    ));
    let h1_weak = Arc::clone(&h1);
    let h2_weak = Arc::clone(&h2);
    reg.register(h1);
    reg.register(h2);
    let out = reg
        .on_tool_result(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "tc",
            "echo",
            &serde_json::Value::Null,
            &parrot_protocol::types::ToolOutput {
                content: "orig".into(),
                is_error: false,
            },
        )
        .await;
    assert_eq!(
        out,
        ToolResultDecision::Replace {
            hook_id: "b".into(),
            content: "second".into(),
            is_error: true
        }
    );
    // h1 must have been called before h2 (ordering trace)
    assert_eq!(h1_weak.calls(), vec!["tool_result"]);
    assert_eq!(h2_weak.calls(), vec!["tool_result"]);
    assert_eq!(order_log.lock().unwrap().as_slice(), ["a", "b"]);
}

#[tokio::test]
async fn turn_start_inject_messages_accumulate() {
    use parrot_core::types::{ChatMessage, ChatRole};
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "a",
        HookPoints::TURN_START,
        vec![HookResult::InjectMessages {
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "ctx1".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    reg.register(Arc::new(RecordingHook::new(
        "b",
        HookPoints::TURN_START,
        vec![HookResult::InjectMessages {
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "ctx2".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    let out = reg
        .on_turn_start(Uuid::new_v4(), Uuid::new_v4(), "hi")
        .await;
    match out {
        TurnStartDecision::Continue { injected_messages } => assert_eq!(injected_messages.len(), 2),
        _ => panic!("expected Continue"),
    }
}

#[tokio::test]
async fn timeout_and_error_are_fail_open_noop() {
    struct Slow;
    #[async_trait::async_trait]
    impl Hook for Slow {
        fn id(&self) -> &'static str {
            "slow"
        }
        fn supported(&self) -> HookPoints {
            HookPoints::TOOL_CALL
        }
        async fn dispatch(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookResult, parrot_core::AgentError> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(HookResult::NoOp)
        }
    }
    struct Boom;
    #[async_trait::async_trait]
    impl Hook for Boom {
        fn id(&self) -> &'static str {
            "boom"
        }
        fn supported(&self) -> HookPoints {
            HookPoints::TURN_START
        }
        async fn dispatch(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookResult, parrot_core::AgentError> {
            Err(parrot_core::AgentError::ToolExecution {
                tool: "x".into(),
                message: "boom".into(),
            })
        }
    }
    let mut reg = HookRegistry::new(Duration::from_millis(50));
    reg.register(Arc::new(Slow));
    reg.register(Arc::new(Boom));
    let out = reg
        .on_tool_call(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "tc",
            "x",
            &serde_json::Value::Null,
        )
        .await;
    assert_eq!(out, ToolCallDecision::Continue);
    let out = reg
        .on_turn_start(Uuid::new_v4(), Uuid::new_v4(), "hi")
        .await;
    match out {
        TurnStartDecision::Continue { injected_messages } => assert!(injected_messages.is_empty()),
        _ => panic!(),
    }
}
