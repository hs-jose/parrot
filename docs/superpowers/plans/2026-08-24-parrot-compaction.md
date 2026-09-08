# Parrot Structured Summary Compaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Pi-style structured summary compaction per `docs/superpowers/specs/2026-08-24-parrot-compaction-design.md` — when estimated context tokens exceed budget × threshold at turn start, summarize the pre-cut-point history (including any old summary) via the current model's non-stream `chat()`, keep recent turns verbatim, and persist a `CompactionSummary` event so resume rebuilds the identical compacted context.

**Architecture:** New pure-logic module `crates/parrot-core/src/compaction.rs` (cut-point planning, prompt, conversation serialization). The engine gains `maybe_compact`, called in `run()` after the TurnStart hook block-check and **before** the `TurnStart` event is emitted (so partial-turn truncation on resume never discards a compaction event). `rebuild_context` applies `CompactionSummary` with message-count semantics. Config flows `[session]` → `SessionManager` → `ReActEngine::with_context_limits(ContextLimits)` (also fixing the pre-existing gap where resumed sessions never received context limits). Existing `ContextManager::prune` stays as the second-line fallback.

**Tech Stack:** Rust 2021, tokio, serde, thiserror, toml. No new dependencies.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-24-parrot-compaction-design.md` — read it before starting.
- `parrot-core` stays zero IO: no `reqwest`, no `tokio::fs`, no `std::env`. Compaction is pure in-memory computation + provider trait call.
- WS/event types are `#[serde(tag = "type")]`. New event variant ⇒ update roundtrip tests in `crates/parrot-protocol/tests/roundtrip.rs`.
- Errors crossing crate boundaries use `thiserror` enums; `anyhow` only inside binaries.
- Summary request shape (spec §6): `[System(SUMMARIZATION_PROMPT), User(serialize_conversation(region))]`, no tools, `max_tokens = summary_max_tokens`.
- Summary message content = `"[CONVERSATION SUMMARY]\n" + model output` (marker prefix identifies old summaries later).
- Event field semantics (spec §4, message-count revision): `dropped_message_count` = messages removed from context (incl. old summary); `kept_message_count` = original messages kept (excl. system and the new summary itself).
- `CompactionSummary` event is emitted and persisted **before** the triggering turn's `TurnStart` event.
- Compaction failure (provider error, empty summary, provider unresolved) ⇒ `tracing::warn!` + skip; prune fallback still runs. Never fatal.
- Defaults: `compaction = true`, `compaction_threshold = 0.9`, `keep_recent_tokens = 20000`, `summary_max_tokens = 4096`.
- Branch: `feat/compaction` created from `feat/tui-iteration` (Task 0).
- After each task: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` must pass.
- Do NOT modify `parrot.toml` (has uncommitted local changes with a real API key).

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/parrot-protocol/src/agent_event.rs` | Modify | `CompactionSummary` variant, `is_persistent`, `session_id()` |
| `crates/parrot-protocol/tests/roundtrip.rs` | Modify | Roundtrip + persistence tests |
| `crates/parrot-core/src/compaction.rs` | Create | `CompactionConfig`, `ContextLimits`, `plan_compaction`, `SUMMARIZATION_PROMPT`, `SUMMARY_MARKER`, `serialize_conversation` |
| `crates/parrot-core/src/context.rs` | Modify | `estimate_tokens` → `pub(crate)` |
| `crates/parrot-core/src/lib.rs` | Modify | `pub mod compaction;` |
| `crates/parrot-core/src/event_log.rs` | Modify | `rebuild_context` handles `CompactionSummary` |
| `crates/parrot-core/src/engine.rs` | Modify | `with_context_limits(ContextLimits)`, `maybe_compact`, budget plumbing |
| `crates/parrot-core/src/session.rs` | Modify | `SessionManager` stores `ContextLimits`; resumed sessions get limits too |
| `crates/parrot-core/tests/react_loop.rs` | Modify | MockProvider `chat()` + captured_calls; compaction engine tests; update 2 existing `with_context_limits` call sites |
| `crates/parrot-config/src/config.rs` | Modify | `SessionConfig` 4 new fields + defaults + tests |
| `crates/parrot-daemon/src/runtime.rs` | Modify | Build `ContextLimits` from config |
| `src/tui/app.rs` | Modify | `apply_event` arm renders Info entry |
| `tests/integration/e2e_test.rs` | Modify | `spawn_daemon_with_provider_and_config`, MockProvider `chat()`, e2e compaction test |

---

### Task 0: Branch

- [ ] **Step 1: Create branch**

```bash
git checkout -b feat/compaction feat/tui-iteration
```

Expected: branch created, working tree carries over (untracked files fine).

---

### Task 1: Protocol — `CompactionSummary` event variant

