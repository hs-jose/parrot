# Parrot — 统一 AgentEvent 事件模型

> 重构 `parrot-protocol` 的事件类型，把当前 `StreamEvent` + `EventLogEntry` + `ServerMessage` 中重复的流式语义合并为单一的、分层的 `AgentEvent`。同时为 Agent → Turn → Message/Tool 三层生命周期提供显式的 `*Start` / `*End` 边界事件。
>
> 本文档为问题 1（生命周期不明确）和问题 4（数据模型拆分）的合并详设。问题 2（crate 拆分）和问题 3（命名统一）在本文档落地后单独处理。

---

## 1. 范围

| 子项 | 目标 | 状态 |
|------|------|------|
| 协议：新增 `AgentEvent` | 分层、嵌套、父子 id 关联的统一事件类型 | ⏳ |
| 协议：`ServerMessage` 瘦身 | 流式相关变体全部折叠为 `AgentEvent` envelope | ⏳ |
| 协议：`EventLogEntry` 替换为 `AgentEvent` | 持久化用同一类型，按 `is_persistent()` 过滤 deltas | ⏳ |
| 引擎：发 `AgentEvent` | `ReActEngine` 替换 `mpsc::Sender<StreamEvent>` 为 `mpsc::Sender<AgentEvent>` | ⏳ |
| 引擎：补齐边界事件 | 显式 emit `AgentStart` / `AgentEnd` / `TurnStart` / `TurnEnd` / `MessageStart` / `MessageEnd` | ⏳ |
| Provider：`ChatStream` 内部事件改名 | provider 适配器吐出的子事件改名为 `ProviderStreamEvent`，仅在 engine 内消费 | ⏳ |
| Daemon：删 `SessionAdapter` | 翻译层归零 | ⏳ |
| Resume：完整性检测 + 截断 + 三重告警 | tracing + ReplayIntegrityWarning + corrupted.log | ⏳ |
| 迁移：旧 `events.log` 转新格式 | 一次性 migration 工具（也可单文件懒迁移） | ⏳ |
| 测试：roundtrip + e2e | 协议序列化 + 引擎事件序列断言 | ⏳ |

> ⏳ = 待实施。完成本批后回填 ✅。

---

## 2. 类型分层

### 2.1 三个独立类型，职责分明

| 类型 | 出现位置 | 何时引入 |
|------|---------|---------|
| `ProviderStreamEvent` | Provider 适配器 ↔ Engine 内部 | provider.rs（替换当前 `crate::event_log::StreamEvent`） |
| `AgentEvent` | Engine → Daemon → Client、Engine → EventLog 文件 | 新增于 `parrot-protocol::agent_event` |
| `ServerMessage` | Daemon ↔ Client wire 协议 | 现有，瘦身后只保留请求-响应 + `AgentEvent` envelope |

为什么不再把 provider 那层也合并进 `AgentEvent`：provider 适配器没有 turn / message lifecycle 的概念（它只懂 SSE 帧），强行套用会让"哪一层负责发 `MessageStart`"变模糊。Provider 只负责把 SSE 翻译成"text delta / tool-call delta / finish"，Engine 在外层补 lifecycle envelope。

### 2.2 引擎事件流改造前后对比

**改造前**（现状）：

```
Provider SSE → mpsc::Receiver<StreamEvent>
              ↓
            Engine 聚合 + 调工具，再往外发同一个 StreamEvent 类型
              ↓
            ┌─→ event_tx: mpsc::Sender<StreamEvent>
            │       → SessionAdapter::stream_event_to_server (50 行翻译)
            │       → ServerMessage::* (语义重复的变体)
            │       → WS → Client
            │
            └─→ event_log.append(EventLogEntry::*) (又一份语义重复)
                    → events.log
```

**改造后**：

```
Provider SSE → mpsc::Receiver<ProviderStreamEvent>
              ↓
            Engine 聚合 + 调工具 + 补 lifecycle envelope
              ↓
            mpsc::Sender<AgentEvent>
              ↓
            ┌─→ Daemon → ServerMessage::AgentEvent(ev) → WS → Client
            │
            └─→ if ev.is_persistent() { event_log.append(ev) }
                    → events.log (单一类型)
```

---

## 3. `AgentEvent` 完整定义

放在 `crates/parrot-protocol/src/agent_event.rs`：

