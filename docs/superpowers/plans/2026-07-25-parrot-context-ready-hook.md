# Context-Ready Hook Point Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a 7th hook point `context_ready` (fired once per turn after first prune, before first LLM call), a new `HookAction::ReplaceContext` variant (last-write-wins), and a new `HookResult::ReplaceContext` variant.

**Architecture:** All changes are in `parrot-core` — no new crate, no new dependencies. `HookEvent` gains `ContextReady { session_id, turn_id, context: &[ChatMessage] }`. `HookPoints` gains `CONTEXT_READY = 1 << 6`. `HookRegistry::run` gains `replace_ctx_last` accumulator. `ReActEngine::handle_turn` gains a hook run site between `context.push(user_msg)` and the `for` loop. Priority: `Block >> ReplaceResult >> ReplaceContext >> Inject >> Continue` — concurrent `Inject` in the same `run()` is silently dropped when `ReplaceContext` wins (consistent with existing `ReplaceResult >> Inject`).

**Tech Stack:** Rust 2021, bitflags, serde, async-trait

## Global Constraints

- Same as Plan #1: `thiserror` for crate-boundary errors, zero IO in `parrot-core`, tagged WS enums.
- Existing 6 hook points, 4 HookActions, 4 HookResults keep their semantics — this is append-only.
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
- Branch: `feat/lifecycle-hooks`
- **Independent of:** Plan #1 and #2 (only touches `parrot-core`). Can be executed in parallel with #1/#2 if the implementer is aware that `RecordingHook` in `hooks_test.rs` will need a new match arm added by this plan.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/parrot-core/src/hooks.rs` | Modify | New `HookEvent::ContextReady`, `HookPoints::CONTEXT_READY`, `HookAction::ReplaceContext`, `HookResult::ReplaceContext`, `run()` accumulator |
| `crates/parrot-core/src/engine.rs` | Modify | `handle_turn` hook site between user_msg push and for-loop |
| `crates/parrot-core/tests/hooks_test.rs` | Modify | New tests + `RecordingHook` match arm |
| `crates/parrot-protocol/tests/roundtrip.rs` | Modify | Roundtrip test for `HookAction::ReplaceContext` |

---

### Task 1: Add `HookPoints::CONTEXT_READY` + `HookEvent::ContextReady`

**Files:**
- Modify: `crates/parrot-core/src/hooks.rs:10-19` (bitflags) + `:22-61` (enum) + `:63-96` (impls)

**Interfaces:**
- Produces: `HookPoints::CONTEXT_READY` (bit 6)
- Produces: `HookEvent::ContextReady { session_id, turn_id, context: &'a [ChatMessage] }`

- [ ] **Step 1: Add bitflag**

In `crates/parrot-core/src/hooks.rs`, update the bitflags from `u8` to `u8` (still fits in 8 bits — bit 6 is the 7th flag):

```rust
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
    pub struct HookPoints: u8 {
        const AGENT_START            = 0b0000_0001;
        const AGENT_END              = 0b0000_0010;
        const TURN_START             = 0b0000_0100;
        const TOOL_CALL              = 0b0000_1000;
        const TOOL_EXECUTION_START   = 0b0001_0000;
        const TOOL_RESULT            = 0b0010_0000;
        const CONTEXT_READY          = 0b0100_0000;
    }
}
```

- [ ] **Step 2: Add `ContextReady` variant to `HookEvent`**

After the `ToolResult` variant (line 60), add:

```rust
    ToolResult {
        session_id: Uuid,
        turn_id: Uuid,
        tool_call_id: &'a str,
        tool_name: &'a str,
        input: &'a serde_json::Value,
        result: &'a ToolOutput,
    },
    ContextReady {
        session_id: Uuid,
        turn_id: Uuid,
        context: &'a [ChatMessage],
    },
```

- [ ] **Step 3: Update `kind()` method**

Add to the match in `HookEvent::kind()`:

```rust
            HookEvent::ContextReady { .. } => "context_ready",
```

- [ ] **Step 4: Update `point()` method**

Add to the match in `HookEvent::point()`:

```rust
            HookEvent::ContextReady { .. } => HookPoints::CONTEXT_READY,
```

- [ ] **Step 5: Update `session_id()` method**

Add to the match in `HookEvent::session_id()`:

```rust
            | HookEvent::ContextReady { session_id, .. }
```