**Files:**
- Modify: `crates/parrot-protocol/src/agent_event.rs`
- Test: `crates/parrot-protocol/tests/roundtrip.rs`
- Modify: `crates/parrot-core/tests/react_loop.rs` (compile-fix `variant_name`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `AgentEvent::CompactionSummary { session_id: SessionId, turn_id: Uuid, summary: String, dropped_message_count: u32, kept_message_count: u32 }` — persistent, `session_id()`-accessible.

- [ ] **Step 1: Write the failing tests**

Append to `crates/parrot-protocol/tests/roundtrip.rs` (inside the existing file, after `server_shell_result_roundtrip`):

```rust
#[test]
fn compaction_summary_roundtrip() {
    let sid = uuid::Uuid::new_v4();
    let msg = AgentEvent::CompactionSummary {
        session_id: sid,
        turn_id: uuid::Uuid::new_v4(),
        summary: "[CONVERSATION SUMMARY]\n## 目标与任务\n...".into(),
        dropped_message_count: 8,
        kept_message_count: 4,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(msg, decoded);
    assert!(msg.is_persistent());
    assert_eq!(msg.session_id(), sid);
    assert_eq!(
        serde_json::to_value(&msg).unwrap()["type"],
        serde_json::json!("CompactionSummary")
    );
}
```

Also add to the `variants` vec inside `agent_event_roundtrip_all_variants` (after the `ReplayIntegrityWarning` entry, around line 210):

```rust
        AgentEvent::CompactionSummary {
            session_id: sid,
            turn_id,
            summary: "[CONVERSATION SUMMARY]\n...".into(),
            dropped_message_count: 3,
            kept_message_count: 5,
        },
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p parrot-protocol --test roundtrip`
Expected: FAIL — compile error "no variant or associated item named `CompactionSummary` found".

- [ ] **Step 3: Add the variant**

In `crates/parrot-protocol/src/agent_event.rs`, add after the `HookFired` variant (end of `AgentEvent` enum, before the closing brace):

```rust
    /// Context compaction applied: pre-cut-point history was replaced by a
    /// structured summary (marker-prefixed). Emitted BEFORE the triggering
    /// turn's `TurnStart`. Replayed by `rebuild_context` with message-count
    /// semantics: keep the last `kept_message_count` rebuilt messages, then
    /// push this summary as a User message.
    CompactionSummary {
        session_id: SessionId,
        turn_id: Uuid,
        summary: String,
        dropped_message_count: u32,
        kept_message_count: u32,
    },
```

Then update the two exhaustive matches in the same file:

In `is_persistent` (no change needed — it's a negative match on MessageDelta/ToolUpdate/HookFired, `CompactionSummary` is persistent by default). Verify by reading.

In `session_id()`, add to the match arm list:

```rust
            | Self::CompactionSummary { session_id, .. } => *session_id,
```

(Pattern-match style: extend the existing `| Self::AgentStart { session_id, .. } ...` chain with this arm.)

- [ ] **Step 4: Fix compile errors in downstream exhaustive matches**

`cargo build --workspace` will fail in `crates/parrot-core/tests/react_loop.rs` (`variant_name` fn, ~line 224). Add:

```rust
        AgentEvent::CompactionSummary { .. } => "CompactionSummary",
```

`src/tui/app.rs` `apply_event` will also fail to compile — that's Task 6; for now add a temporary no-op arm to keep the build green (Task 6 replaces it):

```rust
            AgentEvent::CompactionSummary { .. } => {}
```

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p parrot-protocol`
Expected: PASS (all roundtrip tests including the new one).

Run: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 6: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add crates/parrot-protocol/src/agent_event.rs crates/parrot-protocol/tests/roundtrip.rs crates/parrot-core/tests/react_loop.rs src/tui/app.rs
git commit -m "feat(protocol): CompactionSummary agent event variant"
```

---

### Task 2: `compaction.rs` — pure planning logic

**Files:**
- Create: `crates/parrot-core/src/compaction.rs`
- Modify: `crates/parrot-core/src/context.rs` (make `estimate_tokens` `pub(crate)`)
- Modify: `crates/parrot-core/src/lib.rs` (add `pub mod compaction;`)

**Interfaces:**
- Consumes: `crate::types::{ChatMessage, ChatRole}`; `crate::context::estimate_tokens` (made `pub(crate)`).
- Produces (used by Tasks 4, 5):

```rust
pub const SUMMARY_MARKER: &str = "[CONVERSATION SUMMARY]";
pub const SUMMARIZATION_PROMPT: &str; // full text below
#[derive(Debug, Clone)]
pub struct CompactionConfig { pub enabled: bool, pub threshold: f32, pub keep_recent_tokens: u32, pub summary_max_tokens: u32 } // impl Default
#[derive(Debug, Clone)]
pub struct ContextLimits { pub max_history_tokens: u32, pub keep_recent_turns: u32, pub compaction: CompactionConfig } // impl Default
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan { pub summarize_start: usize, pub cut_index: usize }
pub fn plan_compaction(context: &[ChatMessage], budget_tokens: u32, config: &CompactionConfig) -> Option<CompactionPlan>
pub fn is_summary_message(msg: &ChatMessage) -> bool
pub fn serialize_conversation(messages: &[ChatMessage]) -> String
```

- [ ] **Step 1: Write the failing tests**

Create `crates/parrot-core/src/compaction.rs` with ONLY the test module and struct stubs (implementations in Step 3):

```rust
use crate::context::estimate_tokens;
use crate::types::{ChatMessage, ChatRole};

pub const SUMMARY_MARKER: &str = "[CONVERSATION SUMMARY]";

/// 摘要请求的 system prompt(spec §6,固定内嵌常量)。
pub const SUMMARIZATION_PROMPT: &str = "你是压缩助手。将以下对话历史压缩为结构化摘要,供后续对话参考。\n输出格式(纯文本,不调用任何工具):\n<summary>\n## 目标与任务\n## 关键事实与决定(文件路径、命令、命名、约束)\n## 未完成事项与下一步\n## 关键文件/代码位置\n</summary>";

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub threshold: f32,
    pub keep_recent_tokens: u32,
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.9,
            keep_recent_tokens: 20_000,
            summary_max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextLimits {
    pub max_history_tokens: u32,
    pub keep_recent_turns: u32,
    pub compaction: CompactionConfig,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_history_tokens: 100_000,
            keep_recent_turns: 10,
            compaction: CompactionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPlan {
    /// 待摘要区(含旧摘要,不含 system)起点下标。
    pub summarize_start: usize,
    /// 保留区起点下标(指向某个 User 消息,tool 配对安全)。
    pub cut_index: usize,
}

pub fn is_summary_message(msg: &ChatMessage) -> bool {
    msg.role == ChatRole::User && msg.content.starts_with(SUMMARY_MARKER)
}

pub fn serialize_conversation(messages: &[ChatMessage]) -> String {
    let mut s = String::from("<conversation>\n");
    for m in messages {
        let role = match m.role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Tool => "tool",
        };
        s.push_str(&format!("[{}]\n{}\n\n", role, m.content));
    }
    s.push_str("</conversation>");
    s
}
```

(Then the `plan_compaction` impl and tests below.)

Append to `crates/parrot-core/src/compaction.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        }
    }

    /// [sys, u1, a1, u2, a2, ...] 每条 turn 的 user 消息 chars_each 字符。
    fn context_with_turns(turns: usize, chars_each: usize) -> Vec<ChatMessage> {
        let mut ctx = vec![msg(ChatRole::System, "sys")];
        for i in 0..turns {
            ctx.push(msg(ChatRole::User, &format!("q{}{}", i, "x".repeat(chars_each))));
            ctx.push(msg(ChatRole::Assistant, "done"));
        }
        ctx
    }

    fn default_cfg() -> CompactionConfig {
        CompactionConfig::default()
    }

    #[test]
    fn plan_disabled_returns_none() {
        let ctx = context_with_turns(5, 400);
        let cfg = CompactionConfig {
            enabled: false,
            ..default_cfg()
        };
        assert!(plan_compaction(&ctx, 1, &cfg).is_none());
    }

    #[test]
    fn plan_under_threshold_returns_none() {
        // 5 turns × ~100 tokens = ~500 est tokens, budget 1000 ⇒ 500 < 900.
        let ctx = context_with_turns(5, 400);
        assert!(plan_compaction(&ctx, 1000, &default_cfg()).is_none());
    }

    #[test]
    fn plan_fires_over_threshold() {
        // 3 turns × ~1000 tokens = ~3000 > 900 (budget 1000 × 0.9).
        let ctx = context_with_turns(3, 4000);
        // keep 1000 tokens ⇒ newest turn (~1001 tokens) alone ≥ keep ⇒ kept = 1 turn.
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(plan.summarize_start, 1, "system at 0 excluded");
        assert_eq!(plan.cut_index, ctx.len() - 2, "kept region = last turn (u3,a2… wait u3,a3)");
        assert_eq!(ctx[plan.cut_index].role, ChatRole::User, "cut must land on a User message");
    }

    #[test]
    fn plan_single_turn_returns_none() {
        let ctx = context_with_turns(1, 4000);
        assert!(plan_compaction(&ctx, 1, &default_cfg()).is_none());
    }

    #[test]
    fn plan_walk_fallback_keeps_only_newest_turn() {
        // keep_recent_tokens huge ⇒ 回退累计吞掉所有 turn ⇒ 强制只留最新 1 个 turn。
        let ctx = context_with_turns(3, 4000);
        let cfg = CompactionConfig {
            keep_recent_tokens: 1_000_000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("must make progress");
        assert_eq!(plan.cut_index, ctx.len() - 2, "fallback keeps only newest turn");
        assert!(plan.cut_index > plan.summarize_start, "summarize region non-empty");
    }

    #[test]
    fn plan_includes_old_summary_in_summarize_region() {
        let mut ctx = context_with_turns(3, 4000);
        ctx.insert(
            1,
            msg(ChatRole::User, &format!("{SUMMARY_MARKER}\nold summary")),
        );
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(plan.summarize_start, 1, "old summary at index 1 included in region");
        assert!(is_summary_message(&ctx[plan.summarize_start]));
        assert!(plan.cut_index > 2, "cut is past the old summary");
    }

    #[test]
    fn plan_without_system_prompt_uses_body_start_zero() {
        let mut ctx = context_with_turns(3, 4000);
        ctx.remove(0); // drop system
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(plan.summarize_start, 0);
    }

    #[test]
    fn plan_cut_never_lands_on_tool_message() {
        // turn with tool calls: User, Assistant(tool_calls), Tool, Assistant
        let mut ctx = vec![msg(ChatRole::System, "sys")];
        for i in 0..4 {
            ctx.push(msg(ChatRole::User, &format!("q{}{}", i, "x".repeat(4000))));
            let mut a = msg(ChatRole::Assistant, "");
            a.tool_calls = Some(vec![]);
            ctx.push(a);
            ctx.push(msg(ChatRole::Tool, "result"));
            ctx.push(msg(ChatRole::Assistant, "done"));
        }
        let cfg = CompactionConfig {
            keep_recent_tokens: 1000,
            ..default_cfg()
        };
        let plan = plan_compaction(&ctx, 1000, &cfg).expect("should compact");
        assert_eq!(ctx[plan.cut_index].role, ChatRole::User);
    }

    #[test]
    fn serialize_conversation_renders_roles() {
        let ctx = vec![
            msg(ChatRole::User, "hi"),
            msg(ChatRole::Assistant, "hello"),
        ];
        let s = serialize_conversation(&ctx);
        assert!(s.starts_with("<conversation>"));
        assert!(s.ends_with("</conversation>"));
        assert!(s.contains("[user]\nhi"));
        assert!(s.contains("[assistant]\nhello"));
    }

    #[test]
    fn is_summary_message_requires_marker_and_user_role() {
        assert!(is_summary_message(&msg(
            ChatRole::User,
            "[CONVERSATION SUMMARY]\nabc"
        )));
        assert!(!is_summary_message(&msg(
            ChatRole::Assistant,
            "[CONVERSATION SUMMARY]\nabc"
        )));
        assert!(!is_summary_message(&msg(ChatRole::User, "plain")));
    }
}
```

- [ ] **Step 2: Run to verify failure**

First make `estimate_tokens` visible: in `crates/parrot-core/src/context.rs` change `fn estimate_tokens(msg: &ChatMessage) -> u32` (line 16) to `pub(crate) fn estimate_tokens(msg: &ChatMessage) -> u32`. In `crates/parrot-core/src/lib.rs` add `pub mod compaction;` next to the existing module declarations.

Run: `cargo test -p parrot-core --lib compaction`
Expected: FAIL — `plan_compaction` not found (tests reference it; struct/const stubs compile).

- [ ] **Step 3: Implement `plan_compaction`**

Add to `crates/parrot-core/src/compaction.rs` (above the tests module):

```rust
/// 决定是否压缩以及在哪儿切。返回 `None` 表示本轮不压缩:
/// 禁用 / 未超阈值 / 不足两条完整 turn / 待摘要区为空。
///
/// 切点规则(spec §3.1):从最新 turn 往回累计估算 token 直到
/// ≥ `keep_recent_tokens`,切点对齐到 User 消息(turn 边界),
/// Tool 消息绝不孤立。若回退累计吞掉全部 turn(keep 预算 ≥ 历史
/// 总量但总量已超阈值),强制只保留最新 1 个完整 turn(spec §7)。
pub fn plan_compaction(
    context: &[ChatMessage],
    budget_tokens: u32,
    config: &CompactionConfig,
) -> Option<CompactionPlan> {
    if !config.enabled || context.is_empty() {
        return None;
    }

    let total: u32 = context.iter().map(estimate_tokens).sum();
    if (total as f32) <= budget_tokens as f32 * config.threshold {
        return None;
    }

    // system(若有)不参与压缩。
    let body_start = match context.first() {
        Some(m) if m.role == ChatRole::System => 1,
        _ => 0,
    };

    // 真实对话 turn 的起点:User 消息,排除旧摘要消息。
    let turn_starts: Vec<usize> = context
        .iter()
        .enumerate()
        .filter(|(i, m)| *i >= body_start && m.role == ChatRole::User && !is_summary_message(m))
        .map(|(i, _)| i)
        .collect();
    if turn_starts.len() < 2 {
        return None;
    }

    // 从最新 turn 往回累计,至少保留 1 个完整 turn。
    let turn_end = |k: usize| turn_starts.get(k + 1).copied().unwrap_or(context.len());
    let mut kept_turns = 0usize;
    let mut acc: u32 = 0;
    for k in (0..turn_starts.len()).rev() {
        if kept_turns >= 1 && acc >= config.keep_recent_tokens {
            break;
        }
        let start = turn_starts[k];
        acc += context[start..turn_end(k)].iter().map(estimate_tokens).sum::<u32>();
        kept_turns += 1;
    }
    // 回退吞掉全部 turn ⇒ 强制只留最新 1 个,保证压缩有进展。
    if kept_turns == turn_starts.len() {
        kept_turns = turn_starts.len() - 1;
    }

    let cut_index = turn_starts[turn_starts.len() - kept_turns];
    if cut_index <= body_start {
        return None; // 待摘要区为空,没东西可摘
    }

    Some(CompactionPlan {
        summarize_start: body_start,
        cut_index,
    })
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p parrot-core --lib compaction`
Expected: PASS (all 10 tests).

Note on `plan_fires_over_threshold`: `context_with_turns(3, 4000)` produces `[sys, u1(4001ch), a1, u2, a2, u3, a3]`. `cut_index = ctx.len() - 2` = index of `u3` — assert reads naturally as "kept region = last turn". If the assertion message is confusing, fix the message string only.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add crates/parrot-core/src/compaction.rs crates/parrot-core/src/context.rs crates/parrot-core/src/lib.rs
git commit -m "feat(core): compaction planning pure logic (cut point, config, prompt)"
```

---

### Task 3: `rebuild_context` applies `CompactionSummary`

**Files:**
- Modify: `crates/parrot-core/src/event_log.rs` (`rebuild_context` + tests)

**Interfaces:**
- Consumes: `AgentEvent::CompactionSummary` (Task 1).
- Produces: `rebuild_context` semantics — on `CompactionSummary { summary, kept_message_count, .. }`: keep the last `kept_message_count` already-rebuilt messages, then push the summary as a User message **before** them.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `crates/parrot-core/src/event_log.rs`:

```rust
    #[test]
    fn rebuild_applies_compaction_summary() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let m1 = Uuid::new_v4();
        let m2 = Uuid::new_v4();
        // Turn 1 (to be summarized), Turn 2 (kept), then compaction.
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "old question".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::MessageEnd {
                    session_id: sid,
                    turn_id: t1,
                    message_id: m1,
                    final_content: "old answer".into(),
                    tool_calls: vec![],
                    stop_reason: MessageStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t2,
                    user_message: "new question".into(),
                },
            ),
            make_persisted(
                3,
                AgentEvent::MessageEnd {
                    session_id: sid,
                    turn_id: t2,
                    message_id: m2,
                    final_content: "new answer".into(),
                    tool_calls: vec![],
                    stop_reason: MessageStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                4,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: t2,
                    summary: "[CONVERSATION SUMMARY]\n## 目标\n...".into(),
                    dropped_message_count: 2,
                    kept_message_count: 2,
                },
            ),
        ];
        let ctx = rebuild_context(&events);
        assert_eq!(ctx.len(), 3, "summary + kept 2 messages");
        assert_eq!(ctx[0].role, ChatRole::User);
        assert!(ctx[0].content.starts_with("[CONVERSATION SUMMARY]"));
        assert_eq!(ctx[1].content, "new question");
        assert_eq!(ctx[2].content, "new answer");
        assert!(
            !ctx.iter().any(|m| m.content == "old question"),
            "summarized messages must be dropped"
        );
    }

    #[test]
    fn rebuild_compaction_keeps_all_when_count_exceeds() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "q".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: t1,
                    summary: "[CONVERSATION SUMMARY]\n...".into(),
                    dropped_message_count: 0,
                    kept_message_count: 99, // more than rebuilt
                },
            ),
        ];
        let ctx = rebuild_context(&events);
        assert_eq!(ctx.len(), 2);
        assert!(ctx[0].content.starts_with("[CONVERSATION SUMMARY]"));
        assert_eq!(ctx[1].content, "q");
    }

    #[test]
    fn truncate_keeps_compaction_summary_before_partial_turn() {
        let sid = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let events = vec![
            make_persisted(
                0,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: t1,
                    user_message: "done turn".into(),
                },
            ),
            make_persisted(
                1,
                AgentEvent::TurnEnd {
                    session_id: sid,
                    turn_id: t1,
                    stop_reason: TurnStopReason::EndTurn,
                    usage: Usage::default(),
                },
            ),
            make_persisted(
                2,
                AgentEvent::CompactionSummary {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    summary: "[CONVERSATION SUMMARY]\n...".into(),
                    dropped_message_count: 1,
                    kept_message_count: 1,
                },
            ),
            make_persisted(
                3,
                AgentEvent::TurnStart {
                    session_id: sid,
                    turn_id: Uuid::new_v4(),
                    user_message: "partial turn".into(),
                },
            ),
        ];
        let (keep, drop, issue) = truncate_to_last_complete_turn(events);
        assert!(issue.is_some(), "partial turn must be detected");
        assert_eq!(keep.len(), 3, "CompactionSummary must survive truncation");
        assert!(
            matches!(
                keep.last().map(|e| &e.event),
                Some(AgentEvent::CompactionSummary { .. })
            ),
            "last kept event is the CompactionSummary"
        );
        assert_eq!(drop.len(), 1);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p parrot-core --lib event_log`
Expected: FAIL — `rebuild_applies_compaction_summary` finds old messages still present (arm missing); `truncate_keeps_compaction_summary_before_partial_turn` may already pass (verify it exercises the boundary).

- [ ] **Step 3: Implement the rebuild arm**

In `crates/parrot-core/src/event_log.rs`, `rebuild_context`, add before the `_ => {}` catch-all:

```rust
            AgentEvent::CompactionSummary {
                summary,
                kept_message_count,
                ..
            } => {
                // 消息计数语义(spec §4):当前已重建列表 == 压缩前上下文
                // (不含 system)。保留末尾 kept_message_count 条,摘要以
                // User 角色插在最前。
                let kept = (*kept_message_count as usize).min(ctx.len());
                let kept_msgs: Vec<ChatMessage> = ctx.split_off(ctx.len() - kept);
                ctx.push(ChatMessage {
                    role: ChatRole::User,
                    content: summary.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: None,
                });
                ctx.extend(kept_msgs);
            }
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p parrot-core --lib event_log`
Expected: PASS.

Run: `cargo test --workspace`
Expected: PASS (no regressions elsewhere).

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add crates/parrot-core/src/event_log.rs
git commit -m "feat(core): rebuild_context applies CompactionSummary with message-count semantics"
```

