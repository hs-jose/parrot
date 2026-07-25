use parrot_core::hooks::*;
use parrot_core::types::{ChatMessage, ChatRole};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

/// Records every `HookFired` emission pushed through the registry's emit
/// callback as `(hook_id, event_kind, result_kind, summary)`.
type Sink = Vec<(String, String, String, Option<String>)>;

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

fn path() -> &'static Path {
    Path::new(".")
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
        reg.run(
            HookEvent::TurnStart {
                session_id: sid,
                turn_id: tid,
                user_message: "hi"
            },
            path(),
            &mut emit
        )
        .await,
        HookResult::Continue
    );
    assert_eq!(
        reg.run(
            HookEvent::ToolCall {
                session_id: sid,
                turn_id: tid,
                parent_message_id: mid,
                tool_call_id: "tc1",
                tool_name: "echo",
                arguments: &args
            },
            path(),
            &mut emit
        )
        .await,
        HookResult::Continue
    );
    assert_eq!(
        reg.run(
            HookEvent::ToolResult {
                session_id: sid,
                turn_id: tid,
                tool_call_id: "tc1",
                tool_name: "echo",
                input: &args,
                result: &out
            },
            path(),
            &mut emit
        )
        .await,
        HookResult::Continue
    );
    reg.run(
        HookEvent::AgentStart {
            session_id: sid,
            model: "claude-x",
            provider: "anthropic",
        },
        path(),
        &mut emit,
    )
    .await;
    reg.run(HookEvent::AgentEnd { session_id: sid }, path(), &mut emit)
        .await;
    reg.run(
        HookEvent::ToolExecutionStart {
            session_id: sid,
            turn_id: tid,
            tool_call_id: "tc1",
            tool_name: "echo",
            arguments: &args,
        },
        path(),
        &mut emit,
    )
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
    outcomes: Mutex<Vec<HookAction>>,
    calls: Mutex<Vec<&'static str>>,
    order_log: Option<Arc<Mutex<Vec<String>>>>,
}

impl RecordingHook {
    fn new(id: &'static str, points: HookPoints, outcomes: Vec<HookAction>) -> Self {
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
        outcomes: Vec<HookAction>,
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
    fn pop(&self) -> HookAction {
        self.outcomes
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(HookAction::NoOp)
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
    async fn handle(
        &self,
        ev: HookEvent<'_>,
        _ctx: &HookCtx<'_>,
    ) -> Result<HookAction, parrot_core::AgentError> {
        let name = match ev {
            HookEvent::AgentStart { .. } => "agent_start",
            HookEvent::AgentEnd { .. } => "agent_end",
            HookEvent::TurnStart { .. } => "turn_start",
            HookEvent::ToolCall { .. } => "tool_call",
            HookEvent::ToolExecutionStart { .. } => "tool_execution_start",
            HookEvent::ToolResult { .. } => "tool_result",
            HookEvent::ContextReady { .. } => "context_ready",
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
        vec![HookAction::Block {
            reason: "nope".into(),
        }],
        Arc::clone(&order_log),
    )));
    let b = Arc::new(RecordingHook::with_order_log(
        "b",
        HookPoints::TOOL_CALL,
        vec![HookAction::NoOp],
        Arc::clone(&order_log),
    ));
    let b_weak = Arc::clone(&b);
    reg.register(b);
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let out = reg
        .run(
            HookEvent::ToolCall {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                parent_message_id: Uuid::new_v4(),
                tool_call_id: "tc",
                tool_name: "echo",
                arguments: &serde_json::Value::Null,
            },
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(
        out,
        HookResult::Block {
            hook_id: "a".into(),
            reason: "nope".into()
        }
    );
    // b never ran: bail short-circuited before reaching it.
    assert!(b_weak.calls().is_empty());
    assert_eq!(order_log.lock().unwrap().as_slice(), ["a"]);
    // Block is now emitted inside run(), so sink has one entry.
    let sink = sink.lock().unwrap();
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].0, "a");
    assert_eq!(sink[0].1, "tool_call");
    assert_eq!(sink[0].2, "block");
}