(Append to the existing `|` chain that matches all variants extracting `*session_id`.)

- [ ] **Step 6: Verify build**

Run: `cargo build -p parrot-core`
Expected: FAIL — `RecordingHook` in `hooks_test.rs` has a non-exhaustive match on `HookEvent`. This is expected; fix in Step 7.

- [ ] **Step 7: Fix `RecordingHook` match in tests**

In `crates/parrot-core/tests/hooks_test.rs`, add `ContextReady` to the match in `RecordingHook::handle` (around line 175-182):

```rust
        let name = match ev {
            HookEvent::AgentStart { .. } => "agent_start",
            HookEvent::AgentEnd { .. } => "agent_end",
            HookEvent::TurnStart { .. } => "turn_start",
            HookEvent::ToolCall { .. } => "tool_call",
            HookEvent::ToolExecutionStart { .. } => "tool_execution_start",
            HookEvent::ToolResult { .. } => "tool_result",
            HookEvent::ContextReady { .. } => "context_ready",
        };
```

- [ ] **Step 8: Verify build + existing tests**

Run: `cargo build -p parrot-core && cargo test -p parrot-core`
Expected: PASS (all existing tests still pass — no behavior change, just new variant)

- [ ] **Step 9: Commit**

```bash
git add crates/parrot-core/src/hooks.rs crates/parrot-core/tests/hooks_test.rs
git commit -m "feat: add HookEvent::ContextReady + HookPoints::CONTEXT_READY"
```

---

### Task 2: Add `HookAction::ReplaceContext` + `HookResult::ReplaceContext`

**Files:**
- Modify: `crates/parrot-core/src/hooks.rs:98-106` (HookAction) + `:108-124` (HookResult) + `:226-276` (run method)

**Interfaces:**
- Produces: `HookAction::ReplaceContext { messages: Vec<ChatMessage> }`
- Produces: `HookResult::ReplaceContext { hook_id: String, messages: Vec<ChatMessage> }`

- [ ] **Step 1: Add `ReplaceContext` to `HookAction`**

After `ReplaceResult` in the `HookAction` enum:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookAction {
    NoOp,
    InjectMessages { messages: Vec<ChatMessage> },
    Block { reason: String },
    ReplaceResult { content: String, is_error: bool },
    ReplaceContext { messages: Vec<ChatMessage> },
}
```

- [ ] **Step 2: Add `ReplaceContext` to `HookResult`**

After `Replace` in the `HookResult` enum:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum HookResult {
    Continue,
    Block { hook_id: String, reason: String },
    Inject { messages: Vec<ChatMessage> },
    Replace { hook_id: String, content: String, is_error: bool },
    ReplaceContext { hook_id: String, messages: Vec<ChatMessage> },
}
```

- [ ] **Step 3: Update `HookRegistry::run` — add accumulator + match arm + return**

In the `run` method, add a new accumulator variable after `replace_last` (line 224):

```rust
        let mut replace_last: Option<(String, String, bool)> = None; // (hook_id, content, is_error)
        let mut replace_ctx_last: Option<(String, Vec<ChatMessage>)> = None; // (hook_id, messages)
```

In the `for hook in hooks` loop's match, add after the `ReplaceResult` arm (after line 254):

```rust
                Ok(HookAction::ReplaceContext { messages }) => {
                    emit(
                        hook.id(),
                        kind,
                        "replace_context",
                        Some(format!("{} messages", messages.len())),
                    );
                    replace_ctx_last = Some((hook.id().to_string(), messages.clone()));
                }
```

After the loop, add the return for `replace_ctx_last` — between `replace_last` and `inject_acc`:

```rust
        if let Some((hook_id, content, is_error)) = replace_last {
            return HookResult::Replace {
                hook_id,
                content,
                is_error,
            };
        }
        if let Some((hook_id, messages)) = replace_ctx_last {
            return HookResult::ReplaceContext { hook_id, messages };
        }
        if !inject_acc.is_empty() {
            return HookResult::Inject {
                messages: inject_acc,
            };
        }
        HookResult::Continue
```

- [ ] **Step 4: Verify build + existing tests**

Run: `cargo build -p parrot-core && cargo test -p parrot-core`
Expected: PASS (new variant but no behavior change for existing tests)

- [ ] **Step 5: Commit**

