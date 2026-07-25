use parrot_core::hooks::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

/// Records every `HookFired` emission pushed through the registry's emit
/// callback as `(hook_id, event_kind, result_kind, summary)`.
type Sink = Vec<(String, String, String, Option<String>)>;

/// Build a `Send` `FnMut` emit closure that pushes into an `Arc<Mutex<Sink>>`.
/// The shared-cell pattern lets the test read the sink between dispatches
/// (assertions don't conflict with the closure's mutable borrow scope).
fn make_emit(sink: Arc<Mutex<Sink>>) -> impl FnMut(&str, &str, &str, Option<String>) + Send {
    move |hook_id: &str, event_kind: &str, result_kind: &str, summary: Option<String>| {
        sink.lock().unwrap().push((
            hook_id.to_string(),
            event_kind.to_string(),
            result_kind.to_string(),
            summary,
        ));
    }
}

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
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));

    assert_eq!(
        reg.on_turn_start(&mut emit, sid, tid, "hi").await,
        TurnStartDecision::Continue {
            injected_messages: vec![]
        }
    );
    assert_eq!(
        reg.on_tool_call(&mut emit, sid, tid, mid, "tc1", "echo", &args)
            .await,
        ToolCallDecision::Continue
    );
    assert_eq!(
        reg.on_tool_result(&mut emit, sid, tid, "tc1", "echo", &args, &out)
            .await,
        ToolResultDecision::Continue
    );
    // fire-and-forget should just return without panic
    reg.on_agent_start(&mut emit, sid, "claude-x", "anthropic")
        .await;
    reg.on_agent_end(&mut emit, sid).await;
    reg.on_tool_execution_start(&mut emit, sid, tid, "tc1", "echo", &args)
        .await;

    // Empty-registry fast path never invokes the emit callback.
    assert!(
        sink.lock().unwrap().is_empty(),
        "empty registry must not emit HookFired"
    );
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
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let out = reg
        .on_tool_call(
            &mut emit,
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
    // Helper does NOT emit on Block (engine-side emission only). Sink stays
    // empty because the only handler ran was a Block.
    assert!(
        sink.lock().unwrap().is_empty(),
        "on_tool_call helper must not emit for Block outcomes"
    );
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
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let out = reg
        .on_tool_result(
            &mut emit,
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
    // Each Replace-result dispatch fires one replace_result HookFired from
    // inside the helper.
    let sink = sink.lock().unwrap();
    assert_eq!(sink.len(), 2);
    assert_eq!(sink[0].0, "a");
    assert_eq!(sink[0].1, "tool_result");
    assert_eq!(sink[0].2, "replace_result");
    assert_eq!(sink[1].0, "b");
    assert_eq!(sink[1].2, "replace_result");
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
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let out = reg
        .on_turn_start(&mut emit, Uuid::new_v4(), Uuid::new_v4(), "hi")
        .await;
    match out {
        TurnStartDecision::Continue { injected_messages } => assert_eq!(injected_messages.len(), 2),
        _ => panic!("expected Continue"),
    }
    // Each InjectMessages dispatch fires one inject_messages HookFired.
    let sink = sink.lock().unwrap();
    assert_eq!(sink.len(), 2);
    assert_eq!(sink[0].0, "a");
    assert_eq!(sink[0].1, "turn_start");
    assert_eq!(sink[0].2, "inject_messages");
    assert_eq!(sink[1].0, "b");
    assert_eq!(sink[1].2, "inject_messages");
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
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let out = reg
        .on_tool_call(
            &mut emit,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "tc",
            "x",
            &serde_json::Value::Null,
        )
        .await;
    assert_eq!(out, ToolCallDecision::Continue);
    // Slow hook timed out: a single timeout emit, no summary.
    {
        let sink = sink.lock().unwrap();
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].0, "slow");
        assert_eq!(sink[0].1, "tool_call");
        assert_eq!(sink[0].2, "timeout");
        assert!(sink[0].3.is_none(), "timeout summary must be None");
    }

    let out = reg
        .on_turn_start(&mut emit, Uuid::new_v4(), Uuid::new_v4(), "hi")
        .await;
    match out {
        TurnStartDecision::Continue { injected_messages } => assert!(injected_messages.is_empty()),
        _ => panic!(),
    }
    // Boom hook errored: a single error emit, summary carries AgentError text.
    {
        let sink = sink.lock().unwrap();
        assert_eq!(sink.len(), 2);
        assert_eq!(sink[1].0, "boom");
        assert_eq!(sink[1].1, "turn_start");
        assert_eq!(sink[1].2, "error");
        let summary = sink[1].3.as_ref().expect("error summary must be Some");
        assert!(
            summary.contains("boom"),
            "error summary must preserve AgentError detail, got: {summary}"
        );
    }
}

#[tokio::test]
async fn fire_and_forget_emits_noop_on_completed() {
    struct NoopHook;
    #[async_trait::async_trait]
    impl Hook for NoopHook {
        fn id(&self) -> &'static str {
            "noop-1"
        }
        fn supported(&self) -> HookPoints {
            HookPoints::AGENT_START
        }
        async fn dispatch(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookResult, parrot_core::AgentError> {
            Ok(HookResult::NoOp)
        }
    }
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(NoopHook));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    reg.on_agent_start(&mut emit, Uuid::new_v4(), "m", "p")
        .await;
    let sink = sink.lock().unwrap();
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].0, "noop-1");
    assert_eq!(sink[0].1, "agent_start");
    assert_eq!(sink[0].2, "noop");
    assert!(sink[0].3.is_none());
}