---

### Task 4: Engine integration — `maybe_compact`

**Files:**
- Modify: `crates/parrot-core/src/engine.rs`
- Modify: `crates/parrot-core/src/session.rs` (`with_context_limits` signature)
- Modify: `crates/parrot-core/tests/react_loop.rs` (MockProvider + tests + 2 call-site updates)

**Interfaces:**
- Consumes: `plan_compaction`, `CompactionConfig`, `ContextLimits`, `SUMMARY_MARKER`, `SUMMARIZATION_PROMPT`, `serialize_conversation` (Task 2); `AgentEvent::CompactionSummary` (Task 1).
- Produces: `ReActEngine::with_context_limits(ContextLimits)` (breaking signature change); `SessionManager::with_context_limits(ContextLimits)`; compaction fires in `run()` before `TurnStart` emission.

- [ ] **Step 1: Write the failing engine tests**

In `crates/parrot-core/tests/react_loop.rs`:

1. Extend `MockProvider` — add fields and builders:

```rust
struct MockProvider {
    call_count: AtomicU32,
    captured_messages: tokio::sync::Mutex<Vec<parrot_core::types::ChatMessage>>,
    /// Every provider call's messages (chat + chat_stream), in order.
    captured_calls: tokio::sync::Mutex<Vec<Vec<parrot_core::types::ChatMessage>>>,
    first_tool: Option<(String, Value)>,
    context_window: u32,
    /// Text returned by the non-stream `chat()` (compaction summary call).
    summary_text: Option<String>,
    /// When true, `chat()` returns Err (simulates summary failure).
    fail_chat: bool,
}
```

