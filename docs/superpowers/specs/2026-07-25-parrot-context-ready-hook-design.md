# Parrot `context_ready` Hook Point

> **实现状态：已完成**（commits `ec75701`..`290cc10`）。`HookPoints::CONTEXT_READY`、
> `HookEvent::ContextReady`、`HookAction::ReplaceContext`、`HookResult::ReplaceContext`
> 均已落地，`handle_turn` 中的 hook site 已接入。

> 新增第 7 个 hook point `context_ready`，给 hook 在 turn 开始后、第一次 LLM 调用前
> 对话级修改/替换上下文的能力。同时新增一个 `HookAction::ReplaceContext` 让 hook
> 整体替换 context。
>
> 本 spec 建立在 #1 spec（`2026-07-25-parrot-hooks-crate-and-shell-denylist-design.md`）与
> 原始 lifecycle-hooks spec 之上：6 个现有 point 不改，只新增第 7 个；`HookAction` /
> `HookResult` / `HookPoints` / `HookEvent` 的现有枚举与 `HookRegistry::run` 的 waterfall
> 语义都不改，只 append 新 variant。

## 1. 背景与目标

现有 6 个 point 覆盖 agent 与 tool 周期：`agent_start` / `agent_end` / `turn_start` /
`tool_call` / `tool_execution_start` / `tool_result`。`turn_start` hook 能 inject 消息
（`InjectMessages` → `context.extend(messages)`），但不能：

- **替换**整个 context（最常见用例：把 system + history 收敛成 hook 产出的「精简上下文」，
  避免历史膨胀/捞出无关消息）；
- 在 **user message 已 push、context manager 已 prune** 之后看到完整的、即将发给 LLM 的
  context，做基于现状的最后一刻调整（如根据 context 长度决定是否压缩、注入「上一次任务
  结果摘要」）。

`context_ready` 闭合这两个缺口。它是 turn-level 触发（**每个 turn 第一次 LLM 调用前**，
非每个 react iteration）——避免 hook 在工具循环里反复触发，保证一次 turn 一次决策。

本轮目标：

1. 新增 `HookEvent::ContextReady` 携带 `context: &[ChatMessage]`（read-only），加入
   `HookPoints::CONTEXT_READY`，使 `Hook::supported()` 可声明支持。
2. 新增 `HookAction::ReplaceContext { messages: Vec<ChatMessage> }`，对应瀑布末端「整体
   替换 context」语义。
3. `HookResult` 新增对应聚合体 `ReplaceContext { hook_id, messages }`。
4. 引擎在 `handle_turn` 中**首次 prune 后、首次 `stream_llm_message` 前**触发
   `context_ready`，根据 `HookResult` 应用替换 / 注入。
5. 明确 resume 不变式与已知限制（§7），让 hook 作者能审慎使用。

## 2. 设计原则

1. **append-only**：现有 6 point / 4 HookAction / 4 HookResult 全部保持语义不变。
   本轮只新增 variant 与新触发站点。`enabled` 默认列表（#1 spec §8 矩阵）不强制加
   `context_ready` 类 hook（IDA：本轮连内置 hook 都不提供，只是开放扩展点）。
2. **turn-level，非 iteration-level**：每个 turn 跑一次 `context_ready`，不进 react 循环
   重跑。理由：react 循环中 context 在每轮 push assistant/tool 消息后递增，但「是否要在
   tool 调用链中途替换 context」是另一类问题（且复杂得多——替换会冲突已 push 的 tool_call
   id 序列）。本轮不给；若后续有需求再单独 spec。
3. **read-only event**：`HookEvent::ContextReady` 给 hook 看 `&[ChatMessage]`，hook **不能
   原地改 context**——只能通过返回 action 表达意图。框架集中处理 action，避免 hook 副作用
   不可见。
4. **限制写在台面上**：waterfall + 独立决策是现有架构，`ReplaceContext` 与 `InjectMessages`
   的混合顺序有固有矛盾（§7），spec 不藏——直接写明 limitation，让 author 自觉选策略。

## 3. 新增 `HookEvent::ContextReady`

```rust
// crates/parrot-core/src/hooks.rs
pub enum HookEvent<'a> {
    // ... 既有 6 个 variant 不变 ...
    ContextReady {
        session_id: Uuid,
        turn_id: Uuid,
        /// Engine 当前即将给 LLM 看的 context（已 prune、已含 user message）。
        /// **read-only**：hook 不可原地修改；意图通过返回 `HookAction` 表达。
        context: &'a [ChatMessage],
    },
}
```

