# AgentEndGuard Drop 持久化设计

| 项 | 值 |
|---|---|
| 日期 | 2026-09-22 |
| 范围 | 仅 `crates/parrot-core/src/engine.rs`（+ 测试） |
| 批次 | 批次 A（已收窄）：只做 Drop 持久化 |
| 明确不做 | engine.rs 拆分、EventSink、EventsAfterAgentEnd 测试、半成品 TurnEnd |

## 1. 问题

`AgentEndGuard` 正常路径经 `fire_and_drop` 先落盘再发 `AgentEnd`；**panic / task abort** 走 `Drop` 时仅 `event_tx.try_send`，通道满或无消费者则事件丢失 → `events.log` 无 `AgentEnd`，resume 难以区分「已正常结束」与「崩溃中断」。

根因：`Drop::drop(&self)` 不能临时收参；构造时 guard 未持有 `EventLog` / `data_dir` / 活跃 seq。

相关代码：

- `crates/parrot-core/src/engine.rs` — `AgentEndGuard`、`fire_and_drop`、`impl Drop`
- `crates/parrot-core/src/event_log.rs` — `EventLog::append`（`PersistedAgentEvent` + seq/ts）

## 2. 方案（Z：EventLog 移入 guard）

把 `EventLog` move 进 `AgentEndGuard`，Drop 用**同一实例、同一 `current_seq`** append。不引入 `Arc` 共享 seq，不扫描文件。

### 2.1 结构体

```rust
struct AgentEndGuard {
    session_id: Uuid,
    event_log: EventLog, // 新增：唯一所有者
    event_tx: mpsc::Sender<AgentEvent>,
    fired: bool,
    total_usage: Arc<Mutex<Usage>>,
    reason: Arc<Mutex<AgentEndReason>>,
    hooks: Arc<HookRegistry>,
    working_dir: std::path::PathBuf,
}
```

### 2.2 `run()` 接线

- `EventLog::new` / `with_start_seq` 完成后，将 `event_log` **move 进 guard**（尽早构造，缩短「已有 AgentStart、尚无 guard」窗口）。
- 循环内原 `&mut event_log` → `&mut guard.event_log`（或循环内 reborrow 局部绑定）。
- 末尾：`guard.fire_and_drop().await`（不再传 `event_log` 参数）。

### 2.3 `fire_and_drop(mut self)`（语义不变）

- 删除 `event_log` 参数；用 `self.event_log` 完成 append。
- 顺序保持：hooks → 落盘 → `event_tx.send` → `fired = true`。

### 2.4 `Drop`

```text
if self.fired { return; }
组装 AgentEvent::AgentEnd { session_id, reason, total_usage }  // 与现逻辑相同
let _ = self.event_log.append(event.clone());  // 失败 tracing::warn，禁止 unwrap
let _ = self.event_tx.try_send(event);         // 先落后发；尽力通知在线端
```

### 2.5 不变式

- 落盘格式与 `EventLog::append` 完全一致（`PersistedAgentEvent` + seq/ts）。
- Drop **不**执行 async hooks（与现状一致）。
- 不修改 `EventLog` 对外行为；不引入 EventSink / Arc seq / 文件扫描。
- 半成品 `TurnEnd` 仍由 `replay_for_resume` 截断处理，本设计不扩 scope。

## 3. 测试

tempdir（或仓库既有临时目录惯例）下：

| 用例 | 断言 |
|---|---|
| `drop_persists_agent_end_when_not_fired` | 直接 `drop(guard)` → `replay()` 尾条为 `AgentEnd` |
| `drop_appends_after_existing_events_seq_continuous` | 先 append 若干事件 → drop → 尾条 `seq == prev+1` |
| `fire_and_drop_then_drop_does_not_duplicate` | 显式 fire 后再 Drop → 仅一条 `AgentEnd` |
| `drop_persists_even_if_channel_closed` | `event_tx` 关闭 → 仍落盘成功 |

放置：`engine.rs` 的 `#[cfg(test)]` 模块（或紧邻的集成测试，与现有 engine 测试惯例一致）。

进程级 panic/abort 不强制 e2e：单测直接 `drop(guard)` 覆盖同一代码路径；`shutdown_all` 既有 abort 测试作回归。

## 4. 风险

| 风险 | 对策 |
|---|---|
| reborrow 与 `fire_and_drop` 取所有权冲突 | 循环内 borrow 结束后再 move；靠编译器 |
| guard 创建前 panic 仍无 AgentEnd | 尽早构造 guard；不承诺覆盖 AgentStart 之前 |
| Drop 内 IO 再 panic | append 结果不 unwrap，仅 `warn` |
| 与现有 resume/shutdown 测试回归 | 全量 `cargo test --workspace` |

## 5. 验收

- [ ] §3 新单测通过
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] resume / `fire_and_drop` / shutdown 相关既有测试无回归

## 6. 后续（不在本 spec）

- 批次 C：备忘录剩余 UI → 批次 B：Provider 抽象
- P2：EventLog 移出 core / EventSink（另开项目）
- engine.rs 拆分（已暂缓）