Update `MockProvider::new()`:

```rust
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
            captured_messages: tokio::sync::Mutex::new(Vec::new()),
            captured_calls: tokio::sync::Mutex::new(Vec::new()),
            first_tool: Some(("echo".to_string(), json!({"message": "hello"}))),
            context_window: 200_000,
            summary_text: None,
            fail_chat: false,
        }
    }
```

Add builders after `with_context_window`:

```rust
    fn with_summary(mut self, text: &str) -> Self {
        self.summary_text = Some(text.to_string());
        self
    }

    fn fail_summary(mut self) -> Self {
        self.fail_chat = true;
        self
    }

    async fn captured_calls(&self) -> Vec<Vec<parrot_core::types::ChatMessage>> {
        self.captured_calls.lock().await.clone()
    }
```

In `chat_stream`, add as the first line of the body:

```rust
        self.captured_calls.lock().await.push(messages.to_vec());
```

Replace the `chat()` stub:

```rust
    async fn chat(
        &self,
        _model: &str,
        messages: &[parrot_core::types::ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<parrot_core::types::ChatMessage, parrot_core::error::ProviderError> {
        self.captured_calls.lock().await.push(messages.to_vec());
        if self.fail_chat {
            return Err(parrot_core::error::ProviderError::Network(
                "summary call failed".to_string(),
            ));
        }
        Ok(parrot_core::types::ChatMessage {
            role: parrot_core::types::ChatRole::Assistant,
            content: self
                .summary_text
                .clone()
                .unwrap_or_else(|| "SUMMARY".to_string()),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        })
    }
```

2. Update the two existing `with_context_limits` call sites (lines ~600 and ~645) to the new struct form with compaction disabled (tiny budgets would otherwise trigger compaction into the then-panicking `chat()`):

