# AgentEndGuard Drop 持久化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** panic/task abort 时 `AgentEndGuard::drop` 把 `AgentEnd` 同步写入 `events.log`（先落后发），使 resume 能区分正常结束与崩溃中断。

**Architecture:** 把 `EventLog` move 进 `AgentEndGuard` 作为唯一所有者（方案 Z）。`run()` 全程经 `guard.event_log` 读写；正常路径 `fire_and_drop` 语义不变；`Drop` 在 `fired == false` 时 `append` + `try_send`。不引入 Arc seq、不扫描文件、不拆 engine、不动 EventSink。

**Tech Stack:** Rust 2021, tokio mpsc, `EventLog`（`std::fs` append）, `tempfile`（dev-dependency 已有）。

**Spec:** `docs/superpowers/specs/2026-09-22-agent-end-guard-drop-persist-design.md`

## Global Constraints

- 只改 `crates/parrot-core/src/engine.rs`（结构体、`run`、`fire_and_drop`、`Drop`、`#[cfg(test)]`）。不改 `event_log.rs` 对外行为、不改协议/daemon/TUI。
- `parrot-core` 保持 zero IO 增量约束：不新增 `reqwest`/`tokio::fs`/`std::env`；本任务用已有 `std::fs`（`EventLog::append`）路径。
- Drop 内禁止 `unwrap`/`expect` 落盘结果；失败仅 `tracing::warn`。
- Drop **不**跑 async hooks；半成品 `TurnEnd` 仍靠 `replay_for_resume`。
- 每个 task 结束后：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 必须通过。
- `fire_and_drop` 顺序不变：hooks → 落盘 → `fired = true` → `event_tx.send`（`fired` 在落盘后立刻置位，send 前唯一 await 点 abort 时 Drop 不再二次 append）。
- Drop 顺序：组装 event → `event_log.append` → `try_send`（先落后发）。

---

### Task 1: 将 `EventLog` 移入 `AgentEndGuard`（纯重构）

**Files:**
- Modify: `crates/parrot-core/src/engine.rs`（`AgentEndGuard` 结构体 ~1015、`fire_and_drop` ~1028、`run` ~158-323）

**Interfaces:**
- Consumes: 现有 `EventLog`、`AgentEndGuard` 字段、`run()` 局部 `event_log`。
- Produces:
  - `AgentEndGuard.event_log: EventLog`（私有字段）
  - `async fn fire_and_drop(mut self)` — **无 `event_log` 参数**
  - `run()` 内后续读写一律 `&mut guard.event_log`
  - Drop 行为本 task **不变**（仍仅 `try_send`）

- [ ] **Step 1: 改结构体 — 增加 `event_log` 字段**

在 `crates/parrot-core/src/engine.rs` 替换：

```rust
/// RAII 守卫：drop 时发 `AgentEnd`（除非已触发）。保证 panic 或任务
/// abort 时 `AgentEnd` 也一定发出。持有 `EventLog` 唯一所有权，以便
/// Drop 路径与正常路径共用同一 seq 写盘。
struct AgentEndGuard {
    session_id: Uuid,
    event_log: EventLog,
    event_tx: mpsc::Sender<AgentEvent>,
    fired: bool,
    total_usage: Arc<Mutex<Usage>>,
    reason: Arc<Mutex<AgentEndReason>>,
    hooks: Arc<HookRegistry>,
    working_dir: std::path::PathBuf,
}
```

- [ ] **Step 2: 改 `fire_and_drop` — 去掉参数，用 `self.event_log`**

替换整个 `impl AgentEndGuard` 块为：