`HookEvent::kind()` 返回 `"context_ready"`；`point()` 返回 `HookPoints::CONTEXT_READY`；
`session_id()` 返回 `*session_id`。三个 impl 都补一行 match arm 即可。

`HookPoints` 加 bit flag（保持 bitflags `HookPoints`）：

```rust
bitflags::bitflags! {
    pub struct HookPoints: u32 {
        const AGENT_START            = 1 << 0;
        const AGENT_END             = 1 << 1;
        const TURN_START            = 1 << 2;
        const TOOL_CALL             = 1 << 3;
        const TOOL_EXECUTION_START  = 1 << 4;
        const TOOL_RESULT           = 1 << 5;
        const CONTEXT_READY         = 1 << 6;   // 新增
    }
}
```

## 4. 新增 `HookAction::ReplaceContext`

```rust
// crates/parrot-core/src/hooks.rs
pub enum HookAction {
    NoOp,
    InjectMessages { messages: Vec<ChatMessage> },  // 既有
    Block { reason: String },                         // 既有
    ReplaceResult { content: String, is_error: bool }, // 既有
    /// **整体替换 context**。在此 turn 剩余生命周期内，engine 使用 `messages` 取代
    /// 所有现有 context（含 system、历史、user message）。
    /// waterfall 语义：last write wins（多个 hook 都返回 `ReplaceContext` 时，
    /// 最后一个返回的 messages 成为生效 context）。
    /// **注意**：返回 `ReplaceContext` 的 hook 无法抑制其他 hook 的 `InjectMessages`，
    /// 见 §7 限制。
    ReplaceContext { messages: Vec<ChatMessage> },
}
```

serde 标签：`#[serde(tag = "kind", rename_all = "snake_case")]` 已包裹整个 enum，
自动给出 `"replace_context"` tag。需在 `crates/parrot-protocol/tests/roundtrip.rs`
（或对应 hook test）加 roundtrip 用例。

## 5. 新增 `HookResult::ReplaceContext`

```rust
// crates/parrot-core/src/hooks.rs
pub enum HookResult {
    Continue,                              // 既有
    Block { hook_id: String, reason: String },  // 既有
    Inject { messages: Vec<ChatMessage> }, // 既有
    Replace { hook_id: String, content: String, is_error: bool }, // 既有
    /// 一个或多个 `context_ready` hook 返回 `ReplaceContext`（last write wins）。
    /// `hook_id` 是最后一个返回 `ReplaceContext` 的 hook。
    /// Engine 据此一次性把 `context` 替换为 `messages`，其后若还有 `Inject`
    /// 累积消息，会 extend 到替换后的 context 末尾（见 §6 apply order）。
    ReplaceContext { hook_id: String, messages: Vec<ChatMessage> },
}
```

## 6. `HookRegistry::run` 聚合：waterfall + apply order

`run()` 既有的 waterfall 聚合：`Block` bail、`InjectMessages` accumulate、
`ReplaceResult` last-wins。新增 `ReplaceContext` 同样「last-wins」，累加给
`replace_ctx_last: Option<(String, Vec<ChatMessage>)>`。

修改后的聚合伪代码（非完整源码）：

```rust
let mut inject_acc: Vec<ChatMessage> = Vec::new();
let mut replace_last: Option<(String, String, bool)> = None;       // tool_result
let mut replace_ctx_last: Option<(String, Vec<ChatMessage>)> = None; // context_ready

for hook in hooks {
    let outcome = self.call_with_timeout(&hook, event, &ctx).await;
    match &outcome {
        Ok(HookAction::Block { reason }) => { /* emit; return Block */ }
        Ok(HookAction::InjectMessages { messages }) => {
            emit(..);
            inject_acc.extend(messages.clone());
        }
        Ok(HookAction::ReplaceResult { content, is_error }) => {
            emit(..);
            replace_last = Some((hook.id().into(), content.clone(), *is_error));
        }
        Ok(HookAction::ReplaceContext { messages }) => {
            emit(hook.id(), kind, "replace_context",
                 Some(format!("{} messages", messages.len())));
            replace_ctx_last = Some((hook.id().into(), messages.clone()));
        }
        Ok(HookAction::NoOp) => emit(..),
        Err(..) => emit(..),
    }
}

// 优先级（与既有 run() 一致）：Block >> ReplaceResult >> ReplaceContext >> Inject >> Continue。
// 关键：同一次 run() 中若同时出现 ReplaceContext 与 InjectMessages，
// ReplaceContext 胜出、Inject 被 **静默丢弃**——这与既有 tool_result 的
// ReplaceResult >> Inject 行为对齐（见 hooks.rs:263-274 既有源码）。
// 语义合理：一个返回 ReplaceContext 的 hook 想做「整体 context 重置」，
// 自然不希望被其它 hook 的 inject 又塞回来。
if let Some((hook_id, content, is_error)) = replace_last {
    return HookResult::Replace { hook_id, content, is_error };
}
if let Some((hook_id, messages)) = replace_ctx_last {
    return HookResult::ReplaceContext { hook_id, messages };
}
if !inject_acc.is_empty() {
    return HookResult::Inject { messages: inject_acc };
}
HookResult::Continue
```