#[tokio::test]
async fn tool_result_waterfall_last_wins() {
    let order_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    let h1 = Arc::new(RecordingHook::with_order_log(
        "a",
        HookPoints::TOOL_RESULT,
        vec![HookAction::ReplaceResult {
            content: "first".into(),
            is_error: false,
        }],
        Arc::clone(&order_log),
    ));
    let h2 = Arc::new(RecordingHook::with_order_log(
        "b",
        HookPoints::TOOL_RESULT,
        vec![HookAction::ReplaceResult {
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
        .run(
            HookEvent::ToolResult {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                tool_call_id: "tc",
                tool_name: "echo",
                input: &serde_json::Value::Null,
                result: &parrot_protocol::types::ToolOutput {
                    content: "orig".into(),
                    is_error: false,
                },
            },
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(
        out,
        HookResult::Replace {
            hook_id: "b".into(),
            content: "second".into(),
            is_error: true
        }
    );
    assert_eq!(h1_weak.calls(), vec!["tool_result"]);
    assert_eq!(h2_weak.calls(), vec!["tool_result"]);
    assert_eq!(order_log.lock().unwrap().as_slice(), ["a", "b"]);
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
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "a",
        HookPoints::TURN_START,
        vec![HookAction::InjectMessages {
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
        vec![HookAction::InjectMessages {
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
        .run(
            HookEvent::TurnStart {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                user_message: "hi",
            },
            path(),
            &mut emit,
        )
        .await;
    match out {
        HookResult::Inject { messages } => assert_eq!(messages.len(), 2),
        _ => panic!("expected Inject"),
    }
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
        async fn handle(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookAction, parrot_core::AgentError> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(HookAction::NoOp)
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
        async fn handle(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookAction, parrot_core::AgentError> {
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
        .run(
            HookEvent::ToolCall {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                parent_message_id: Uuid::new_v4(),
                tool_call_id: "tc",
                tool_name: "x",
                arguments: &serde_json::Value::Null,
            },
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(out, HookResult::Continue);
    {
        let sink = sink.lock().unwrap();
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].0, "slow");
        assert_eq!(sink[0].1, "tool_call");
        assert_eq!(sink[0].2, "timeout");
        assert!(sink[0].3.is_none(), "timeout summary must be None");
    }

    let out = reg
        .run(
            HookEvent::TurnStart {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                user_message: "hi",
            },
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(out, HookResult::Continue);
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
        async fn handle(
            &self,
            _: HookEvent<'_>,
            _: &HookCtx<'_>,
        ) -> Result<HookAction, parrot_core::AgentError> {
            Ok(HookAction::NoOp)
        }
    }
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(NoopHook));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    reg.run(
        HookEvent::AgentStart {
            session_id: Uuid::new_v4(),
            model: "m",
            provider: "p",
        },
        path(),
        &mut emit,
    )
    .await;
    let sink = sink.lock().unwrap();
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].0, "noop-1");
    assert_eq!(sink[0].1, "agent_start");
    assert_eq!(sink[0].2, "noop");
    assert!(sink[0].3.is_none());
}

fn context_ready_event<'a>(sid: Uuid, tid: Uuid, context: &'a [ChatMessage]) -> HookEvent<'a> {
    HookEvent::ContextReady {
        session_id: sid,
        turn_id: tid,
        context,
    }
}

#[tokio::test]
async fn context_ready_inject_accumulates() {
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "injector",
        HookPoints::CONTEXT_READY,
        vec![HookAction::InjectMessages {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "summary".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let ctx = vec![ChatMessage {
        role: ChatRole::System,
        content: "sys".into(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: None,
    }];
    let result = reg
        .run(
            context_ready_event(Uuid::new_v4(), Uuid::new_v4(), &ctx),
            path(),
            &mut emit,
        )
        .await;
    match result {
        HookResult::Inject { messages } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content, "summary");
        }
        _ => panic!("expected Inject, got {:?}", result),
    }
}

#[tokio::test]
async fn context_ready_replace_last_wins() {
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "h1",
        HookPoints::CONTEXT_READY,
        vec![HookAction::ReplaceContext {
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "first".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    reg.register(Arc::new(RecordingHook::new(
        "h2",
        HookPoints::CONTEXT_READY,
        vec![HookAction::ReplaceContext {
            messages: vec![ChatMessage {
                role: ChatRole::System,
                content: "second".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let ctx = vec![];
    let result = reg
        .run(
            context_ready_event(Uuid::new_v4(), Uuid::new_v4(), &ctx),
            path(),
            &mut emit,
        )
        .await;
    match result {
        HookResult::ReplaceContext { hook_id, messages } => {
            assert_eq!(hook_id, "h2");
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content, "second");
        }
        _ => panic!("expected ReplaceContext, got {:?}", result),
    }
}

#[tokio::test]
async fn context_ready_replace_drops_concurrent_inject() {
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "injector",
        HookPoints::CONTEXT_READY,
        vec![HookAction::InjectMessages {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "a".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            }],
        }],
    )));
    reg.register(Arc::new(RecordingHook::new(
        "replacer",
        HookPoints::CONTEXT_READY,
        vec![HookAction::ReplaceContext {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "b1".into(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "b2".into(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                },
            ],
        }],
    )));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let ctx = vec![];
    let result = reg
        .run(
            context_ready_event(Uuid::new_v4(), Uuid::new_v4(), &ctx),
            path(),
            &mut emit,
        )
        .await;
    match result {
        HookResult::ReplaceContext { hook_id, messages } => {
            assert_eq!(hook_id, "replacer");
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].content, "b1");
            assert_eq!(messages[1].content, "b2");
        }
        HookResult::Inject { .. } => panic!("ReplaceContext should win over Inject"),
        _ => panic!("expected ReplaceContext, got {:?}", result),
    }
}

#[tokio::test]
async fn context_ready_block_bails() {
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "blocker",
        HookPoints::CONTEXT_READY,
        vec![HookAction::Block {
            reason: "context too large".into(),
        }],
    )));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let ctx = vec![];
    let result = reg
        .run(
            context_ready_event(Uuid::new_v4(), Uuid::new_v4(), &ctx),
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(
        result,
        HookResult::Block {
            hook_id: "blocker".into(),
            reason: "context too large".into()
        }
    );
}

#[tokio::test]
async fn context_ready_noop_returns_continue() {
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    reg.register(Arc::new(RecordingHook::new(
        "noop",
        HookPoints::CONTEXT_READY,
        vec![HookAction::NoOp],
    )));
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let ctx = vec![];
    let result = reg
        .run(
            context_ready_event(Uuid::new_v4(), Uuid::new_v4(), &ctx),
            path(),
            &mut emit,
        )
        .await;
    assert_eq!(result, HookResult::Continue);
}
