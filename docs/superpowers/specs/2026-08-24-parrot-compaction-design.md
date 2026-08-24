# Parrot 上下文压缩(结构化摘要)— Design

**Date:** 2026-08-24
**Status:** Approved (brainstorm 5 问 + 两段设计均已确认)
**Branch:** `feat/compaction`(从 `feat/tui-iteration` 创建)

## 1. 目标

在既有预算裁剪(整轮丢弃的 `ContextManager::prune`)之上,增加
**Pi 式结构化摘要压缩**:触发阈值后,把切点之前的历史交给当前模型
生成结构化摘要,切点之后的近期消息原样保留。解决"prune 之后模型
彻底失忆"的问题,兑现备忘录 p0「上下文压缩窗口减预留触发、char/4
估算、生成结构化摘要」。

参考调研:`docs/superpowers/业界对比调研.md` 与用户提供的
三方对比文章(Claude Code / Pi / Codex 缓存·结构·边界三支柱)。

## 2. 已确认的决策(brainstorm 结论)

| 决策点 | 选择 | 落选项 |
|---|---|---|
| 压缩后上下文形态 | **Pi 式:摘要 + 原样保留近期** | Codex 式激进丢弃;仅微压缩 |
| 摘要模型 | **会话当前模型**;摘要生成收敛为单一内部函数边界,将来可替换 | 可配置专用模型;hook 接管 |
| 触发时机 | **Turn 开始时主动检查**(每 turn 一次,在 ReAct 循环外) | 反应式 400 重试;双保险 |
| 持久化 | **新持久事件 `CompactionSummary`**,events.log 唯一事实源不变量保持 | 内存+快照;独立 summary.json |
| 摘要注入角色 | **User 角色 + 醒目定界符** | System 拼接;Assistant |

## 3. 架构

```
parrot-core/src/compaction.rs   ← 新文件,纯逻辑(切点/估算/prompt/标记)
parrot-core/src/context.rs      ← 不动(prune 保留为第二层兜底)
parrot-core/src/engine.rs       ← run() 中 TurnStart 事件前插入 maybe_compact
parrot-protocol/agent_event.rs  ← 新事件变体 CompactionSummary
parrot-core/event_log.rs        ← rebuild_context 处理新事件
parrot-config                   ← [session] 新增 4 个配置字段
parrot-daemon/runtime.rs        ← 配置接线(with_context_limits 扩为 struct)
src/tui/                        ← replay 渲染 Info 条目
```

### 3.1 流程

```
run() 收到 Chat 命令
  ├─ hooks TurnStart(现有,不动;Block 则不浪费摘要调用)
  ├─ ★ maybe_compact(context)        ← 新增,每 turn 一次
  │    1. 估算总量 = Σ char/4(复用 ContextManager::estimate_tokens 逻辑,
  │       基于**不含当前用户消息**的历史;阈值预留的余量吸收新消息)
  │    2. 若 > budget × threshold(默认 0.9)→ 执行压缩,否则跳过
  │    3. 找切点:从最新往回累加估算,直到 ≥ keep_recent_tokens,
  │       对齐到 User 消息边界(turn 边界,与现有 prune 同粒度)
  │       —— Tool 消息绝不孤立,tool_use/tool_result 配对约束天然满足;
  │       摘要范围 = 已完成的历史 turn,当前 turn 不参与
  │    4. 移除旧摘要消息(若有),把 [切点前消息 + 旧摘要] 序列化
  │       → 调 provider.chat_stream(无 tools)生成结构化摘要
  │    5. context = [system] + [摘要(User, 带标记前缀)] + 保留段
  │    6. 落盘 AgentEvent::CompactionSummary + 发给 client
  ├─ 发 TurnStart 事件(现有)
  └─ handle_turn(现有,不动)
       ├─ push 用户消息、hooks ContextReady
       ├─ prune(现有,降级为第二层兜底)
       └─ ReAct 循环(不动)
```

**为何必须在 TurnStart 事件之前**:若 `CompactionSummary` 落在
TurnStart 与 TurnEnd 之间,resume 的半成品 turn 截断
(`truncate_to_last_complete_turn`)会把它连同残缺 turn 一起丢掉,
导致每次 resume 反复重压。放在 TurnStart 之前,它属于"上一完整
turn 之后、tail 内 TurnStart 之前"的位置,截断后必然保留。

### 3.2 关键机制

- **增量语义**:再次压缩时,旧摘要消息在切点之前、被并入新摘要的
  输入(消息自然携带,无需 Pi 的 UPDATE 双 prompt),摘要不膨胀。
- **摘要调用**:`provider_registry.resolve(当前模型)`,
  不带 tool_defs,`max_tokens = summary_max_tokens`(默认 4096)。
  期间 Abort 不打断(调用短,MVP 接受,此处记录为已知取舍)。
- **旧摘要识别**:摘要消息内容以固定标记前缀
  `[CONVERSATION SUMMARY]` 开头;压缩与 rebuild 均据此定位。
  零 ChatMessage 类型改动,serde/resume 天然兼容。
- **压缩失败兜底**:摘要 LLM 调用失败 → `tracing::warn` + 跳过,
  后续 prune 照跑。两层防线,会话永不因压缩卡死。