```rust
impl AgentEndGuard {
    /// 显式触发 AgentEnd（正常退出路径调用，避免 Drop 不可控）。
    /// 先落盘再发送：resume 时靠读 events.log 区分"会话已结束"和"daemon 崩溃"。
    async fn fire_and_drop(mut self) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        let (_, execs) = self
            .hooks
            .run(
                HookEvent::AgentEnd {
                    session_id: self.session_id,
                },
                &self.working_dir,
            )
            .await;
        emit_hook_executions(&execs, self.session_id, &self.event_tx);
        let event = AgentEvent::AgentEnd {
            session_id: self.session_id,
            reason,
            total_usage: usage,
        };
        if let Err(e) = self.event_log.append(event.clone()) {
            tracing::warn!(error = ?e, "failed to persist AgentEnd");
        }
        self.fired = true;
        let _ = self.event_tx.send(event).await.ok();
    }
}
```

`impl Drop` **本步不改**（仍 `try_send` only）。

- [ ] **Step 3: 改 `run()` — 尽早构造 guard 并 move `event_log` 进去**

在 `run()` 中，将从 `let mut event_log = EventLog::new(...)` 到 `let guard = AgentEndGuard { ... }`（原 ~158-217）替换为：

```rust
        let mut event_log = EventLog::new(self.data_dir.clone());
        // resume 后续 seq 要从 replay 恢复点继续，保持与磁盘日志连续。
        if self.resume_seq_offset > 0 {
            event_log = event_log.with_start_seq(self.resume_seq_offset);
        }

        // RAII 守卫：确保 AgentEnd 即使 panic 也发。临界区都是 clone 取值后
        // 立刻丢锁，不在 await 间持锁，所以用 std::sync::Mutex 是安全的。
        // event_log 一并移入 guard：Drop 与 fire_and_drop 共用同一实例/seq。
        let total_usage: Arc<Mutex<Usage>> = Arc::new(Mutex::new(Usage::default()));
        let event_tx_for_guard = event_tx.clone();
        let mut guard = AgentEndGuard {
            session_id,
            event_log,
            event_tx: event_tx_for_guard,
            fired: false,
            total_usage: Arc::clone(&total_usage),
            reason: Arc::clone(&self.end_reason),
            hooks: Arc::clone(&self.hooks),
            working_dir: self.working_dir.clone(),
        };

        let provider_id = self.resolve_provider_id().await;
        let agent_start = AgentEvent::AgentStart {
            session_id,
            model: self.config.model.clone(),
            provider: provider_id.clone(),
            system_prompt_hash: self.system_prompt_hash.clone(),
            resumed_from_seq: self.resumed_from_seq,
        };
        emit_and_persist(&event_tx, &mut guard.event_log, agent_start).await;
```

注意：原代码里 `emit_and_persist(..., agent_start)` 与 hooks/integrity 段夹着 `provider_id` 计算——合并后顺序为 **先建 guard，再发 AgentStart**（缩短无 guard 窗口，spec §2.2）。原 ~174-196 的 hooks 与 `ReplayIntegrityWarning` 段保持逻辑不变，仅把其中的 `&mut event_log` 改为 `&mut guard.event_log`。

- [ ] **Step 4: `run()` 循环内所有 `event_log` 引用改为 `guard.event_log`**

在 `run()` 的 `loop` 内及 `maybe_compact` / `handle_turn` 调用处，将：

- `&mut event_log` → `&mut guard.event_log`

共约 6 处（Block-hint TurnEnd、`maybe_compact`、TurnStart、`handle_turn`、正常 TurnEnd；以及 integrity warning 段若 Step 3 未覆盖）。调用形如：

```rust
                            emit_and_persist(&event_tx, &mut guard.event_log, turn_end).await;
                            continue;
```

```rust
                    self.maybe_compact(&mut context, turn_id, budget, &event_tx, &mut guard.event_log)
                        .await;
```

```rust
                    emit_and_persist(&event_tx, &mut guard.event_log, turn_start).await;
```

```rust
                        &mut guard.event_log,
```

```rust
                    emit_and_persist(&event_tx, &mut guard.event_log, turn_end).await;
```

- [ ] **Step 5: 改 `run()` 末尾调用**

```rust
        guard.fire_and_drop().await;
```

（删除原 `guard.fire_and_drop(&mut event_log).await;`）

- [ ] **Step 6: 编译并跑全量测试（回归）**