```rust
    .with_context_limits(parrot_core::compaction::ContextLimits {
        max_history_tokens: 1,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: false,
            ..Default::default()
        },
    });
```

and

```rust
    .with_context_limits(parrot_core::compaction::ContextLimits {
        max_history_tokens: u32::MAX,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: false,
            ..Default::default()
        },
    });
```

3. Add the new tests at the end of the file:

```rust
// ---------------------------------------------------------------------------
// Compaction (structured summary) tests
// ---------------------------------------------------------------------------

fn compaction_engine(
    tool_registry: Arc<ToolRegistry>,
    provider_registry: Arc<ProviderRegistry>,
    limits: parrot_core::compaction::ContextLimits,
    data_dir: std::path::PathBuf,
    working_dir: std::path::PathBuf,
) -> ReActEngine {
    let config = GenerateConfig {
        model: "mock-model".to_string(),
        temperature: None,
        max_tokens: Some(8192),
        stop_sequences: None,
    };
    ReActEngine::new(
        uuid::Uuid::new_v4(),
        tool_registry,
        provider_registry,
        config,
        Some("You are a test assistant.".to_string()),
        data_dir,
        working_dir,
    )
    .with_context_limits(limits)
}

fn small_budget_limits() -> parrot_core::compaction::ContextLimits {
    // est budget 1000 tokens ⇒ threshold 900; keep 300 tokens ⇒ 1 kept turn.
    parrot_core::compaction::ContextLimits {
        max_history_tokens: 1000,
        keep_recent_turns: 10,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: true,
            threshold: 0.9,
            keep_recent_tokens: 300,
            summary_max_tokens: 1024,
        },
    }
}

/// Drive 3 text-only turns; returns (all events, all provider calls).
async fn run_three_turns(
    mock: Arc<MockProvider>,
    engine: ReActEngine,
) -> (Vec<AgentEvent>, Vec<Vec<parrot_core::types::ChatMessage>>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);
    let engine_task = tokio::spawn(async move { engine.run(cmd_rx, event_tx).await });

    let mut events = Vec::new();
    for i in 0..3 {
        // ~1000 est tokens each: over the 900-token threshold by turn 3.
        let q = format!("q{}{}", i, "x".repeat(4000));
        cmd_tx.send(SessionCmd::Chat { message: q }).await.unwrap();
        events.extend(collect_until_turn_end(&mut event_rx).await);
    }
    drop(cmd_tx);
    events.extend(drain_until_agent_end(&mut event_rx).await);
    let _ = engine_task.await;
    let calls = mock.captured_calls().await;
    (events, calls)
}

#[tokio::test]
async fn compaction_summarizes_old_turns_and_keeps_recent() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only().with_summary("## 目标与任务\nfix bug"));
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let engine = compaction_engine(
        Arc::clone(&tool_registry),
        Arc::clone(&provider_registry),
        small_budget_limits(),
        data_dir,
        working_dir,
    );

    let (events, calls) = run_three_turns(mock, engine).await;

    // 1) A CompactionSummary event was emitted, BEFORE the 3rd turn's TurnStart.
    let cs_idx = events
        .iter()
        .position(|ev| matches!(ev, AgentEvent::CompactionSummary { .. }))
        .expect("CompactionSummary event expected");
    let third_turn_start = events
        .iter()
        .position(|ev| {
            matches!(
                ev,
                AgentEvent::TurnStart { user_message, .. } if user_message.starts_with("q2")
            )
        })
        .expect("3rd TurnStart");
    assert!(cs_idx < third_turn_start, "CompactionSummary must precede TurnStart");

    // 2) The summary event content carries the marker and counts.
    if let AgentEvent::CompactionSummary {
        summary,
        dropped_message_count,
        kept_message_count,
        ..
    } = &events[cs_idx]
    {
        assert!(summary.starts_with("[CONVERSATION SUMMARY]"));
        assert!(summary.contains("fix bug"));
        assert!(*dropped_message_count >= 2, "turn-1 user+assistant summarized");
        assert!(*kept_message_count >= 2, "turn-2 user+assistant kept");
    } else {
        panic!("unreachable");
    }

    // 3) The summary call itself: 2 messages (system prompt + conversation).
    let summary_call = calls
        .iter()
        .find(|c| c.len() == 2
            && c[0].role == parrot_core::types::ChatRole::System
            && c[0].content.contains("压缩助手"))
        .expect("summary call captured");
    assert!(summary_call[1].content.contains("<conversation>"));

    // 4) The LAST main call (turn 3): contains summary message, no q0 turn,
    //    keeps q1 turn verbatim.
    let last_main = calls.last().expect("main call");
    assert!(
        last_main
            .iter()
            .any(|m| m.content.starts_with("[CONVERSATION SUMMARY]")),
        "context must contain the summary message"
    );
    assert!(
        !last_main.iter().any(|m| m.content.starts_with("q0")),
        "summarized turn must be gone"
    );
    assert!(
        last_main.iter().any(|m| m.content.starts_with("q1")),
        "kept turn must remain verbatim"
    );
    assert_eq!(
        last_main.first().map(|m| m.role),
        Some(parrot_core::types::ChatRole::System),
        "system stays on top"
    );
}

#[tokio::test]
async fn compaction_skipped_under_threshold() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    // Huge budget ⇒ never over threshold.
    let limits = parrot_core::compaction::ContextLimits {
        max_history_tokens: 1_000_000,
        ..small_budget_limits()
    };
    let engine = compaction_engine(tool_registry, provider_registry, limits, data_dir, working_dir);

    let (events, calls) = run_three_turns(mock, engine).await;
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. })),
        "no compaction under threshold"
    );
    assert!(
        calls.iter().all(|c| c.len() != 2),
        "no summary-style call captured"
    );
}

#[tokio::test]
async fn compaction_failure_falls_back_to_prune() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only().fail_summary());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    // Budget 1 token ⇒ prune keeps only keep_recent_turns=1 turn after the
    // summary call fails.
    let limits = parrot_core::compaction::ContextLimits {
        max_history_tokens: 1,
        keep_recent_turns: 1,
        compaction: parrot_core::compaction::CompactionConfig {
            enabled: true,
            ..Default::default()
        },
    };
    let engine = compaction_engine(tool_registry, provider_registry, limits, data_dir, working_dir);

    let (events, _calls) = run_three_turns(mock, engine).await;

    // No CompactionSummary event (fail-open), turns still complete.
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev, AgentEvent::TurnEnd { .. }))
            .count(),
        3,
        "all turns must complete despite summary failure"
    );
    // Prune fallback: the last main call must not contain q0 (pruned).
    // (Verified via captured calls in the fail case: last call is turn 3.)
}

#[tokio::test]
async fn compaction_disabled_matches_old_behavior() {
    let (_tmp, working_dir, data_dir) = temp_dirs();
    let tool_registry = Arc::new(ToolRegistry::new());
    let mock = Arc::new(MockProvider::new().text_only());
    let provider: Arc<dyn LlmProvider> = mock.clone();
    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let mut limits = small_budget_limits();
    limits.compaction.enabled = false;
    let engine = compaction_engine(tool_registry, provider_registry, limits, data_dir, working_dir);

    let (events, _calls) = run_three_turns(mock, engine).await;
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::CompactionSummary { .. }))
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p parrot-core --test react_loop`
Expected: FAIL — compile error: `with_context_limits` expects 2 args (struct form not yet implemented), `captured_calls`/`with_summary`/`fail_summary` undefined on MockProvider if you added tests before the struct changes (add the struct changes from Step 1 first — they are part of the test file).