- **单轮超预算**:切点对齐 turn 边界时若单轮自身 > keep 预算,
  整轮保留(不切),交给 prune 兜底。不做 Pi 的 split-turn
  双摘要(复杂度大头,MVP 明确不做)。

## 4. 协议与持久化

```rust
// parrot-protocol/agent_event.rs 新变体
CompactionSummary {
    session_id: SessionId,
    turn_id: TurnId,               // 触发本次压缩的 turn(事件在其 TurnStart 之前落盘)
    summary: String,              // 含标记前缀的完整摘要文本
    dropped_event_count: u32,     // 本次压缩丢弃的上下文消息数
    kept_from_seq: u64,           // 保留段起点事件 seq(见下,重建语义的必要字段)
}
```

- 加入 `is_persistent()` 集合。
- **resume**:`rebuild_context` 重建时记录每个事件 seq 对应的
  已重建消息数;遇到 `CompactionSummary` → 把已重建列表截断到
  `kept_from_seq` 时刻的长度(即丢弃来自 seq < kept_from_seq 事件的
  消息),再输出一条 User 角色摘要消息(带标记前缀),继续重建
  保留段。最终 context = system + 摘要 + 保留段。
  `kept_from_seq` 指向保留段第一个 turn 的 `TurnStart` 事件 seq;
  全部历史被摘要时指向 `CompactionSummary` 自身 seq(保留段为空)。
  **该字段是重建正确性的必要输入,不是可选审计信息。**
- **事件流不删不改**:原始 TurnStart/MessageEnd/ToolEnd 全部留在
  events.log,`CompactionSummary` 只是"上下文重建时的分界指令"。
  审计、debug、未来"解压回放"都保得住。
- TUI:replay 到 `CompactionSummary` 渲染 Info 条目
  ("已压缩 N 条历史消息");session_adapter 透传。
- roundtrip 测试补 `crates/parrot-protocol/tests/roundtrip.rs`。

## 5. 配置(`[session]`,全部带默认,零配置可用)

```toml
[session]
compaction = true                  # 默认 true;false 时完全走旧 prune 路径
compaction_threshold = 0.9         # 估算/budget 比例触发
keep_recent_tokens = 20000         # 切点保留预算
summary_max_tokens = 4096          # 摘要调用 max_tokens 上限
# 现有 max_history_tokens / keep_recent_turns 语义不变
```

daemon 经现有 `with_context_limits` 路径传入;该方法参数扩为
小 struct `ContextLimits`(避免参数列表继续膨胀)。

## 6. 摘要 Prompt(固定内嵌常量)

```
你是压缩助手。将以下对话历史压缩为结构化摘要,供后续对话参考。
输出格式(纯文本,不调用任何工具):
<summary>
## 目标与任务
## 关键事实与决定(文件路径、命令、命名、约束)
## 未完成事项与下一步
## 关键文件/代码位置
</summary>
```

请求形态:`[system(压缩指令)] + [User(待压缩对话文本)]`,
与主对话完全隔离的一次性调用(无缓存复用考量,当前 provider
适配器未接 prompt cache)。

## 7. 边界情况

- 压缩时上下文不足两条 turn → 跳过(没东西可摘)。
- keep 段极小(keep_recent_tokens 被模型窗口压得很低)→
  切点至少保留最近 1 个完整 turn。
- system prompt 不参与压缩,永远置顶。
- `compaction = false` → 行为与现状完全一致(纯 prune)。

## 8. 测试策略

| 层 | 测试 |
|---|---|
| `compaction.rs` 纯函数 | 切点选择(阈值触发/不触发、turn 边界对齐、旧摘要识别、极小 keep 段)、估算 |
| engine(`react_loop.rs` 模式) | MockProvider 记录收到的消息:断言压缩后请求含摘要消息且不含被压消息;断言摘要失败时 prune 兜底仍生效;断言 `CompactionSummary` 落盘于 TurnStart 之前 |
| resume | 构造含 `CompactionSummary` 的 events.log → `rebuild_context` 按 `kept_from_seq` 截断,输出 = system + 摘要 + 保留段;半成品 turn 截断后 `CompactionSummary` 仍保留 |
| protocol | roundtrip + `is_persistent` |
| e2e | MockProvider 撑大上下文 → 断言 client 收到 `CompactionSummary` 事件 |

## 9. 明确不做(YAGNI)

- prompt 缓存保护(provider 未接 cache,无对象)
- 树/分支摘要(备忘录 p2 独立项)
- split-turn 双摘要
- 反应式 400 压缩重试
- 可插拔摘要模型 / hook 接管(决策 2 已留函数边界)
- 手动 `/compact` 命令

## 10. 约束(AGENTS.md 摘要)

- `parrot-core` 零 IO:压缩逻辑是纯内存计算 + provider trait 调用,
  符合约束(provider 调用经 trait,非 reqwest 直连)。
- 错误跨 crate 走 `thiserror`(复用 `AgentError::Provider` 等)。
- 验证:`cargo build --workspace && cargo test --workspace &&
  cargo clippy --workspace --all-targets -- -D warnings &&
  cargo fmt --all -- --check`。