```bash
git add crates/parrot-core/src/hooks.rs
git commit -m "feat: add HookAction::ReplaceContext + HookResult::ReplaceContext"
```

---

### Task 3: Unit Tests for `context_ready` Hook Aggregation

**Files:**
- Modify: `crates/parrot-core/tests/hooks_test.rs`

- [ ] **Step 1: Write tests**

Add to `crates/parrot-core/tests/hooks_test.rs`:

```rust
use parrot_core::types::{ChatMessage, ChatRole};

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
            // Inject "a" is silently dropped (ReplaceContext >> Inject)
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p parrot-core -- context_ready`
Expected: All 5 PASS

- [ ] **Step 3: Commit**

```bash
git add crates/parrot-core/tests/hooks_test.rs
git commit -m "test: context_ready hook aggregation (inject/replace/block/noop)"
```

---

### Task 4: Serialization Roundtrip for `ReplaceContext`

**Files:**
- Modify: `crates/parrot-protocol/tests/roundtrip.rs`

- [ ] **Step 1: Write the test**

Add to `crates/parrot-protocol/tests/roundtrip.rs`:

```rust
#[test]
fn hook_action_replace_context_roundtrip() {
    use parrot_core::hooks::HookAction;
    use parrot_core::types::{ChatMessage, ChatRole};

    let action = HookAction::ReplaceContext {
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: "system prompt".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
        ],
    };
    let json = serde_json::to_string(&action).unwrap();
    assert!(
        json.contains(r#""kind":"replace_context""#),
        "expected replace_context tag in: {json}"
    );
    let decoded: HookAction = serde_json::from_str(&json).unwrap();
    assert_eq!(action, decoded);
}
```

> **Note:** This test lives in `parrot-protocol` tests because that's where roundtrip tests live, but it tests `parrot_core::hooks::HookAction`. The root `Cargo.toml` already has `parrot-core` as a dev-dependency, so this import works.

- [ ] **Step 2: Run test**

Run: `cargo test --test roundtrip -- hook_action_replace_context_roundtrip` (if the roundtrip test binary name is `roundtrip`)

Or: `cargo test -p parrot-protocol -- hook_action_replace_context_roundtrip`

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/parrot-protocol/tests/roundtrip.rs
git commit -m "test: HookAction::ReplaceContext serde roundtrip"
```

---

### Task 5: Engine Integration — `context_ready` Hook Site in `handle_turn`

**Files:**
- Modify: `crates/parrot-core/src/engine.rs:306-360` (`handle_turn` method)

**Interfaces:**
- Consumes: `HookEvent::ContextReady`, `HookResult::ReplaceContext` from earlier tasks
- Produces: engine fires `context_ready` once per turn, applies `ReplaceContext` / `Inject` / `Block` before the ReAct loop

- [ ] **Step 1: Write the failing test**

Add to `crates/parrot-core/tests/hooks_test.rs`:

```rust
use parrot_core::types::{ChatMessage, ChatRole};

