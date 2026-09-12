# 运行时切换 model 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 会话在 turn 边界切换模型：`ClientMessage::Model` → `SessionCmd::SetModel` → 下一轮生效；`ServerMessage::ModelSet` 即时 ack。

**Architecture:** 协议两消息 + core SessionCmd 变体（turn 中 warn+忽略，与 Chat 同模式）+ daemon arm/meta 更新 + TUI `/model` 命令。

**Spec:** `docs/superpowers/specs/2026-09-12-runtime-model-switch-design.md`

## Global Constraints

- WS 消息 `#[serde(tag = "type")]`；新增消息必须同步 `crates/parrot-protocol/tests/roundtrip.rs`。
- turn 中收到 SetModel ⇒ warn + 忽略（对齐 SessionCmd::Chat 现有模式 engine.rs:544-548）。
- 客户端消息名：`Model`；服务端 ack：`ModelSet`；core 内部 `SessionCmd::SetModel`。
- parrot-core 零 IO 不变（meta 写入在 daemon）。
- 每任务结束 `cargo build --workspace` 绿；全量 gates：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
- 只动计划列出的文件。

---

### Task 1: protocol — Model / ModelSet + roundtrip

**Files:**
- Modify: `crates/parrot-protocol/src/client_message.rs`
- Modify: `crates/parrot-protocol/src/server_message.rs`
- Test: `crates/parrot-protocol/tests/roundtrip.rs`

**Interfaces:**
- Produces: `ClientMessage::Model { session_id: SessionId, model: String }`；`ServerMessage::ModelSet { session_id: SessionId, model: String }`（带 `SessionId` 的消息应与 ModelSet 的响应形态一致——参考 MCP 消息的 session_id 携带方式）

**Steps:**
1. roundtrip 失败测试（两条消息 roundtrip + serde tag 断言）
2. 确认失败 → 实现（两 enum 各加变体；ServerMessage 需要注意现有 exhaustive match 的 ripple——`crates/parrot-daemon/src/runtime.rs` 与 `src/cli` 的处理点，先加最小 stub arm 保编译）
3. gates + commit `feat(protocol): Model/ModelSet 运行时切换模型消息`

---

### Task 2: core — SessionCmd::SetModel + 会话任务处理

**Files:**
- Modify: `crates/parrot-core/src/session.rs`（SessionCmd + 会话任务 loop）
- Modify: `crates/parrot-core/src/engine.rs`（mid-stream select! 加 SetModel arm：warn+忽略，对齐 Chat 分支）
- Test: `crates/parrot-core/tests/react_loop.rs` 或新集成测试

**Interfaces:**
- Consumes: `SessionCmd`（现 `{Chat, Abort}`）
- Produces: `SessionCmd::SetModel { model: String }`；会话任务在 turn 边界处理它（更新 gen_config.model；其余字段保留）；mid-turn warn+忽略

**Steps:**
1. 失败测试：会话任务收到 SetModel 后下一轮 provider 收到新 model id（mock provider 断言收到的 model 参数）；mid-turn 收到不影响当前轮
2. 实现：SessionCmd 变体 + 会话 loop 处理（turn 边界更新；engine 的 select! 加 arm）
3. gates + commit `feat(core): SessionCmd::SetModel turn 边界切换模型`

---

### Task 3: daemon — runtime arm + meta 更新

**Files:**
- Modify: `crates/parrot-daemon/src/runtime.rs`
- Modify: `crates/parrot-daemon/src/session_store.rs`（如需 meta 写入辅助）

**Interfaces:**
- Consumes: Task 1/2 的消息与命令
- Produces: `ClientMessage::Model` arm：会话不存在 ⇒ `Error{SessionNotFound}`；`model` 为空串 ⇒ Error；否则转发 SetModel + 回 `ModelSet` + 更新 meta.json model/provider

**Steps:**
1. 失败测试（e2e_test.rs 模式）：Model 消息 → ModelSet 响应；空 model → Error；未知 session → Error
2. 实现（session_store 若已有 update 类函数则复用，否则加 `update_model`）
3. gates + commit `feat(daemon): Model 消息接线与 meta 更新`

---

### Task 4: TUI — /model 命令

**Files:**
- Modify: `src/tui/slash.rs`（registry + arg 解析 + action）
- Modify: `src/tui/app.rs`（`ServerMessage::ModelSet` 处理 → Info 条目 + self.model 更新）
- Modify: `src/tui/mod.rs`（handle_command 分发）

**Interfaces:**
- Consumes: Task 1 协议
- Produces: `/model <name>`：无参 ⇒ Info 提示用法；有参 ⇒ 发送 Model 消息；收到 ModelSet ⇒ Info「模型已切换为 X」并更新状态栏 model

**Steps:**
1. 失败测试（slash 解析：有参/无参；app ModelSet→Info 条目）
2. 实现
3. gates + commit `feat(tui): /model 运行时切换模型`

---

### Task 5: 全量验证

1. `cargo test --workspace` / clippy / fmt 全绿
2. spec §2 逐条核对
3. AGENTS.md 架构速查如需补一行协议消息（遵循现有 MCP 消息的先例——检查该文件是否列消息；不强制）

---

### Final whole-branch review

分支段终审：协议 roundtrip 完整、mid-turn 忽略语义与 Chat 对齐、meta 更新原子性、TUI 状态一致性（self.model 更新来源唯一）。