**注意**：`ReplaceResult`（tool_result 点）与 `ReplaceContext`（context_ready 点）**不同点
触发**，运行时一次 `run()` 调用里 `event.point()` 固定、`interested()` 只返回声明支持该
point 的 hook，正常用法下二者不会同时非空。即便有 hook 误用（在 context_ready 点返回
`ReplaceResult`），engine 的 site match 也只处理对应 `HookResult` variant，多余分支被
`_ => {}` 兜底——无副作用。

### 6.1 Engine 应用 `HookResult`

`handle_turn` 改动（伪代码示意，commit 时按实际代码结构落）：

```rust
// 现有：context.push(user_msg) + for _ in 0..MAX_REACT_ITERATIONS { prune; stream_llm; ... }
context.push(ChatMessage { role: User, content: user_msg.to_string(), .. });

let tool_defs = self.tool_registry.list_definitions().await;

// === 新站点：仅在第 0 次 iteration 之前跑一次 context_ready ===
{
    context_manager.prune(context);
    let outcome = self.hooks.run(
        HookEvent::ContextReady { session_id, turn_id, context: context },
        &self.working_dir, &mut emit).await;
    match outcome {
        HookResult::Block { reason, .. } => {
            // 等价 turn_start Block：发 TurnEnd{BlockedHook} 并 return turn
            return Ok((TurnStopReason::BlockedHook(reason), Usage::default()));
        }
        HookResult::ReplaceContext { messages, .. } => {
            *context = messages;
            context_manager.prune(context); // 替换后再次 prune，保 token 上限不爆
        }
        HookResult::Inject { messages } => {
            context.extend(messages);
            context_manager.prune(context);
        }
        // Continue / Replace（误用时的 fallthrough）→ 不动 context
        HookResult::Continue | HookResult::Replace { .. } => {}
    }
}

for _ in 0..MAX_REACT_ITERATIONS {
    context_manager.prune(context);
    let msg = self.stream_llm_message(turn_id, context, &tool_defs, event_tx, event_log, cmd_rx).await?;
    // ... 既有逻辑不变 ...
}
```

**关键点**：

1. **触发一次**——context_ready hook 站点在 `for` 循环之外，per turn 触发一次。
2. **应用顺序：互斥，非串联**。`HookRegistry::run` 一次调用只返回一个 `HookResult`
   variant（见 §6 优先级）。`ReplaceContext` 与 `Inject` 互斥——若同一次 run 里同时出现，
   `ReplaceContext` 胜出、`Inject` 静默丢弃（与既有 tool_result 的 `Replace >> Inject`
   行为对齐）。engine 不会「先 replace 再 append」——match 只命中一个 arm。
3. **替换后再次 prune**：换上来的 context 可能超 token 上限或多于保留长度，再 prune 一次
   保证不变式。注意 prune 可能裁掉 hook 想保留的早期消息——hook 应自己控制 messages 大小。
4. **`replace_ctx_last` 与 `replace_last` 不同点**：tool_result hook 不会出现在
   context_ready 点的 `hooks.iter()` 里，正常用法下二者不会同时非空。

## 7. 已知限制与权衡