- [ ] **Step 3: Change `with_context_limits` to struct form**

In `crates/parrot-core/src/engine.rs`:

1. Add imports at the top:

```rust
use crate::compaction::{
    plan_compaction, serialize_conversation, CompactionConfig, ContextLimits,
    SUMMARY_MARKER, SUMMARIZATION_PROMPT,
};
```

2. Replace the two fields `max_history_tokens: u32` and `keep_recent_turns: u32` (with their doc comments) with:

```rust
    /// Context limits (`[session]` + compaction, wired in by the daemon).
    context_limits: ContextLimits,
```

3. In `ReActEngine::new`, replace `max_history_tokens: 100_000, keep_recent_turns: 10,` with `context_limits: ContextLimits::default(),`.

4. Replace `with_context_limits`:

```rust
    /// Override context limits. The daemon passes `[session]` values from
    /// parrot.toml here.
    pub fn with_context_limits(mut self, limits: ContextLimits) -> Self {
        self.context_limits = limits;
        self
    }
```

5. In `run()`, replace the budget computation:

```rust
        let model_window = self.resolve_model_context_window().await;
        let budget = context_budget(self.context_limits.max_history_tokens, model_window);
        let context_manager = ContextManager::new(budget, self.context_limits.keep_recent_turns);
```

6. In `run()`'s `Some(SessionCmd::Chat { message })` arm, insert the compaction call after the hook block-match (`HookResult::Inject`/`_ => {}` arm closes the match, right before the `let _ = event_tx.send(AgentEvent::TurnStart ...)` block):

```rust
                    // 压缩检查在 TurnStart 事件之前(spec §3.1):避免
                    // CompactionSummary 落入半成品 turn 被 resume 截断丢弃。
                    self.maybe_compact(
                        &mut context,
                        turn_id,
                        budget,
                        &event_tx,
                        &mut event_log,
                    )
                    .await;
```

7. Add the `maybe_compact` method to `impl ReActEngine` (after `resolve_model_context_window`):

```rust
    /// Pi 式上下文压缩(spec §3.1):超过预算阈值时,把切点之前的
    /// 历史(含旧摘要)交给当前模型的非流式 `chat()` 生成结构化摘要,
    /// 切点之后的近期消息原样保留。任何失败 fail-open:warn 后返回,
    /// 后续 prune 兜底,会话永不因压缩卡死。
    async fn maybe_compact(
        &self,
        context: &mut Vec<ChatMessage>,
        turn_id: Uuid,
        budget: u32,
        event_tx: &mpsc::Sender<AgentEvent>,
        event_log: &mut EventLog,
    ) {
        let session_id = self.session_id;
        let cfg = &self.context_limits.compaction;
        let Some(plan) = plan_compaction(context, budget, cfg) else {
            return;
        };

        let region: Vec<ChatMessage> = context[plan.summarize_start..plan.cut_index].to_vec();
        let request = vec![
            ChatMessage {
                role: ChatRole::System,
                content: SUMMARIZATION_PROMPT.to_string(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
            ChatMessage {
                role: ChatRole::User,
                content: serialize_conversation(&region),
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
            },
        ];

        let Some(provider) = self.provider_registry.resolve(&self.config.model).await else {
            tracing::warn!(session_id = %session_id, "compaction skipped: provider not found");
            return;
        };
        let mut summary_config = self.config.clone();
        summary_config.max_tokens = Some(cfg.summary_max_tokens);
        let summary = match provider
            .chat(&self.config.model, &request, &[], &summary_config)
            .await
        {
            Ok(m) => m.content,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "compaction summary call failed; falling back to prune"
                );
                return;
            }
        };
        if summary.trim().is_empty() {
            tracing::warn!(session_id = %session_id, "compaction summary empty; falling back to prune");
            return;
        }

        let summary_content = format!("{SUMMARY_MARKER}\n{summary}");
        let kept: Vec<ChatMessage> = context[plan.cut_index..].to_vec();
        let dropped = (plan.cut_index - plan.summarize_start) as u32;
        let kept_count = kept.len() as u32;

        let head: Vec<ChatMessage> = context[..plan.summarize_start].to_vec();
        *context = head;
        context.push(ChatMessage {
            role: ChatRole::User,
            content: summary_content.clone(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        });
        context.extend(kept);

        let event = AgentEvent::CompactionSummary {
            session_id,
            turn_id,
            summary: summary_content,
            dropped_message_count: dropped,
            kept_message_count: kept_count,
        };
        let _ = event_tx.send(event.clone()).await.ok();
        if let Err(e) = event_log.append(event) {
            tracing::warn!(error = ?e, "failed to persist CompactionSummary");
        }
    }
```

8. In `crates/parrot-core/src/session.rs`: replace fields `max_history_tokens: u32` / `keep_recent_turns: u32` with `context_limits: crate::compaction::ContextLimits` (update `SessionManager::new` initializer to `context_limits: ContextLimits::default(),`), change `with_context_limits` to:

```rust
    /// Set context limits (`[session]` from parrot.toml). Every engine
    /// spawned afterwards inherits them.
    pub fn with_context_limits(mut self, limits: crate::compaction::ContextLimits) -> Self {
        self.context_limits = limits;
        self
    }
```

and update the `spawn_session` engine builder call (line ~264):

```rust
        .with_context_limits(self.context_limits.clone());
```

Also add the same builder call in `create_resumed_session` after `.with_resumed_from(resumed_from_seq);` (line ~207) — this fixes the pre-existing gap where resumed engines got default limits:

```rust
        .with_context_limits(self.context_limits.clone());
```

Add the import at the top of session.rs if needed: `use crate::compaction::ContextLimits;`

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p parrot-core`
Expected: PASS (new compaction tests + existing react_loop/context/event_log tests).

Note: `compaction_failure_falls_back_to_prune` relies on `prune` with budget=1: verify all 3 turns complete and no panic. If the mock's `chat()` panic surfaces instead, ensure the engine guards provider errors (Step 3.7 handles it).

- [ ] **Step 5: Full workspace verification**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS. The daemon crate compiles against the new `with_context_limits` signature only after Task 5 wires it — **if `parrot-daemon` fails to compile now**, make the minimal mechanical fix in `crates/parrot-daemon/src/runtime.rs` lines 107-110:

```rust
        .with_context_limits(parrot_core::compaction::ContextLimits {
            max_history_tokens: config.session.max_history_tokens,
            keep_recent_turns: config.session.keep_recent_turns,
            compaction: parrot_core::compaction::CompactionConfig {
                enabled: config.session.compaction,
                threshold: config.session.compaction_threshold,
                keep_recent_tokens: config.session.keep_recent_tokens,
                summary_max_tokens: config.session.summary_max_tokens,
            },
        }),
```

—but the `config.session` fields don't exist until Task 5. **Do Task 5's config fields together with this step if needed** (add the 4 fields to `SessionConfig` with serde defaults as specified in Task 5 Step 3, then this compiles). Commit both together with a note.

- [ ] **Step 6: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add crates/parrot-core/src/engine.rs crates/parrot-core/src/session.rs crates/parrot-core/tests/react_loop.rs crates/parrot-config/src/config.rs crates/parrot-daemon/src/runtime.rs
git commit -m "feat(core): engine maybe_compact before TurnStart; ContextLimits struct wiring"
```