```rust
use crate::types::{SessionId, ToolOutput, Usage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 统一的 Agent 事件流。
///
/// 三层生命周期，严格嵌套：
///
/// ```text
/// AgentStart
///   TurnStart
///     MessageStart
///       MessageDelta*    (高频，不持久化)
///     MessageEnd         (final_content 完整，供 replay 重建 context)
///     ToolStart          (parent_message_id 关联到上面的 message)
///       ToolUpdate*      (高频，不持久化，长任务进度)
///       ToolConfirmRequired? (可选，仅当工具命中 require_confirmation)
///     ToolEnd
///     // (ToolEnd 后 ReAct 可能进入下一轮 MessageStart...MessageEnd)
///   TurnEnd
/// AgentEnd
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AgentEvent {
    // ============== Agent 层 (Session 生命周期) ==============

    /// Session 启动。Daemon 在 `CreateSession`/`ResumeSession` 后、引擎处理首个
    /// `Chat` 前由引擎 emit。`system_prompt_hash` 用 SHA-256 的前 16 字节 hex，
    /// 让客户端能识别 system prompt 变更而不传输完整文本。
    AgentStart {
        session_id: SessionId,
        model: String,
        provider: String,
        system_prompt_hash: String,
        resumed_from_seq: Option<u64>, // None=fresh; Some(n)=replay 到第 n 条后启动
    },

    /// Session 终止。引擎退出前必须 emit 一次（用 RAII guard 保证，见 §6.4）。
    AgentEnd {
        session_id: SessionId,
        reason: AgentEndReason,
        total_usage: Usage,
    },

    // ============== Turn 层 (一次 User → final Assistant) ==============

    /// 一个 turn 开始。`turn_id` 由引擎在收到 `SessionCmd::Chat` 时生成。
    /// `user_message` 是触发本 turn 的用户输入。
    TurnStart {
        session_id: SessionId,
        turn_id: Uuid,
        user_message: String,
    },

    /// 一个 turn 结束。
    ///
    /// - `EndTurn` 表示 LLM 自然结束
    /// - `MaxTokens` / `MaxIterations` 表示触发上限
    /// - `Aborted` 表示客户端 `Abort`
    ///
    /// 注意：`StopReason::ToolUse` **不会** 出现在 `TurnEnd` 中——它是
    /// `MessageEnd` 内的字段。一个 turn 的"结束"语义只有上面三种。
    TurnEnd {
        session_id: SessionId,
        turn_id: Uuid,
        stop_reason: TurnStopReason,
        usage: Usage,
    },

    // ============== Message 层 (单次 LLM 调用) ==============

    /// 单条 LLM 响应消息开始。`message_id` 由引擎生成，关联后续 deltas/end。
    MessageStart {
        session_id: SessionId,
        turn_id: Uuid,
        message_id: Uuid,
    },

    /// 流式增量。**不持久化**。`payload` 是子枚举，区分文本/工具调用增量。
    MessageDelta {
        session_id: SessionId,
        message_id: Uuid,
        payload: MessageDeltaPayload,
    },

    /// 单条 LLM 响应消息结束。`final_content` 是聚合后的完整快照，
    /// `tool_calls` 是这条消息里发起的所有工具调用元信息（id/name/args）。
    /// `stop_reason` 表示这次 LLM 调用因何结束（用 `ToolUse` 表示后续要执行
    /// tool；用 `EndTurn` 表示这是 final assistant message，turn 即将结束）。
    MessageEnd {
        session_id: SessionId,
        turn_id: Uuid,
        message_id: Uuid,
        final_content: String,
        tool_calls: Vec<ToolCallInfo>,
        stop_reason: MessageStopReason,
        usage: Usage,
    },

    // ============== Tool 层 (单次工具调用) ==============

    /// 工具开始执行。`parent_message_id` 指向是哪条 LLM message 发起的它。
    ToolStart {
        session_id: SessionId,
        turn_id: Uuid,
        parent_message_id: Uuid,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    /// 工具执行中的进度更新。**不持久化**。MVP 不发射，留作未来 long-running
    /// tool 接口扩展（如 shell_exec 实时回显行、web_fetch 字节数）。
    ToolUpdate {
        session_id: SessionId,
        tool_call_id: String,
        partial: ToolPartial,
    },

    /// 工具执行结束。结果含 is_error。
    ToolEnd {
        session_id: SessionId,
        turn_id: Uuid,
        tool_call_id: String,
        result: ToolOutput,
    },

    /// 工具需要客户端二次确认（命中 `require_confirmation`）。
    /// 在 `ToolStart` 之后、`ToolEnd` 之前出现一次。客户端的
    /// `ConfirmToolCall` 响应不属于本事件流——由 daemon 的 `ConfirmRouter` 处理。
    ToolConfirmRequired {
        session_id: SessionId,
        turn_id: Uuid,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },

    // ============== 系统层 (运维 / 完整性) ==============

    /// Resume 时检测到 events.log 完整性问题（半截 turn / AgentEnd 之后还有事件），
    /// daemon 已自动截断到最后一个完整 turn，并把丢弃的事件写入 corrupted.log。
    ///
    /// 在 `AgentStart` 之后、首个 `TurnStart` 之前 emit 且**只 emit 一次**。
    /// 本事件持久化到新的 events.log（标记"我们做过截断"），下次 resume 时
    /// 不会重复触发。详见 §7.3。
    ReplayIntegrityWarning {
        session_id: SessionId,
        issue: IntegrityIssue,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum MessageDeltaPayload {
    TextDelta { delta: String },
    ToolCallStart { tool_call_id: String, tool_name: String },
    ToolCallArgsDelta { tool_call_id: String, args_delta: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolPartial {
    pub kind: String,           // e.g. "stdout_line" / "bytes_downloaded"
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallInfo {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageStopReason {
    EndTurn,    // 这是 final assistant message
    ToolUse,    // 后面会有 ToolStart..ToolEnd
    MaxTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TurnStopReason {
    EndTurn,
    MaxTokens,
    MaxIterations,   // 取代当前 AgentError::Config("max iterations") 的字符串错误
    Aborted,
    Error(String),   // turn 级别的 fatal，e.g. provider error
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentEndReason {
    ClientClose,         // CloseSession 命令
    ClientDisconnect,    // WS 连接掉，session 自动清理
    DaemonShutdown,      // 进程退出
    FatalError(String),  // 不可恢复错误
}

impl AgentEvent {
    /// 是否需要追加到 `events.log`。增量事件 (MessageDelta / ToolUpdate) 是
    /// 流式 UI 用的，存盘只会让日志膨胀且对 replay 无价值——`MessageEnd` 的
    /// `final_content` 已经包含完整内容。
    pub fn is_persistent(&self) -> bool {
        !matches!(self, Self::MessageDelta { .. } | Self::ToolUpdate { .. })
    }

    /// 返回事件所属的 session_id（所有变体都有）。
    pub fn session_id(&self) -> SessionId {
        match self {
            Self::AgentStart { session_id, .. }
            | Self::AgentEnd { session_id, .. }
            | Self::TurnStart { session_id, .. }
            | Self::TurnEnd { session_id, .. }
            | Self::MessageStart { session_id, .. }
            | Self::MessageDelta { session_id, .. }
            | Self::MessageEnd { session_id, .. }
            | Self::ToolStart { session_id, .. }
            | Self::ToolUpdate { session_id, .. }
            | Self::ToolEnd { session_id, .. }
            | Self::ToolConfirmRequired { session_id, .. }
            | Self::ReplayIntegrityWarning { session_id, .. } => *session_id,
        }
    }
}
```

### 3.1 `EventLogEntryWithMeta` 演化

仍然保留外层包装来携带 `seq` + `ts`，但内层从 `EventLogEntry` 替换为 `AgentEvent`：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedAgentEvent {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub event: AgentEvent,
}
```

`EventLogEntry` 类型直接删除。`ServerMessage::History` 的 `entries` 字段类型从
`Vec<EventLogEntryWithMeta>` 改为 `Vec<PersistedAgentEvent>`。

### 3.2 `ProviderStreamEvent` 重命名

现在的 `parrot_core::event_log::StreamEvent` 改名移到 `parrot_core::provider::ProviderStreamEvent`：

```rust
pub enum ProviderStreamEvent {
    TextDelta { delta: String },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, args_delta: String },
    ToolCallEnd { id: String, arguments: serde_json::Value },
    Finish { stop_reason: ProviderStopReason, usage: Usage },
}

pub enum ProviderStopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}
```

注意：`ToolResult` 和 `ToolCallConfirmationRequired` **不再属于** provider stream——
前者是引擎自己产生（执行工具后），后者是引擎策略决定（命中 require_confirmation）。
当前 `StreamEvent::ToolResult` 和 `StreamEvent::ToolCallConfirmationRequired` 出现在
provider stream 类型里是历史遗留。

---

## 4. `ServerMessage` 瘦身

### 4.1 删除的变体

```rust
// 全部删除，统一改走 ServerMessage::AgentEvent(...)
ServerMessage::TextDelta { ... }
ServerMessage::ToolCallStart { ... }
ServerMessage::ToolCallDelta { ... }
ServerMessage::ToolCallEnd { ... }
ServerMessage::ToolResult { ... }
ServerMessage::Finished { ... }
ServerMessage::ToolCallConfirmationRequired { ... }
```

### 4.2 新增的变体

```rust
ServerMessage::AgentEvent(AgentEvent)  // envelope
```

### 4.3 完整改造后

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    // ── 握手 / 控制 ──
    HelloAck { server_version: String },

    // ── 请求-响应 ──
    SessionCreated { session_id: SessionId },
    SessionResumed { session_id: SessionId },
    SessionList    { sessions: Vec<SessionMeta> },
    ModelList      { models:   Vec<ModelInfo> },
    ToolList       { session_id: SessionId, tools: Vec<ToolDefinitionWire> },
    History        { session_id: SessionId, events: Vec<PersistedAgentEvent> },

    // ── 流式 / 推送 ──
    AgentEvent(AgentEvent),

    // ── 错误 ──
    Error { session_id: Option<SessionId>, code: ErrorCode, message: String },
}
```

`session_adapter.rs` 整个删除。Daemon 直接 `client.send(ServerMessage::AgentEvent(ev)).await`。

---

## 5. 引擎事件序列

### 5.1 一个完整 turn 的事件流（含工具）

下面是 `Chat { message: "read /a.txt then say hi" }` 的完整事件序列示例，
带工具调用且 LLM 第二轮直接结束：

```text
TurnStart           { turn_id: T1, user_message: "read /a.txt then say hi" }

  MessageStart      { message_id: M1, turn_id: T1 }
  MessageDelta      { message_id: M1, payload: TextDelta { delta: "I'll read" } }
  MessageDelta      { message_id: M1, payload: TextDelta { delta: " the file." } }
  MessageDelta      { message_id: M1, payload: ToolCallStart { tool_call_id: tc1, ... } }
  MessageDelta      { message_id: M1, payload: ToolCallArgsDelta { tool_call_id: tc1, ... } }
  MessageEnd        { message_id: M1, final_content: "I'll read the file.",
                      tool_calls: [{tc1, file_read, {path: /a.txt}}],
                      stop_reason: ToolUse, usage: {...} }

  ToolStart         { tool_call_id: tc1, tool_name: file_read,
                      arguments: {path: /a.txt}, parent_message_id: M1 }
  ToolEnd           { tool_call_id: tc1, result: {content: "hello world", is_error: false} }

  MessageStart      { message_id: M2, turn_id: T1 }
  MessageDelta      { message_id: M2, payload: TextDelta { delta: "hi" } }
  MessageEnd        { message_id: M2, final_content: "hi",
                      tool_calls: [],
                      stop_reason: EndTurn, usage: {...} }

TurnEnd             { turn_id: T1, stop_reason: EndTurn, usage: {total of T1} }
```

### 5.2 一个二次确认的工具流

```text
ToolStart           { tool_call_id: tc1, tool_name: shell_exec, ... }
ToolConfirmRequired { tool_call_id: tc1, tool_name: shell_exec, arguments: {...} }
// [客户端 ConfirmToolCall(Approve) 通过 ConfirmRouter 路由，不在事件流]
ToolEnd             { tool_call_id: tc1, result: ... }
```

如果客户端拒绝：

```text
ToolStart           { tool_call_id: tc1, ... }
ToolConfirmRequired { tool_call_id: tc1, ... }
ToolEnd             { tool_call_id: tc1, result: {content: "user rejected", is_error: true} }
```

### 5.3 Abort 的事件流

```text
TurnStart           { turn_id: T1, ... }
  MessageStart      { message_id: M1, ... }
  MessageDelta      { ... }
  // [SessionCmd::Abort 到达]
TurnEnd             { turn_id: T1, stop_reason: Aborted, usage: {...} }
```

注意 `MessageEnd` 在 abort 路径下 **可能不发**——因为 LLM 流还没结束就被打断了。
客户端 UI 在收到 `TurnEnd { stop_reason: Aborted }` 时应清理任何未关闭的
`MessageStart`/`ToolStart`。这是一个**有意的设计**：不强行 emit 不真实的
`MessageEnd`（final_content 是不完整的）；客户端用 turn 的边界事件做兜底。

---

## 6. 引擎改动

### 6.1 字段

```rust
pub struct ReActEngine {
    session_id: Uuid,
    // ... 既有 ...

    // 新增：保留 system_prompt 的 hash，AgentStart 时一次性 emit
    system_prompt_hash: String,

    // 新增：累加本 session 的 Usage（用于 AgentEnd.total_usage）
    // 注意是引擎私有状态，不暴露
}
```

### 6.2 `run()` 的事件骨架

```rust
pub async fn run(
    mut self,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    event_tx: mpsc::Sender<AgentEvent>,
) {
    let _ = event_tx.send(AgentEvent::AgentStart {
        session_id: self.session_id,
        model: self.config.model.clone(),
        provider: self.provider_id.clone(),
        system_prompt_hash: self.system_prompt_hash.clone(),
        resumed_from_seq: self.resumed_from_seq,
    }).await;

    let mut total_usage = Usage { input_tokens: 0, output_tokens: 0 };
    let mut end_reason = AgentEndReason::ClientClose;  // 默认值，下面按需覆盖

    loop {
        match cmd_rx.recv().await {
            Some(SessionCmd::Chat { message }) => {
                let turn_id = Uuid::new_v4();
                let _ = event_tx.send(AgentEvent::TurnStart {
                    session_id: self.session_id,
                    turn_id,
                    user_message: message.clone(),
                }).await;

                let turn_result = self.handle_turn(
                    turn_id, &message, &mut context, &context_manager,
                    &event_tx, &mut event_log, &mut cmd_rx,
                ).await;

                let (stop_reason, turn_usage) = match turn_result {
                    Ok((sr, usage)) => (sr, usage),
                    Err(AgentError::Aborted) => (TurnStopReason::Aborted, Usage::default()),
                    Err(e) => (TurnStopReason::Error(e.to_string()), Usage::default()),
                };

                total_usage.input_tokens  += turn_usage.input_tokens;
                total_usage.output_tokens += turn_usage.output_tokens;

                let _ = event_tx.send(AgentEvent::TurnEnd {
                    session_id: self.session_id,
                    turn_id,
                    stop_reason,
                    usage: turn_usage,
                }).await;
            }
            Some(SessionCmd::Abort) => {
                // 顶层 abort，无活跃 turn——忽略（abort 只在 turn 内有意义）
            }
            None => {
                end_reason = AgentEndReason::ClientDisconnect;
                break;
            }
        }
    }

    let _ = event_tx.send(AgentEvent::AgentEnd {
        session_id: self.session_id,
        reason: end_reason,
        total_usage,
    }).await;
}
```

### 6.3 `handle_turn` (旧 `handle_chat`) 的事件骨架

```rust
async fn handle_turn(
    &self,
    turn_id: Uuid,
    user_msg: &str,
    context: &mut Vec<ChatMessage>,
    /* ... */
) -> Result<(TurnStopReason, Usage), AgentError> {
    // 1. push user message to context
    context.push(ChatMessage { role: User, content: user_msg.into(), .. });

    let mut turn_usage = Usage::default();

    for iter in 0..MAX_REACT_ITERATIONS {
        let message_id = Uuid::new_v4();
        let _ = event_tx.send(AgentEvent::MessageStart {
            session_id, turn_id, message_id
        }).await;

        // 2. open provider stream, consume ProviderStreamEvent
        let mut stream = provider.chat_stream(...).await?;
        let mut accumulated_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut msg_stop = MessageStopReason::EndTurn;
        let mut msg_usage = Usage::default();

        loop {
            tokio::select! {
                biased;
                Some(SessionCmd::Abort) = cmd_rx.recv() => {
                    drop(stream);
                    return Err(AgentError::Aborted);
                }
                ev = stream.inner.recv() => {
                    let Some(ev) = ev else { break };
                    match ev {
                        ProviderStreamEvent::TextDelta { delta } => {
                            accumulated_text.push_str(&delta);
                            let _ = event_tx.send(AgentEvent::MessageDelta {
                                session_id, message_id,
                                payload: MessageDeltaPayload::TextDelta { delta },
                            }).await;
                        }
                        ProviderStreamEvent::ToolCallStart { id, name } => {
                            tool_calls.push(PendingToolCall { id: id.clone(), name: name.clone(), arguments: String::new(), arguments_json: None });
                            let _ = event_tx.send(AgentEvent::MessageDelta {
                                session_id, message_id,
                                payload: MessageDeltaPayload::ToolCallStart { tool_call_id: id, tool_name: name },
                            }).await;
                        }
                        ProviderStreamEvent::ToolCallDelta { id, args_delta } => {
                            if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                                tc.arguments.push_str(&args_delta);
                            }
                            let _ = event_tx.send(AgentEvent::MessageDelta {
                                session_id, message_id,
                                payload: MessageDeltaPayload::ToolCallArgsDelta { tool_call_id: id, args_delta },
                            }).await;
                        }
                        ProviderStreamEvent::ToolCallEnd { id, arguments: _ } => {
                            // 解析 JSON（沿用现状）
                            if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == id) {
                                tc.arguments_json = Some(serde_json::from_str(&tc.arguments).unwrap_or_default());
                            }
                        }
                        ProviderStreamEvent::Finish { stop_reason, usage } => {
                            msg_stop = stop_reason.into();   // ProviderStopReason → MessageStopReason
                            msg_usage = usage;
                            break;
                        }
                    }
                }
            }
        }

        // 3. emit MessageEnd with final snapshot
        let tool_calls_info: Vec<ToolCallInfo> = tool_calls.iter()
            .map(|tc| ToolCallInfo {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                arguments: tc.arguments_json.clone().unwrap_or_default(),
            })
            .collect();

        let _ = event_tx.send(AgentEvent::MessageEnd {
            session_id, turn_id, message_id,
            final_content: accumulated_text.clone(),
            tool_calls: tool_calls_info.clone(),
            stop_reason: msg_stop.clone(),
            usage: msg_usage.clone(),
        }).await;

        turn_usage.input_tokens  += msg_usage.input_tokens;
        turn_usage.output_tokens += msg_usage.output_tokens;

        // 4. push assistant message to context
        context.push(ChatMessage { role: Assistant, content: accumulated_text, ..with tool_calls });

        // 5. if no tool calls, turn ends
        if msg_stop == MessageStopReason::EndTurn || tool_calls.is_empty() {
            return Ok((TurnStopReason::EndTurn, turn_usage));
        }

        // 6. execute each tool: ToolStart → [ToolConfirmRequired] → ToolEnd
        for tc in &tool_calls {
            let _ = event_tx.send(AgentEvent::ToolStart {
                session_id, turn_id, parent_message_id: message_id,
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                arguments: tc.arguments_json.clone().unwrap_or_default(),
            }).await;

            let decision = if self.needs_confirm(&tc.name) {
                let _ = event_tx.send(AgentEvent::ToolConfirmRequired {
                    session_id, turn_id,
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments_json.clone().unwrap_or_default(),
                }).await;
                self.wait_confirm(...).await
            } else {
                ConfirmDecision::Approve
            };

            let result = match decision {
                ConfirmDecision::Approve => self.execute_tool(...).await
                    .unwrap_or_else(|e| ToolOutput { content: format!("Error: {}", e), is_error: true }),
                ConfirmDecision::Reject => ToolOutput { content: "user rejected".into(), is_error: true },
                ConfirmDecision::Timeout => ToolOutput { content: "confirmation timeout".into(), is_error: true },
            };

            let _ = event_tx.send(AgentEvent::ToolEnd {
                session_id, turn_id,
                tool_call_id: tc.id.clone(),
                result: result.clone(),
            }).await;

            context.push(ChatMessage { role: Tool, content: result.content, .. });
        }
    }

    Ok((TurnStopReason::MaxIterations, turn_usage))
}
```

### 6.4 `AgentEnd` 必发：RAII guard

`run()` 退出路径有三条（cmd_rx 关闭、panic、tokio task 被 abort），仅靠在每个
`break` 前手动 emit 容易漏。用 guard struct：

```rust
struct AgentEndGuard {
    session_id: SessionId,
    event_tx: mpsc::Sender<AgentEvent>,
    fired: bool,
    total_usage: Arc<Mutex<Usage>>,
    reason: Arc<Mutex<AgentEndReason>>,
}

impl Drop for AgentEndGuard {
    fn drop(&mut self) {
        if self.fired { return; }
        let usage = self.total_usage.lock().unwrap().clone();
        let reason = self.reason.lock().unwrap().clone();
        // try_send 是非阻塞，drop 路径下 channel 可能已关闭——best effort
        let _ = self.event_tx.try_send(AgentEvent::AgentEnd {
            session_id: self.session_id,
            reason,
            total_usage: usage,
        });
    }
}
```

正常退出时 `fired = true` 避免重发，异常退出时 drop 兜底。`Arc<Mutex<>>` 是因为
我们需要在循环里修改 total_usage 同时让 guard 读到——`std::sync::Mutex` 在
单线程 await 里不会阻塞。

---

## 7. 持久化与重放

### 7.1 events.log 文件格式

每行一个 JSON：

```jsonl
{"seq":0,"ts":"2026-06-22T10:00:00Z","type":"AgentStart","session_id":"...","model":"claude-sonnet-4-6","provider":"anthropic","system_prompt_hash":"a3f9...","resumed_from_seq":null}
{"seq":1,"ts":"2026-06-22T10:00:01Z","type":"TurnStart","session_id":"...","turn_id":"...","user_message":"hi"}
{"seq":2,"ts":"2026-06-22T10:00:01Z","type":"MessageStart","session_id":"...","turn_id":"...","message_id":"..."}
{"seq":3,"ts":"2026-06-22T10:00:02Z","type":"MessageEnd","session_id":"...","turn_id":"...","message_id":"...","final_content":"hello","tool_calls":[],"stop_reason":"EndTurn","usage":{...}}
{"seq":4,"ts":"2026-06-22T10:00:02Z","type":"TurnEnd","session_id":"...","turn_id":"...","stop_reason":"EndTurn","usage":{...}}
{"seq":5,"ts":"2026-06-22T10:00:05Z","type":"AgentEnd","session_id":"...","reason":"ClientClose","total_usage":{...}}
```

`MessageDelta` 和 `ToolUpdate` 不出现在文件里。

### 7.2 Replay → Context 重建算法

给一个 `Vec<PersistedAgentEvent>`（按 seq 排序），重建 `Vec<ChatMessage>`：

```rust
pub fn rebuild_context(events: &[PersistedAgentEvent]) -> Vec<ChatMessage> {
    let mut ctx = Vec::new();
    for ev in events {
        match &ev.event {
            AgentEvent::TurnStart { user_message, .. } => {
                ctx.push(ChatMessage { role: User, content: user_message.clone(), .. });
            }
            AgentEvent::MessageEnd { final_content, tool_calls, .. } => {
                ctx.push(ChatMessage {
                    role: Assistant,
                    content: final_content.clone(),
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls.clone().into_iter().map(Into::into).collect()) },
                    ..
                });
            }
            AgentEvent::ToolEnd { tool_call_id, result, .. } => {
                ctx.push(ChatMessage {
                    role: Tool,
                    content: result.content.clone(),
                    tool_call_id: Some(tool_call_id.clone()),
                    ..
                });
            }
            _ => {}  // Start/Confirm/Delta/AgentStart/AgentEnd/TurnEnd 都不入 context
        }
    }
    ctx
}
```

### 7.3 半截 turn 的检测、截断与告警

#### 7.3.1 何为"半截 turn"

正常 turn 在 events.log 中的形态是 `TurnStart ... TurnEnd` 配对出现。
异常情况会留下不配对的 `TurnStart`：

| 触发原因 | 留下的痕迹 |
|---------|----------|
| Daemon 进程被 `kill -9` 或 OS OOM 杀死 | 最后一个 `TurnStart` 之后没有 `TurnEnd` |
| Daemon 进程崩溃（panic 未处理） | 同上 |
| 写 events.log 时磁盘满 / 权限丢失 | `TurnEnd` 应该写但失败了，下次启动看到不配对 |
| `AgentEnd` 之后还有事件 | 表示进程在已声明 session 结束后仍写入——severe data corruption |

注意：**正常的 `AgentEnd { reason: ClientDisconnect }` 不算半截**，因为 turn 已正常闭合，
只是 session 结束。半截特指 turn 边界破损。

#### 7.3.2 截断算法

```rust
/// 把事件序列截到"最后一个完整 turn 的 TurnEnd 之后"。
/// 返回 (清理后的事件, 被丢弃的事件, 截断原因)。
///
/// 调用方负责把丢弃的事件写到 corrupted.log（见 §7.3.4），并 emit
/// AgentEvent::ReplayIntegrityWarning（见 §3，加入此变体）。
pub fn truncate_to_last_complete_turn(
    events: Vec<PersistedAgentEvent>,
) -> (Vec<PersistedAgentEvent>, Vec<PersistedAgentEvent>, Option<IntegrityIssue>) {
    // 1. 找到最后一个 TurnEnd 的 index。
    let last_turn_end_idx = events.iter().enumerate().rev()
        .find_map(|(i, ev)| matches!(&ev.event, AgentEvent::TurnEnd { .. }).then_some(i));

    // 2. 从尾部往前扫，记录是否有不配对的 TurnStart / 落单 MessageStart / 落单 ToolStart。
    let tail_start = last_turn_end_idx.map(|i| i + 1).unwrap_or(0);
    let tail = &events[tail_start..];

    if tail.is_empty() {
        return (events, Vec::new(), None);
    }

    let mut issue = IntegrityIssue {
        kind: IntegrityIssueKind::PartialTurn,
        dropped_event_count: tail.len() as u32,
        first_dropped_seq: tail.first().map(|e| e.seq).unwrap_or(0),
        last_dropped_seq: tail.last().map(|e| e.seq).unwrap_or(0),
        dangling_turn_ids: Vec::new(),
        dangling_message_ids: Vec::new(),
        dangling_tool_call_ids: Vec::new(),
    };

    for ev in tail {
        match &ev.event {
            AgentEvent::TurnStart { turn_id, .. } => issue.dangling_turn_ids.push(*turn_id),
            AgentEvent::MessageStart { message_id, .. } => issue.dangling_message_ids.push(*message_id),
            AgentEvent::ToolStart { tool_call_id, .. } => issue.dangling_tool_call_ids.push(tool_call_id.clone()),
            _ => {}
        }
    }

    // 3. 还要检查：如果文件存在 AgentEnd 但其后又有事件，那是 severe corruption。
    let agent_end_idx = events.iter().enumerate()
        .find_map(|(i, ev)| matches!(&ev.event, AgentEvent::AgentEnd { .. }).then_some(i));
    if let Some(end_idx) = agent_end_idx {
        if end_idx + 1 < events.len() {
            issue.kind = IntegrityIssueKind::EventsAfterAgentEnd;
            // 截到 AgentEnd 而非最后 TurnEnd
            let (keep, drop) = events.split_at(end_idx + 1);
            return (keep.to_vec(), drop.to_vec(), Some(issue));
        }
    }

    let (keep, drop) = events.split_at(tail_start);
    (keep.to_vec(), drop.to_vec(), Some(issue))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntegrityIssue {
    pub kind: IntegrityIssueKind,
    pub dropped_event_count: u32,
    pub first_dropped_seq: u64,
    pub last_dropped_seq: u64,
    pub dangling_turn_ids: Vec<Uuid>,
    pub dangling_message_ids: Vec<Uuid>,
    pub dangling_tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IntegrityIssueKind {
    /// 末尾有 TurnStart 但无 TurnEnd——最常见，daemon 崩溃留下的尾巴。
    PartialTurn,
    /// AgentEnd 之后还有事件——文件被并发写或回滚出错。
    EventsAfterAgentEnd,
}
```

#### 7.3.3 必发的三重日志反馈

每次截断发生，**必须**通过三条通道同时通知：

**1. tracing::warn! 写到 daemon 日志**

```rust
tracing::warn!(
    session_id = %session_id,
    kind = ?issue.kind,
    dropped = issue.dropped_event_count,
    first_seq = issue.first_dropped_seq,
    last_seq = issue.last_dropped_seq,
    dangling_turns = ?issue.dangling_turn_ids,
    dangling_messages = ?issue.dangling_message_ids,
    dangling_tools = ?issue.dangling_tool_call_ids,
    "events.log integrity issue detected — truncated to last complete turn"
);
```

字段全部用结构化日志（不是字符串拼接），方便日志系统聚合检索。

**2. AgentEvent::ReplayIntegrityWarning 发到客户端**

新增 `AgentEvent` 变体（见 §3 补充），在 `AgentStart` **之后**、首个 `TurnStart` **之前** emit：

```rust
AgentEvent::ReplayIntegrityWarning {
    session_id: SessionId,
    issue: IntegrityIssue,
}
```

客户端可以渲染一个明显的提示（CLI 用红色文字，TUI 用模态横幅）。
本事件**也会持久化**——它本身记录了"我们做过截断"这件事，下次再 resume 时
不会重复检测（截断已落盘）。

**3. 被丢弃的事件写到 `corrupted.log`**

```text
{data_dir}/sessions/{session_id}/
  ├── events.log           # 已被截断的主日志
  ├── corrupted.log        # 被丢弃的事件（追加，时间戳前缀）
  └── snapshot.json
```

`corrupted.log` 是 append-only，每次截断都追加一段，带头部元信息：

```jsonl
{"_corrupted_block": true, "detected_at": "2026-06-22T10:00:00Z", "issue": {...IntegrityIssue...}}
{"seq":42,"ts":"...","type":"TurnStart","turn_id":"...",...}
{"seq":43,"ts":"...","type":"MessageStart",...}
{"seq":44,"ts":"...","type":"MessageDelta",...}
```

> **为什么不直接删？**——丢失的事件可能有 debug 价值（譬如 panic 前最后一条
> ToolStart 显示是哪个工具触发的崩溃）。`corrupted.log` 给运维一个事后取证手段，
> 文件本身被排除在 replay 之外（`EventLog::replay()` 只读 `events.log`）。

#### 7.3.4 实施位置

在 `EventLog::replay_for_resume()` 中执行（新增方法，与现有 `replay()` 区分）：

```rust
impl EventLog {
    /// Resume 专用 replay：读 events.log，截断半截 turn，写 corrupted.log，
    /// 返回清理后的事件流 + 可选的 IntegrityIssue（调用方据此 emit
    /// AgentEvent::ReplayIntegrityWarning）。
    pub fn replay_for_resume(&mut self) -> std::io::Result<(Vec<PersistedAgentEvent>, Option<IntegrityIssue>)> {
        let raw = self.replay()?;  // 读现有 events.log
        let (clean, dropped, issue) = truncate_to_last_complete_turn(raw);

        if let Some(ref issue) = issue {
            // 写 corrupted.log（best-effort，失败不阻塞 resume）
            if let Err(e) = self.append_corrupted_block(issue, &dropped) {
                tracing::error!(error = ?e, "failed to write corrupted.log; dropped events lost");
            }

            // 重写 events.log：保留 clean，丢弃 tail
            // 用 tmp file + atomic rename 保证写入失败不毁文件
            if let Err(e) = self.rewrite_truncated(&clean) {
                tracing::error!(error = ?e, "failed to rewrite events.log after truncation");
                return Err(e);
            }

            // tracing 警告
            tracing::warn!(
                session_id = %self.session_id,
                kind = ?issue.kind,
                dropped = issue.dropped_event_count,
                first_seq = issue.first_dropped_seq,
                last_seq = issue.last_dropped_seq,
                "events.log integrity issue — truncated to last complete turn"
            );
        }

        Ok((clean, issue))
    }
}
```

调用方（`SessionManager::create_session_with_context` 的 resume 路径）：

```rust
let (events, integrity_issue) = event_log.replay_for_resume()?;
let context = rebuild_context(&events);

let engine = ReActEngine::new(...).with_initial_context(context);
// 把 issue 传给 engine，让它在 AgentStart 之后 emit ReplayIntegrityWarning
let engine = if let Some(issue) = integrity_issue {
    engine.with_pending_integrity_warning(issue)
} else {
    engine
};
```

引擎在 `run()` 里：

```rust
event_tx.send(AgentEvent::AgentStart { ... }).await;

if let Some(issue) = self.pending_integrity_warning.take() {
    let warning = AgentEvent::ReplayIntegrityWarning {
        session_id: self.session_id,
        issue,
    };
    // 这条事件也持久化（is_persistent() = true），让"我们检测并截断过"
    // 被永久记录。下次 replay 时如已存在该事件，截断检测应跳过同一段。
    let _ = event_log.append(warning.clone())?;
    let _ = event_tx.send(warning).await;
}

// ... 进入 cmd loop
```

#### 7.3.5 幂等性

`AgentEvent::ReplayIntegrityWarning` 持久化后，下次再 resume 时该 session
events.log 末尾不会再有半截 turn（已被截断并写过 warning），所以 `replay_for_resume`
返回 `issue: None`。**警告只 emit 一次**。

如果出现新的崩溃（resume 后跑了一会儿又挂），下次 resume 检测到新的半截 turn，
再次 emit 一个新的 `ReplayIntegrityWarning`——`corrupted.log` 也追加新 block。
每次崩溃留一份痕迹，独立可溯源。

### 7.4 Snapshot

`EventLog::maybe_snapshot()` 逻辑保留——每 100 个事件写一次 `snapshot.json`，内容为
"重建后的 Vec<ChatMessage>"。Resume 时优先读 snapshot，然后从 snapshot 对应的 seq
之后开始 replay events.log——这是性能优化，不影响正确性（fallback 仍可只用 events.log）。

---

## 8. 迁移路径

### 8.1 旧 events.log 兼容

旧格式（`EventLogEntry`）字段示例：

```jsonl
{"seq":0,"ts":"...","type":"UserMessage","content":"hi"}
{"seq":1,"ts":"...","type":"AssistantText","content":"hello"}
{"seq":2,"ts":"...","type":"Finish","stop_reason":"EndTurn","usage":{...}}
```

新代码读到此格式时，做一次性懒迁移：

```rust
fn migrate_legacy(events: &[LegacyEventLogEntry]) -> Vec<AgentEvent> {
    // 把无 turn_id / message_id 的旧事件，按"UserMessage → Finish"分组生成合成 turn。
    // 合成 turn_id / message_id 用确定性 UUID（v5(seq) 之类），让多次 migration 幂等。
    ...
}
```

**或者**简单粗暴：MVP 阶段直接不兼容，让用户重启 session。文档列出"重大变更，旧 events.log 需丢弃"。我倾向**简单不兼容**——目前还没有生产用户，迁移代码价值不够。

### 8.2 实施切换点

为避免半成品中间态，单次大改：

1. 新 `parrot-protocol::AgentEvent` 引入
2. `EventLogEntry` 删除，`ServerMessage` 瘦身
3. `ProviderStreamEvent` 重命名
4. 引擎全面改造
5. 删 `SessionAdapter`
6. CLI 接收端用 `AgentEvent` 渲染
7. 所有测试调整

中间无法 partial-apply（类型不兼容）。**因此本批是一个 PR，全量替换。**

---

## 9. 测试

| 测试 | 类型 | 覆盖点 |
|------|------|--------|
| `protocol::agent_event::roundtrip_all_variants` | unit | 每个变体 serde roundtrip |
| `protocol::agent_event::is_persistent_filter` | unit | `is_persistent()` 表 |
| `protocol::server_message::roundtrip_with_envelope` | unit | `ServerMessage::AgentEvent(...)` serde |
| `core::engine::emits_lifecycle_bracket` | integration | 跑一个 mock provider 的 turn，断言 `[AgentStart, TurnStart, MessageStart, MessageDelta+, MessageEnd, TurnEnd]` 严格嵌套 |
| `core::engine::tool_turn_has_tool_envelope` | integration | 含工具的 turn 断言 `[..., MessageEnd(stop=ToolUse), ToolStart, ToolEnd, MessageStart, MessageEnd(stop=EndTurn), TurnEnd]` |
| `core::engine::abort_emits_turnend_aborted` | integration | abort 中途 → `TurnEnd { stop_reason: Aborted }` |
| `core::engine::confirm_reject_emits_toolend_error` | integration | confirm reject → `ToolEnd { result: is_error=true }` |
| `core::engine::agent_end_on_disconnect` | integration | drop cmd_rx → 收到 `AgentEnd { reason: ClientDisconnect }` |
| `core::session::resume_replays_to_context` | integration | append 一串 events → `rebuild_context` 后 context 等价 |
| `core::session::resume_drops_partial_turn` | integration | 末尾有 TurnStart 但无 TurnEnd → resume 后这截被丢弃 |
| `core::session::resume_emits_integrity_warning` | integration | 末尾半截 → AgentStart 后立刻收到 `ReplayIntegrityWarning`，issue.dangling_turn_ids 含丢弃的 turn_id |
| `core::session::resume_writes_corrupted_log` | integration | 半截 turn → `corrupted.log` 出现一个 block，含元信息 + 被丢的事件 |
| `core::session::resume_truncation_is_idempotent` | integration | resume → 再 resume：第二次不再 emit warning（events.log 已被截断且 warning 已持久化） |
| `core::session::resume_events_after_agentend_truncated` | integration | 构造 `AgentEnd` 之后又写事件 → 检测为 `EventsAfterAgentEnd`，截到 AgentEnd 后 |
| `e2e::e2e_chat_emits_full_lifecycle` | integration | 端到端：CreateSession → Chat → 客户端收到完整 AgentEvent 序列 |
| `e2e::e2e_resume_replays_history` | integration | Chat → ResumeSession → History 返回的事件能重建相同 context |

---

## 10. 不在本批的事项（未来扩展）

- **Sub-agent 嵌套**：`AgentEvent` 已为 sub-agent 预留——所有变体带 `session_id`，未来子 agent 用独立 session_id 但带 `parent_session_id` 字段。本批暂不引入字段，避免引入未消费的复杂度。
- **`ToolUpdate` 真正消费**：本批定义类型但 MVP 不发射。等 `shell_exec` 升级为 streaming output / `web_fetch` 升级为分块下载时再启用。
- **拆 `parrot-providers` / `parrot-tools` crate**：见问题 2，后续独立批。
- **命名统一**：见问题 3，最后一步。

---

## 11. 实施顺序

1. **协议层**：写 `agent_event.rs`，删旧 `EventLogEntry` / `StreamEvent` 中重复变体，瘦身 `ServerMessage`，加 roundtrip 测试
2. **Provider 层**：`StreamEvent` → `ProviderStreamEvent` 重命名 + 移到 `provider.rs`，删除 `ToolResult` / `ToolCallConfirmationRequired` 两个变体
3. **引擎层**：重写 `run()` + `handle_turn()`，补 lifecycle envelope，加 `AgentEndGuard`
4. **持久化层**：`EventLog` 改吃 `AgentEvent`，按 `is_persistent()` 过滤；`rebuild_context` 替换旧 replay 逻辑
5. **Daemon 层**：删 `session_adapter.rs`，server.rs 直接 `ServerMessage::AgentEvent(ev)` 转发
6. **CLI 层**：渲染器读 `AgentEvent`，重写 `print_stream`
7. **测试**：上面 §9 列表全部到位
8. **文档**：本文档回填 ✅；主设计文档 `2026-06-21-parrot-design.md` 的 §4 流式映射图更新

完成后再启动**问题 2**（crate 拆分）和**问题 3**（命名）专项。
