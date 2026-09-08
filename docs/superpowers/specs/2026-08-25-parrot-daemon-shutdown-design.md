# Parrot Daemon 优雅退出 — Design

**Date:** 2026-08-25
**Status:** Approved (brainstorm 3 问 + 设计已确认)
**Branch:** `feat/daemon-shutdown`(从 `feat/compaction` 创建)

## 1. 目标

daemon 收到 Ctrl+C / SIGTERM 时,让所有活动 session 走引擎正常退出路径
(`cmd_rx.recv()=None → break → AgentEndGuard::fire_and_drop`),保证
`AgentEnd` 事件落盘 `events.log`。后续 resume 看到"末尾有 AgentEnd"
即可区分正常关闭与崩溃(崩溃时无 AgentEnd)。兑现备忘录 p1
「daemon 进程的优雅退出与崩溃兜底」。

## 2. 已确认的决策

| 决策点 | 选择 |
|---|---|
| 等待时长 | 短超时(3 秒),超时强制 abort 兜底 |
| 清理范围 | 遍历 SessionManager 所有 handle;已结束的(cmd_tx 已关、JoinHandle 已完成)是 no-op,实际只影响活动 session |
| 关闭机制 | 协作式:发 Abort 打断 in-flight → 关 cmd_tx → 引擎走干净路径 → JoinHandle 限时 await → 超时 abort() |
| 信号源 | `tokio::signal::ctrl_c()`(跨平台 SIGINT)+ Unix SIGTERM |

## 3. 根因分析

当前 `abort_handle.abort()` 取消引擎 task 时,`fire_and_drop` 内的
`.await`(hooks.run、event_log.append、event_tx.send)被一并取消,只剩
`Drop::try_send`(只发不落盘)。所以 daemon 被杀时 in-flight turn 的
`AgentEnd` 永远进不了 `events.log`。

修复路径:不 abort,改走引擎**正常退出路径**——关 `cmd_tx` 让
`cmd_rx.recv()` 返回 `None`,引擎主循环 break,`fire_and_drop`
完整跑完(含 append 到磁盘)。abort 仅作 deadline 超时兜底。

## 4. 架构

### 4.1 组件改动

- **`SessionHandle`**:`AbortHandle` → `JoinHandle<()>`(tokio 既能
  `.await` 又能 `.abort()`);新增 `end_reason: Arc<Mutex<AgentEndReason>>`
  字段,`shutdown_all` 触发前设为 `DaemonShutdown`。
- **`ReActEngine`**:`end_reason` 从 `run()` 局部变量改成 engine 字段,
  由 `with_end_reason(Arc<Mutex<AgentEndReason>>)` builder 传入;
  `AgentEndGuard` 读 `self.end_reason`(行为不变,仅来源迁移)。
- **`SessionManager`**:新增 `shutdown_all(deadline: Duration)`;`spawn_session`
  /`create_resumed_session` 构造 `end_reason` Arc 并存入 handle,经
  `with_end_reason` 传给 engine。
- **`runtime::run()`**:`tokio::select!` 在关闭信号与 `accept` 之间
  竞速;信号到 → 停 accept → `shutdown_all(3s)` → break。
- **信号源**:`tokio::signal::ctrl_c()`(跨平台)+ Unix
  `signal::unix::SignalKind::terminate()`(select 合并两个信号 future)。

### 4.2 流程

遍历所有 handle;已结束的(cmd_tx 已关、JoinHandle 已完成)在每步
都是 no-op,实际只影响活动 session。

```
Ctrl+C / SIGTERM 收到
  → 停止 accept 新连接
  → shutdown_all(Duration::from_secs(3)):
      for each session handle:
        end_reason.lock() = DaemonShutdown
        cmd_tx.send(Abort).ok()     // 打断 in-flight 流/工具
        drop cmd_tx                  // 关通道
      for each JoinHandle:
        timeout(3s, handle.await):
          Ok(())  → fire_and_drop 已落盘 AgentEnd
          超时    → handle.abort()  // Drop try_send 兜底,warn
  → run() 返回,进程退出
```

### 4.3 各引擎状态下的行为

| 引擎状态 | Abort 的效果 | 关 cmd_tx 后 |
|---|---|---|
| 空闲(`cmd_rx.recv().await` 中) | 顶层 Abort 被忽略(no-op) | `recv()` 返回 None → break → fire_and_drop |
| 流式中(`stream_llm_message` 的 select) | 取消流,设 aborted,发 TurnEnd{Aborted} | 主循环 recv()=None → break → fire_and_drop |
| 工具执行中(`race_with_abort`) | Abortable::Aborted → aborted ToolEnd + TurnEnd{Aborted} | 同上 |
| confirm 等待中(`race_with_abort`) | 同上,撤注册 | 同上 |

所有路径最终都走 `fire_and_drop`,AgentEnd 先 append 后 send(落盘顺序不变)。

## 5. 不变量

- `fire_and_drop` 的"先落盘再发送"顺序不变(resume 靠读 events.log
  区分"已结束"与"崩溃")。
- `abort()` 仅 deadline 超时兜底;此时 Drop 的 `try_send` 是最后手段,
  可能不落盘但 warn 记录——这与现状(直接 abort)等价,不是回归。
- `AgentEndReason::DaemonShutdown` 终于被设置(resume 能进一步区分
  "daemon 被杀" vs "client 断开但 daemon 存活")。
- `handle_connection` 任务不在 `shutdown_all` 范围;engine 退出后
  event_rx 关闭,relay 自然结束。
- 现有 `truncate_lone_trailing_resume_agent_start_is_kept` 证明 resume
  追加 `AgentStart` 不会被误判为 `EventsAfterAgentEnd` 损坏——
  备忘录担心的误判其实已修,本次不动。

## 6. 测试

| 层 | 测试 |
|---|---|
| 单元(`session.rs`) | `shutdown_all` 对 mock 引擎任务(睡眠/响应 Abort)调用,断言 JoinHandle 完成、`end_reason=DaemonShutdown`、`AgentEnd` 落盘 |
| 单元 | 空闲 session + shutdown_all:cmd_tx 关闭后引擎 task 在 deadline 内退出 |
| 单元 | 超时 session(永不响应):shutdown_all deadline 到 → abort,返回不卡死 |
| 集成(`e2e_test.rs`) | 起一个 session + 发 Chat 触发流式 + 调 `shutdown_all`(不真发信号,避免测试发系统信号),断言 events.log 末尾有 `AgentEnd{reason: DaemonShutdown}` |

## 7. 明确不做(YAGNI)

- Windows SIGTERM(`TerminateProcess` 不可捕获)
- CLI 侧主动发关闭信号(独立项;目前 CLI 退出 = 子进程随之死,
  那条路径的优雅化是另一项)
- `/shutdown` RPC、`ClientMessage::Shutdown`
- 活动 session 之外的 `handle_connection` 任务清理