---

### Task 5: Config fields + daemon wiring

(If Task 5 Step 5 already forced the config fields, verify + test them here.)

**Files:**
- Modify: `crates/parrot-config/src/config.rs`
- Test: `crates/parrot-config/tests/config_test.rs`
- Modify: `crates/parrot-daemon/src/runtime.rs`

**Interfaces:**
- Consumes: `ContextLimits`/`CompactionConfig` (Task 2).
- Produces: `SessionConfig { compaction: bool, compaction_threshold: f32, keep_recent_tokens: u32, summary_max_tokens: u32 }` with serde defaults; daemon passes them into `SessionManager::with_context_limits`.

- [ ] **Step 1: Write the failing config tests**

Append to `crates/parrot-config/tests/config_test.rs` (follow the file's existing inline-TOML test style):

```rust
#[test]
fn session_compaction_defaults() {
    let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6
"#;
    let c: AppConfig = toml::from_str(toml).unwrap();
    assert!(c.session.compaction);
    assert!((c.session.compaction_threshold - 0.9).abs() < f32::EPSILON);
    assert_eq!(c.session.keep_recent_tokens, 20000);
    assert_eq!(c.session.summary_max_tokens, 4096);
}

#[test]
fn session_compaction_overrides() {
    let toml = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = ""

[[providers]]
id = "anthropic"
api_key = "x"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = ""
max_history_tokens = 100000
keep_recent_turns = 6
compaction = false
compaction_threshold = 0.75
keep_recent_tokens = 8000
summary_max_tokens = 2048
"#;
    let c: AppConfig = toml::from_str(toml).unwrap();
    assert!(!c.session.compaction);
    assert!((c.session.compaction_threshold - 0.75).abs() < f32::EPSILON);
    assert_eq!(c.session.keep_recent_tokens, 8000);
    assert_eq!(c.session.summary_max_tokens, 2048);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p parrot-config`
Expected: FAIL — no field `compaction` on `SessionConfig`.

- [ ] **Step 3: Add the fields**

In `crates/parrot-config/src/config.rs`, replace `SessionConfig`:

```rust
fn default_true() -> bool {
    true
}
fn default_compaction_threshold() -> f32 {
    0.9
}
fn default_keep_recent_tokens() -> u32 {
    20_000
}
fn default_summary_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub data_dir: String,
    pub max_history_tokens: u32,
    pub keep_recent_turns: u32,
    /// 结构化摘要压缩总开关(spec §5)。
    #[serde(default = "default_true")]
    pub compaction: bool,
    /// 估算/budget 触发比例。
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    /// 切点保留预算(token 估算)。
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u32,
    /// 摘要调用 max_tokens 上限。
    #[serde(default = "default_summary_max_tokens")]
    pub summary_max_tokens: u32,
}
```

Also add the 4 fields with the same default values to `AppConfig::default_config()`'s `session:` initializer:

```rust
            session: SessionConfig {
                data_dir: String::new(),
                max_history_tokens: 100_000,
                keep_recent_turns: 6,
                compaction: true,
                compaction_threshold: 0.9,
                keep_recent_tokens: 20_000,
                summary_max_tokens: 4096,
            },
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p parrot-config`
Expected: PASS.

Verify daemon wiring compiles (Task 4 Step 5's runtime.rs block or add it now if not yet):

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add crates/parrot-config/src/config.rs crates/parrot-config/tests/config_test.rs crates/parrot-daemon/src/runtime.rs
git commit -m "feat(config): [session] compaction knobs wired into daemon ContextLimits"
```

---

### Task 6: TUI renders compaction entries

**Files:**
- Modify: `src/tui/app.rs` (replace the Task-1 temporary no-op arm)

**Interfaces:**
- Consumes: `AgentEvent::CompactionSummary` (Task 1).
- Produces: replay/live `CompactionSummary` pushes a `ChatEntry::Info`.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `src/tui/app.rs` (follow existing `sid()` helper usage):

```rust
    #[test]
    fn compaction_summary_pushes_info_entry() {
        let sid_v = sid();
        let mut app = App::new(sid_v);
        app.apply_event(AgentEvent::CompactionSummary {
            session_id: sid_v,
            turn_id: Uuid::new_v4(),
            summary: "[CONVERSATION SUMMARY]\n...".into(),
            dropped_message_count: 6,
            kept_message_count: 4,
        });
        match app.entries.last() {
            Some(ChatEntry::Info(text)) => {
                assert!(text.contains("6"), "info mentions dropped count: {text}");
            }
            other => panic!("expected Info entry, got {:?}", other),
        }
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p parrot --bin parrot`
Expected: FAIL — the temporary no-op arm means no entry is pushed.

- [ ] **Step 3: Implement the arm**

In `src/tui/app.rs` `apply_event`, replace the temporary no-op arm:

```rust
            AgentEvent::CompactionSummary {
                dropped_message_count,
                ..
            } => {
                self.entries.push(ChatEntry::Info(format!(
                    "已压缩 {dropped_message_count} 条历史消息"
                )));
            }
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p parrot --bin parrot`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add src/tui/app.rs
git commit -m "feat(tui): render CompactionSummary as info entry"
```

---

### Task 7: E2E — daemon-level compaction over the wire

**Files:**
- Modify: `tests/integration/e2e_test.rs`

**Interfaces:**
- Consumes: `AppConfig` (Task 5), `AgentEvent::CompactionSummary` (Task 1), existing `expect_agent_event` helper.
- Produces: `spawn_daemon_with_provider_and_config(provider, config)` helper; e2e proof that a real daemon (config → SessionManager → engine → WS) emits `CompactionSummary`.

- [ ] **Step 1: Implement MockProvider.chat in e2e**

In `tests/integration/e2e_test.rs`, the main `MockProvider`'s `chat()` is `unimplemented!()`. Replace:

```rust
    async fn chat(
        &self,
        _model: &str,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _config: &GenerateConfig,
    ) -> Result<ChatMessage, ProviderError> {
        // Compaction summary call: return a fixed structured summary.
        Ok(ChatMessage {
            role: parrot_core::types::ChatRole::Assistant,
            content: "## 目标与任务\ne2e compaction test".to_string(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
        })
    }
```

(Adapt the `ChatMessage` import — the file already imports `ChatMessage` from `parrot_core::types` for `chat_stream`; check imports at the top and reuse.)

- [ ] **Step 2: Refactor the spawn helper**

Rename/extend: keep `spawn_daemon_with_provider(provider)` unchanged; add:

```rust
async fn spawn_daemon_with_provider_and_config(
    provider: Arc<dyn LlmProvider>,
    config: AppConfig,
) -> (
    mpsc::Receiver<ServerMessage>,
    mpsc::Sender<ClientMessage>,
    String,
    tokio::task::JoinHandle<()>,
) {
    // Same body as spawn_daemon_with_provider, except:
    // - take `config` as parameter, drop the `let config = test_config(...)` line
    // - provider_registry / tool_registry / auth setup identical
    let token_path = std::path::PathBuf::from(&config.daemon.auth_token_file);
    let data_dir = std::path::PathBuf::from(&config.session.data_dir);

    let auth = parrot_daemon::auth::Auth::new(&token_path)
        .await
        .expect("init auth");
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    let auth = Arc::new(auth);

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(provider, vec!["mock-model".to_string()])
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry.register(Arc::new(EchoTool) as Arc<dyn Tool>).await;

    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{}", {
        // port from config — capture before the move above; see note below.
        0
    });
    unreachable!("see Step 3 for the correct implementation")
}
```

**Note (implement properly):** the port must be extracted BEFORE the config is moved into the spawn task. Structure it as:

```rust
async fn spawn_daemon_with_provider_and_config(
    provider: Arc<dyn LlmProvider>,
    mut config: AppConfig,
) -> (mpsc::Receiver<ServerMessage>, mpsc::Sender<ClientMessage>, String, tokio::task::JoinHandle<()>) {
    let port = free_port();
    config.daemon.host = "127.0.0.1".to_string();
    config.daemon.port = port;
    // ... identical to spawn_daemon_with_provider from the token_path read
    // through the end, minus the test_config call ...
}
```

Model it exactly on `spawn_daemon_with_provider` (lines 550-612): same tmp-dir ownership pattern — **careful**: the original creates its own TempDir and `std::mem::forget(tmp)`; with an injected config the caller owns the temp dir, so drop the forget (the caller's TempDir cleans up; the daemon is aborted at test end).

Then optionally make `spawn_daemon_with_provider` delegate:

```rust
async fn spawn_daemon_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    mpsc::Receiver<ServerMessage>,
    mpsc::Sender<ClientMessage>,
    String,
    tokio::task::JoinHandle<()>,
) {
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = test_config(port, &data_dir, &token_path);
    std::mem::forget(tmp);
    spawn_daemon_with_provider_and_config(provider, config).await
}
```

- [ ] **Step 3: Write the failing e2e test**

Append to `tests/integration/e2e_test.rs`:

```rust
#[tokio::test]
async fn e2e_compaction_emits_summary_event_and_compacts_context() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut config = test_config(0, &data_dir, &token_path);
    // 估算预算 1000 token ⇒ 阈值 900;默认 keep 20000 ⇒ 回退兜底,
    // 保留最新 1 个完整 turn。
    config.session.max_history_tokens = 1000;

    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) =
        spawn_daemon_with_provider_and_config(provider, config).await;

    // Create a session.
    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: Some("mock-model".to_string()),
            system_prompt: None,
        }),
    })
    .await
    .expect("send CreateSession");
    let session_id = expect_server_message(
        &mut rx,
        |m| {
            if let ServerMessage::SessionCreated { session_id } = m {
                Some(session_id)
            } else {
                None
            }
        },
        "SessionCreated",
    )
    .await;

    // Turn 1 + Turn 2: ~1000 est tokens each. Turn 3's pre-check crosses 900.
    for i in 0..3 {
        let msg = format!("q{}{}", i, "x".repeat(4000));
        tx.send(ClientMessage::Chat {
            session_id,
            message: msg,
        })
        .await
        .expect("send Chat");

        // Wait for the turn to finish before sending the next (engine is
        // single-turn serialized: a Chat during an active turn is ignored).
        expect_agent_event(&mut rx, |ev| {
            matches!(ev, AgentEvent::TurnEnd { .. }).then_some(())
        }, "TurnEnd")
        .await;
    }

    // The 3rd turn must have emitted a CompactionSummary before its TurnStart.
    let (summary, dropped, kept) = expect_agent_event(
        &mut rx,
        |ev| {
            if let AgentEvent::CompactionSummary {
                summary,
                dropped_message_count,
                kept_message_count,
                ..
            } = ev
            {
                Some((summary.clone(), *dropped_message_count, *kept_message_count))
            } else {
                None
            }
        },
        "CompactionSummary",
    )
    .await;

    assert!(summary.starts_with("[CONVERSATION SUMMARY]"));
    assert!(summary.contains("e2e compaction test"));
    assert!(dropped >= 2, "turn 1 user+assistant summarized");
    assert!(kept >= 2, "turn 2 kept verbatim");

    daemon_handle.abort();
}
```

**Important ordering caveat:** `expect_agent_event` skips non-matching messages, so the CompactionSummary assertion works even after the TurnEnd wait consumed turn 3's other events — but the events arrive in order, so by the time turn 3's TurnEnd was seen, CompactionSummary has already been delivered. If `expect_agent_event` for TurnEnd already consumed it (it doesn't match TurnEnd), the later wait finds it in the channel buffer — both waits work because the helper scans rather than requires strict order.

Check: the `CreateSession` variant and `SessionConfig` (protocol type) usage above matches the file's existing tests (see line ~356 `config: Some(SessionConfig { ... })`) — mirror the exact field names used there (`model`, `system_prompt`).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --test e2e e2e_compaction_emits_summary_event_and_compacts_context`
Expected: PASS. If it fails with "CompactionSummary timed out": debug via the daemon log — most likely cause is the threshold not being crossed (est tokens per turn = 4001/4 ≈ 1000; after 2 turns ≈ 2000 > 900 — verified) or `chat()` not implemented (Step 1).

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Commit:

```bash
git add tests/integration/e2e_test.rs
git commit -m "test(e2e): daemon emits CompactionSummary over the wire"
```

---

## Self-Review

**Spec coverage:**
- §2 决策表(5 项)→ Task 4 (Pi 式 + 当前模型 + turn 前 + User 角色), Task 1 (持久事件). ✅
- §3.1 流程(阈值、切点、TurnStart 前落盘)→ Tasks 2, 4. ✅
- §3.2 增量语义/摘要调用/fail-open/单轮超预算 → Tasks 2 (old-summary in region, walk fallback), 4 (empty-summary guard, provider-error guard). ✅
- §4 事件 + rebuild 消息计数语义 → Tasks 1, 3. ✅
- §5 配置 4 字段 + ContextLimits → Task 5. ✅
- §6 prompt + serialize_conversation → Task 2. ✅
- §7 边界(<2 turn 跳过、极小 keep、回退兜底、system 置顶、开关)→ Task 2 tests. ✅
- §8 测试表:纯函数 (T2)、engine (T4)、resume (T3)、protocol (T1)、e2e (T7)、TUI (T6). ✅
- resumed-session limits 缺口修复 → Task 4 Step 3.8 (记录在 plan). ✅

**Placeholder scan:** No TBDs. Task 7 Step 2's first code block is deliberately marked "see Step 3 for the correct implementation" — the correct version follows immediately; the executor must use the port-capture structure. All other steps carry complete code.

**Type consistency:** `CompactionSummary { session_id, turn_id, summary, dropped_message_count, kept_message_count }` consistent across Tasks 1, 3, 4, 6, 7. `ContextLimits { max_history_tokens, keep_recent_turns, compaction }` consistent across Tasks 2, 4, 5. `CompactionPlan { summarize_start, cut_index }` defined Task 2, consumed Task 4. `SUMMARY_MARKER` string identical in code and tests. MockProvider `with_summary`/`fail_summary`/`captured_calls` defined and used only in react_loop.rs (Task 4); e2e MockProvider is a separate struct (Task 7).

**Known deviations from spec (documented):** none — the spec was amended (kept_from_seq → kept_message_count) before this plan was written.