#[tokio::test]
async fn context_ready_fires_once_per_turn() {
    // This is a hook-level test, not an engine-level test.
    // We verify that CONTEXT_READY is a distinct point from TURN_START
    // and that a hook registered for CONTEXT_READY only fires on
    // ContextReady events, not TurnStart events.
    let mut reg = HookRegistry::new(Duration::from_secs(5));
    let hook = Arc::new(RecordingHook::new(
        "ctx_hook",
        HookPoints::CONTEXT_READY,
        vec![HookAction::NoOp],
    ));
    let hook_weak = Arc::clone(&hook);
    reg.register(hook);
    let sink: Arc<Mutex<Sink>> = Arc::new(Mutex::new(Vec::new()));
    let mut emit = make_emit(Arc::clone(&sink));
    let sid = Uuid::new_v4();
    let tid = Uuid::new_v4();

    // TurnStart should NOT trigger the context_ready hook
    reg.run(
        HookEvent::TurnStart {
            session_id: sid,
            turn_id: tid,
            user_message: "hi",
        },
        path(),
        &mut emit,
    )
    .await;
    assert!(
        hook_weak.calls().is_empty(),
        "CONTEXT_READY hook should not fire on TurnStart"
    );

    // ContextReady SHOULD trigger it
    let ctx = vec![ChatMessage {
        role: ChatRole::User,
        content: "hi".into(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: None,
    }];
    reg.run(
        context_ready_event(sid, tid, &ctx),
        path(),
        &mut emit,
    )
    .await;
    assert_eq!(
        hook_weak.calls(),
        vec!["context_ready"],
        "CONTEXT_READY hook should fire on ContextReady event"
    );
}
```

- [ ] **Step 2: Run test to verify it passes (hook-level isolation)**

Run: `cargo test -p parrot-core -- context_ready_fires_once_per_turn`
Expected: PASS (hook registry already correctly filters by point)

- [ ] **Step 3: Add `context_ready` hook site to `handle_turn`**

In `crates/parrot-core/src/engine.rs`, modify `handle_turn`. The current code (lines 316-327) is:

```rust
        context.push(ChatMessage {
            role: ChatRole::User,
            content: user_msg.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let mut turn_usage = Usage::default();
        let tool_defs = self.tool_registry.list_definitions().await;

        for _ in 0..MAX_REACT_ITERATIONS {
            context_manager.prune(context);
```

Insert the hook site between `tool_defs` and `for`:

```rust
        context.push(ChatMessage {
            role: ChatRole::User,
            content: user_msg.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });

        let mut turn_usage = Usage::default();
        let tool_defs = self.tool_registry.list_definitions().await;

        // context_ready hook: fired once per turn, after user message is
        // pushed and before the first LLM call. Hooks can inject messages,
        // replace the entire context, or block the turn.
        {
            context_manager.prune(context);
            let session_id = self.session_id;
            let mut emit = make_emit(session_id, event_tx);
            let outcome = self
                .hooks
                .run(
                    HookEvent::ContextReady {
                        session_id,
                        turn_id,
                        context,
                    },
                    &self.working_dir,
                    &mut emit,
                )
                .await;
            match outcome {
                HookResult::Block { reason, .. } => {
                    let turn_end = AgentEvent::TurnEnd {
                        session_id,
                        turn_id,
                        stop_reason: TurnStopReason::BlockedHook(reason),
                        usage: Usage::default(),
                    };
                    let _ = event_tx.send(turn_end.clone()).await.ok();
                    let _ = event_log.append(turn_end);
                    return Ok((TurnStopReason::BlockedHook(reason), Usage::default()));
                }
                HookResult::ReplaceContext { messages, .. } => {
                    *context = messages;
                    context_manager.prune(context);
                }
                HookResult::Inject { messages } => {
                    context.extend(messages);
                    context_manager.prune(context);
                }
                HookResult::Continue | HookResult::Replace { .. } => {}
            }
        }

        for _ in 0..MAX_REACT_ITERATIONS {
            context_manager.prune(context);
```

- [ ] **Step 4: Verify build**

Run: `cargo build -p parrot-core`
Expected: PASS

- [ ] **Step 5: Verify all tests pass**

Run: `cargo test --workspace`
Expected: PASS (no existing tests should break — `context_ready` only fires when hooks are registered for that point, and no existing hooks register for it)

- [ ] **Step 6: Commit**

```bash
git add crates/parrot-core/src/engine.rs crates/parrot-core/tests/hooks_test.rs
git commit -m "feat: context_ready hook site in handle_turn (once per turn, pre-LLM)"
```

---

### Task 6: Final Verification

- [ ] **Step 1: Full workspace check**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: All PASS

- [ ] **Step 2: Commit if any fmt/clippy fixes needed**

```bash
git add -A
git commit -m "fix: fmt/clippy after context_ready hook"
```
(Only if there are changes; skip if clean.)

---

### Task 7: Update Design Doc

**Files:**
- Modify: `docs/superpowers/specs/2026-07-25-parrot-lifecycle-hooks-design.md`

- [ ] **Step 1: Update "6 hook points" → "7 hook points"**

Search the design doc for references to "6 hook points" or the hook point list. Add `context_ready` as the 7th point. Add a brief description:

- `context_ready` — fired once per turn after `turn_start`, after the user message is pushed and context is pruned, before the first `stream_llm_message` call. Supports `InjectMessages`, `ReplaceContext`, and `Block`. Waterfall priority: `Block >> ReplaceContext >> Inject >> Continue`.

- [ ] **Step 2: Commit**

```bash
git add docs/
git commit -m "docs: update lifecycle-hooks design for context_ready (7th hook point)"
```