Run: `cargo test --workspace`
Expected: PASS（既有 react_loop / resume / shutdown 测试不回归；Drop 仍仅 try_send，行为同改前）

若编译错误指向残留的 `event_log` 绑定或 `fire_and_drop(&mut event_log)`，修到编译通过。

- [ ] **Step 7: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Run: `cargo fmt --all -- --check`

```bash
git add crates/parrot-core/src/engine.rs
git commit -m "refactor(core): move EventLog into AgentEndGuard for shared seq ownership"
```

---

### Task 2: Drop 落盘 `AgentEnd` + 四条单测

**Files:**
- Modify: `crates/parrot-core/src/engine.rs`（`impl Drop` ~1057、`#[cfg(test)] mod tests` ~1087）

**Interfaces:**
- Consumes: Task 1 的 `AgentEndGuard.event_log`、`fire_and_drop(mut self)`。
- Produces:
  - `Drop`：`fired == false` 时 `self.event_log.append(AgentEnd)` 再 `try_send`
  - 测试 helper `fn test_guard(dir: PathBuf) -> (AgentEndGuard, mpsc::Receiver<AgentEvent>)`
  - 四个测试名与 spec §3 一致

- [ ] **Step 1: 写四条失败测试**

在 `crates/parrot-core/src/engine.rs` 的 `#[cfg(test)] mod tests` 内追加（保留已有 `context_budget_*` 测试）：

```rust
    use tempfile::TempDir;

    fn test_guard(dir: std::path::PathBuf) -> (AgentEndGuard, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let guard = AgentEndGuard {
            session_id: Uuid::new_v4(),
            event_log: EventLog::new(dir),
            event_tx: tx,
            fired: false,
            total_usage: Arc::new(Mutex::new(Usage::default())),
            reason: Arc::new(Mutex::new(AgentEndReason::ClientDisconnect)),
            hooks: Arc::new(HookRegistry::empty()),
            working_dir: std::path::PathBuf::from("."),
        };
        (guard, rx)
    }

    fn is_agent_end(ev: &PersistedAgentEvent) -> bool {
        matches!(ev.event, AgentEvent::AgentEnd { .. })
    }

    #[test]
    fn drop_persists_agent_end_when_not_fired() {
        let tmp = TempDir::new().expect("temp dir");
        let dir = tmp.path().to_path_buf();
        let (guard, _rx) = test_guard(dir.clone());
        drop(guard);

        let log = EventLog::new(dir);
        let events = log.replay().expect("replay");
        assert!(!events.is_empty(), "expected at least AgentEnd");
        assert!(is_agent_end(events.last().unwrap()), "last event must be AgentEnd: {:?}", events.last().unwrap().event);
    }

    #[test]
    fn drop_appends_after_existing_events_seq_continuous() {
        let tmp = TempDir::new().expect("temp dir");
        let dir = tmp.path().to_path_buf();
        let (mut guard, _rx) = test_guard(dir.clone());

        let start = AgentEvent::AgentStart {
            session_id: guard.session_id,
            model: "m".into(),
            provider: "p".into(),
            system_prompt_hash: String::new(),
            resumed_from_seq: None,
        };
        guard.event_log.append(start).expect("append start");
        let after_first = guard.event_log.replay().expect("replay mid");
        let prev_seq = after_first.last().unwrap().seq;

        drop(guard);

        let events = EventLog::new(dir).replay().expect("replay final");
        let last = events.last().unwrap();
        assert!(is_agent_end(last));
        assert_eq!(last.seq, prev_seq + 1, "AgentEnd seq must continue");
    }

    #[tokio::test]
    async fn fire_and_drop_then_drop_does_not_duplicate() {
        let tmp = TempDir::new().expect("temp dir");
        let dir = tmp.path().to_path_buf();
        let (guard, _rx) = test_guard(dir.clone());

        guard.fire_and_drop().await;
        // fire_and_drop 消耗 guard，返回时 Drop 见 fired=true 必须 no-op。

        let events = EventLog::new(dir).replay().expect("replay");
        let agent_ends = events.iter().filter(|e| is_agent_end(e)).count();
        assert_eq!(agent_ends, 1, "exactly one AgentEnd after fire_and_drop");
    }

    #[test]
    fn drop_persists_even_if_channel_closed() {
        let tmp = TempDir::new().expect("temp dir");
        let dir = tmp.path().to_path_buf();
        let (guard, rx) = test_guard(dir.clone());
        drop(rx); // 关闭接收端：try_send 必失败，落盘仍须成功

        drop(guard);

        let events = EventLog::new(dir).replay().expect("replay");
        assert!(is_agent_end(events.last().unwrap()));
    }
```

