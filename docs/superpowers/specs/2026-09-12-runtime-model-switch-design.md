# 运行时切换 model 设计

日期：2026-09-13
状态：已批准
前置：`docs/superpowers/specs/2026-09-12-single-provider-design.md`

## 1. 目标

已存在的会话可在 turn 边界切换模型（`ClientMessage::Model`）。单 provider + 透传使切换只是字符串变更；`[1m]` 的 thinking / effort 元数据在请求时按模型 id 查找，切换后自动生效；**压缩预算（context window 相关）随切换同步重算**——模型与其上下文大小配置是一体的。

## 2. 设计

### 2.1 协议

- `ClientMessage::Model { session_id: SessionId, model: String }`
- `ServerMessage::ModelSet { session_id: SessionId, model: String }`——daemon 收到即回（确认已接收），实际生效点是下一轮 turn；**轮次进行中发送会被忽略**（引擎丢弃），故 TUI 在 turn 活跃时本地拦截、不发送（方案 A：轮中拒绝）

### 2.2 会话命令（parrot-core）

- `SessionCmd::SetModel { model: String }`（内部命名）
- 会话任务处理：非 turn 期间收到 ⇒ 更新持有的 `GenerateConfig.model`（max_tokens/temperature 等其余字段保留），发 `AgentEvent` 通知? 不——**不发新 AgentEvent**（scope 控制；客户端已有 ModelSet ack）。
- **压缩预算同步重算（模型与其上下文大小配置一体）**：SetModel 生效时重新解析新模型的 context window（复用 `resolve_model_context_window`）并重建 `ContextManager`：`budget = min(max_history_tokens, new_window)`，`keep_recent_turns` 保持会话配置。未知 window（0/未收录）⇒ 预算退回 `max_history_tokens`（与 session 启动语义一致）。该解析可能触发一次 `list_models`（anthropic 远端），发生在空闲等待下一轮的间隙，不阻塞 ack。
- **turn 进行中**：引擎的 select! 已持有 cmd_rx；`SetModel` 与 `Chat` 同样处理 ⇒ warn + 忽略（该轮保持旧模型与旧预算）。语义 = 用户确认的方案 A（变体：排队不做）。
- `meta.json` 的 model 字段：daemon 收到 `ClientMessage::Model` 时即更新（与 init_session 同一写入路径；轮次报错时日志模型仍是请求目标）。provider 字段按 registry 解析更新。

### 2.3 daemon

runtime 新 arm：校验会话存在 → 转发 `SessionCmd::SetModel` → 回 `ModelSet`。`model` 为空串视为无效（Error 响应）。

### 2.4 TUI

- `/model <name>`：**turn 活跃时**（`app.is_turn_active()`）push Info「当前轮进行中，请稍后再切换」且不发送；否则发送 `ClientMessage::Model`；daemon `ModelSet` 到达时 push Info 条目「model 已切换为 X」并更新状态栏 model
- 帮助文本更新：不枚举（用户已重写 /help；命令发现走弹窗 REGISTRY）

### 2.5 非目标

- 不在 turn 中途热切（warn+忽略）
- 不改 `temperature`/`max_tokens` 运行时切换（max_tokens 仍来自 provider 配置）
- resume 语义不变（meta.json 的 model 仅记录，恢复仍跟随 daemon 当前默认——runtime.rs 现有行为）
- 不重算 `AgentStart` 事件（只在会话启动发一次；TUI 状态栏由 ModelSet ack 更新）

## 3. 测试

- protocol roundtrip：Model / ModelSet 双向
- core：SetModel 在 turn 边界更新 config（下轮 provider 收到新 model）；turn 中收到被忽略（沿用 Chat 模式）；**切换后压缩预算按新模型 window 重算**（mock provider list_models 中两模型 window 不同，观察 prune/compaction 行为随之改变）
- daemon：Model arm 空 model 报错、ModelSet 回包
- TUI：slash 解析 + Info 条目
- 全量 gates