1. **`ReplaceContext` 静默丢弃同次 run 的 `InjectMessages`**：waterfall 架构下
   `HookRegistry::run` 一次调用只返回一个 `HookResult` variant，优先级
   `Block >> ReplaceResult >> ReplaceContext >> Inject >> Continue`。若一次
   context_ready run 里既有 hook 返回 `ReplaceContext` 又有 hook 返回
   `InjectMessages`，`ReplaceContext` 胜出、`Inject` 被丢弃。这与既有 tool_result 的
   `ReplaceResult >> Inject` 行为对齐（hooks.rs:263-274）。**实践建议**：在
   `[hooks].enabled` 里同一 turn 不要把返回 `ReplaceContext` 的 hook 和返回
   `InjectMessages` 的 hook 同时启用——选一种策略。若 author 确需「整体替换 + 追加少量
   消息」，应让 `ReplaceContext` 的 `messages` 自己包含那部分内容（单一 hook 完成）。

2. **resume 不变式（重要）**：
   - context_ready 每个 turn 触发，**含 resume turn**。resume 时 context 从
     `event_log` 最后一次 `maybe_snapshot(context)` 重建（见既有 engine line 359，
     snapshot 发生在 turn 结束且包含 ReplaceContext 已被应用的 context）。
   - 因此 resume 时 hook 看到的 context 已是上次 turn 被替换过的状态，hook 需在 builder
     里实现自幂等：对同一会话连续触发 `context_ready` 不应导致 context 无限膨胀
     （例如 ReplaceContext 每次返回相同的精简版 → OK；InjectMessages 每次 push 同样的
     摘要 → resume 后 context 会因 baseline 里已有摘要 + 又 inject 一份而出现重复）。
   - 框架**不负责**去重或溯源 hook 产出消息，只做 append/replace。文档须明确提示这一点。

3. **`ReplaceContext` 替换会丢弃 system prompt**：engine 启动时把 `system_prompt` 作为
   context[0] 注入（engine line 122-133），`ReplaceContext` 是「全部替换」——若 hook 返回
   的 messages 不含 system prompt，LLM 调用就不带 system prompt。若需要继续带 system，
   hook 应在 messages[0] 重建 system 消息。**不提供「只换 user/history、保 system」的半
   替换 variant**——简单优先，复杂场景由 hook 自行拼装。

4. **`context_ready` 与 `turn_start` 的分工**：`turn_start` 在 user message push 之前触发
   （engine line 203-227，inject 的消息排在 user message 之前）；`context_ready` 在 user
   message push 之后、prune 之后触发。两个站点都允许 `InjectMessages`：
   - `turn_start.Inject` 的消息出现在 **user message 之前**（既有行为不变）。
   - `context_ready.Inject` 的消息出现在 **user message 之后**（context 尾）。
   - 同一启用列表里同时用两个站点 inject 是允许的，顺序由站点决定。
   - 只在 `context_ready` 点能拿 `&context` 看完整即将提交的内容；`turn_start` 不持有
     context 引用（既有 event 只有 `user_message: &str`），不打算改。

5. **不做 read-only 强制 barrier**：`context: &'a [ChatMessage]` 只是 Rust 借用规则上
   read-only，hook 仍可能 `clone` 后自造假 context 再返回——但框架只接收 `ReplaceContext`
   的 Vec，不接受 hook 改动原 slice。这一约束由 type system 保证。

6. **`ReplaceContext` 不带 `drop_injects` / 优先级字段**：刻意不引入。default 行为
   （同次 run 里 `ReplaceContext >> Inject`，inject 被丢）已覆盖 `drop_injects=true`
   的语义；想做 `drop_injects=false`（先 replace 再 append）需重构 `run()` 返回值类型
   从单 variant enum 变成多字段 struct，超出本轮范围。本轮先开放「互斥优先级」一条路径，
   user 实践若反馈「先 replace 再 append」是普遍需求，下一轮 spec 再加。

## 8. 测试

### 8.1 单元（`crates/parrot-core/tests/hooks_test.rs` 或新文件）

1. `context_ready_inject_appends_after_user_msg`：注册一个 hook 在 context_ready 点
   返回 `InjectMessages([summary])`；engine 跑一个空 turn；断言最终的 `stream_llm_message`
   被调用时 context 末尾是 `summary`（在 user message 之后）。
2. `context_ready_replace_swaps_wholesale`：注册一个 hook 返回
   `ReplaceContext { messages: [精简版...] }`；断言传给 provider 的 context 恰为精简版
   （不含原始 system / user，除非 hook 自己加回）。