若 `PersistedAgentEvent` / `Usage` / `AgentEndReason` 未在 `mod tests` 作用域：`use super::*` 已含 engine 顶层导入；`PersistedAgentEvent` 在 `parrot_protocol::agent_event` — 若缺失则在 tests 内加：

```rust
    use parrot_protocol::agent_event::PersistedAgentEvent;
```

`#[tokio::test]`：`session.rs` 既有异步测试同 crate 可用；若 macro 不可用，改为 `tokio::runtime::Runtime::new().unwrap().block_on(...)` 包一层（workspace tokio 已启用 macros/rt 则优先 `#[tokio::test]`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p parrot-core --lib engine::tests`
Expected: FAIL — `drop_persists_*` 断言失败（当前 Drop 不写盘）或编译期字段已具备但行为未实现 → `last event` 不是 `AgentEnd` / `agent_ends == 0`。

- [ ] **Step 3: 实现 Drop 落盘**

替换整个 `impl Drop for AgentEndGuard` 为：

```rust
impl Drop for AgentEndGuard {
    fn drop(&mut self) {
        if self.fired {
            return;
        }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        let event = AgentEvent::AgentEnd {
            session_id: self.session_id,
            reason,
            total_usage: usage,
        };
        // 先落后发：resume 靠 events.log 区分正常结束与崩溃。
        // Drop 内禁止 panic：append 失败只 warn。
        if let Err(e) = self.event_log.append(event.clone()) {
            tracing::warn!(error = ?e, "failed to persist AgentEnd on drop");
        }
        let _ = self.event_tx.try_send(event);
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p parrot-core --lib engine::tests`
Expected: PASS（含四条新测试 + 原有 `context_budget_*`）

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Run: `cargo fmt --all -- --check`

```bash
git add crates/parrot-core/src/engine.rs
git commit -m "fix(core): persist AgentEnd on AgentEndGuard drop before try_send"
```

---

## Self-Review

**Spec coverage:**
- §2.1 结构体含 `event_log` → Task 1 Step 1 ✅
- §2.2 `run()` 尽早 move + `guard.event_log` + 末尾无参 `fire_and_drop` → Task 1 Steps 3-5 ✅
- §2.3 `fire_and_drop` 语义/顺序不变 → Task 1 Step 2 ✅
- §2.4 Drop 先落后发、warn 不 unwrap → Task 2 Step 3 ✅
- §2.5 不动 EventSink/半成品 TurnEnd → Global Constraints ✅
- §3 四条测试 → Task 2 Step 1 ✅
- §4 风险（reborrow/早构造/不 unwrap/回归）→ Task 1 Step 3-6、Task 2 Steps 3-4 ✅
- §5 验收 → 各 task Step「Lint + commit」+ `cargo test --workspace` ✅

**Placeholder scan:** 无 TBD/TODO；所有改码步骤含完整代码；无「Similar to Task N」。

**Type consistency:** `fire_and_drop(mut self)` 与调用点 `guard.fire_and_drop().await` 一致；测试构造字段名与 Task 1 结构体一致；`is_agent_end(&PersistedAgentEvent)` 与 `EventLog::replay()` 返回类型一致。

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-09-22-agent-end-guard-drop-persist.md`. Two execution options:

**1. Subagent-Driven (recommended)** — 每 task 派发 fresh subagent，task 间 review，快速迭代

**2. Inline Execution** — 本会话用 executing-plans 按批执行并设检查点

Which approach?