3. `context_ready_block_returns_blocked_hook`：注册一个 hook 返回
   `Block { reason }`；断言 `TurnEnd` 的 `stop_reason == BlockedHook(reason)`，
   资源 `Usage::default()`，turn 不进入 react 循环。
4. `context_ready_fires_once_per_turn`：在一个 turn 多个 react iteration 的场景（mock
   provider 第一轮返回 tool_call、第二轮 end_turn）下，断言 hook 只被调用 1 次
   （而非每次 iteration）。
5. `context_ready_last_write_wins`：注册 H1、H2 都返回 `ReplaceContext`，验证 engine
   应用的是 H2 的 messages（last wins）。
6. `context_ready_replace_drops_concurrent_inject`：注册 H1 返回
   `InjectMessages([a])`、H2 返回 `ReplaceContext([b1, b2])`（按 enabled 顺序）；
   断言最终 context 恰为 `[b1, b2]`，inject `[a]` 被静默丢弃（验证 §6.1 apply order 与
   `ReplaceContext >> Inject` 优先级）。
7. `context_ready_noop_when_continue`：注册的 hook 都返回 `NoOp`；engine 行为与没注册
   hook 完全一样（context 不变）。
8. `context_ready_serialize_roundtrip`：`HookAction::ReplaceContext` 与
   `HookResult::ReplaceContext` 的 JSON roundtrip 通过；kind = `"replace_context"`。

### 8.2 config 解析

`context_ready` 本身只是 hook point，不需要新增 config 字段。本轮不提供内置 hook，
故无需 `[hooks.<id>]` 子表测试。仅附带验证现有 `HooksConfig` 反序列化时
`enabled` 列表允许任意字符串（hook point 名不强制白名单）——既有 #1 spec 已覆盖。

### 8.3 e2e（可选）

`e2e_context_ready_injects_summary`：mock provider 在第 0 轮 stream 前 engine 已
extend 一条上下文摘要。可用现有 e2e 模板（参考 `e2e_hook_blocks_tool_call`）。
**不强制**——单测覆盖了核心路径，e2e 由计划阶段决定是否加。

## 9. 实施步骤（高层）

1. `crates/parrot-core/src/hooks.rs`：
   - `HookEvent::ContextReady` 加，三个 impl 补 arm。
   - `HookPoints::CONTEXT_READY = 1 << 6` 加。
   - `HookAction::ReplaceContext { messages }` 加。
   - `HookResult::ReplaceContext { hook_id, messages }` 加。
   - `HookRegistry::run` 加 `replace_ctx_last` 累加路径，瀑布尾按 §6 优先级 return。
2. `crates/parrot-core/src/engine.rs`：
   - `handle_turn` 在 `context.push(user_msg)` 后、`for` 之前插入 context_ready hook
     run 站点 + 处理 `ReplaceContext` / `Inject` / `Block`（伪代码见 §6.1）。
3. `crates/parrot-protocol/tests/roundtrip.rs`：新增 `HookAction::ReplaceContext` /
   `HookResult::ReplaceContext` 的 JSON roundtrip 用例。
4. 写 8 个单测（§8.1）。
5. （可选）写 e2e（§8.3）。
6. `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` 全绿。
7. 同步更新 design doc §3 StreamEvent/ServerMessage 表（如有）与
   「6 hook points」→「7 hook points」表述，并在 `parrot-lifecycle-hooks-design.md`
   补 context_ready 行。

## 10. 不在范围

- **react iteration 内的 context 修改**：本轮只在 turn 第一次 stream 前触发，不在每次
  iteration 后触发。若需要 mid-turn 介入，单独另起 spec。
- **内置 `context_ready` hook**：本轮只开放 point + action，不内置任何 hook（与 #1
  `shell_denylist` 不同、与 #2 `redact_secrets` 扩展不同——context_ready 是扩展点，不
  是即装即用功能）。后续若有「自动摘要」「memory 注入」「context budget 压缩」等内置
  hook 需求，再分别起 spec。
- **`drop_injects` / 优先级元数据**：见 §7.6，刻意不开。
- **`context_ready` 的 `Replace` 半级（仅替历史、保 system）**：见 §7.3，不开。
- **hook 看到其它 hook 的输出**：waterfall 限制，不开（既有架构原则）。
- **resume 时跳过 context_ready 触发**：见 §7.2，每个 turn 仍触发，hook 自幂等。