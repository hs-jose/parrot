# Daemon 优雅退出 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** daemon 收到 Ctrl+C/SIGTERM 时,所有活动 session 走引擎正常退出路径(`cmd_rx.recv()=None → fire_and_drop`),保证 `AgentEnd{reason: DaemonShutdown}` 落盘 events.log。

**Architecture:** `SessionHandle` 的 `AbortHandle` 换成 `JoinHandle`(可 await + 可 abort);`SessionManager::shutdown_all(deadline)` 发 Abort 打断 in-flight → 关 cmd_tx → 引擎走干净路径 → JoinHandle 限时 await,超时才 abort 兜底;`runtime::run()` 用 `tokio::select!` 在关闭信号与 accept 之间竞速。

**Tech Stack:** Rust 2021, tokio (signal, time), 现有 engine/session 管道。无新依赖。

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-25-parrot-daemon-shutdown-design.md`
- `parrot-core` 零 IO 不变;信号处理在 daemon 二进制层。
- `AgentEndGuard::fire_and_drop` 的"先落盘再发送"顺序不变。
- `AgentEndReason::DaemonShutdown` 终于被设置(已存在但从未使用)。
- 不加无意义注释。代码精简。
- Branch: `feat/daemon-shutdown` 从 `feat/compaction` 创建。
- 每个任务后:`cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` 必须通过。
- 不改 `parrot.toml`(有未提交 API key)。
- 若 cargo 报 `os error 5`(target\debug\*.exe 被占用):用 `CARGO_TARGET_DIR=C:\Users\ADMINI~1\AppData\Local\Temp\opencode\parrot-target`,不要杀进程。

---

### Task 1: `SessionHandle` 用 `JoinHandle` + `end_reason` Arc

**Files:**
- Modify: `crates/parrot-core/src/session.rs` (`SessionHandle`, `spawn_session`, `create_resumed_session`, `remove`)
- Modify: `crates/parrot-core/src/engine.rs` (`ReActEngine` 加 `end_reason` 字段 + `with_end_reason` builder;`run()` 读它)

**Interfaces:**
- Consumes: `AgentEndReason`(已有)。
- Produces: `SessionHandle { id, cmd_tx, event_rx, join_handle: JoinHandle<()>, end_reason: Arc<Mutex<AgentEndReason>> }`;`ReActEngine::with_end_reason(Arc<Mutex<AgentEndReason>>)`;engine `run()` 从 `self.end_reason` 读 reason 而非局部 `end_reason`。

- [ ] **Step 1: 改 `SessionHandle` 结构体**

`crates/parrot-core/src/session.rs`:

```rust
pub struct SessionHandle {
    pub id: Uuid,
    pub cmd_tx: mpsc::Sender<SessionCmd>,
    event_rx: Option<mpsc::Receiver<AgentEvent>>,
    pub join_handle: tokio::task::JoinHandle<()>,
    pub end_reason: Arc<Mutex<AgentEndReason>>,
}
```

加 import:`use parrot_protocol::agent_event::AgentEndReason;`、`use std::sync::Mutex;`(若未引入)。

- [ ] **Step 2: `spawn_session` 构造 end_reason Arc + JoinHandle + 传给 engine**

把 `spawn_session` 里的引擎构造改为:

```rust
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));

        let engine = ReActEngine::new(
            id,
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.provider_registry),
            gen_config,
            system_prompt,
            session_dir,
            self.working_dir.clone(),
        )
        .with_confirm_config(self.confirm_config.clone())
        .with_initial_context(initial_context)
        .with_context_limits(self.context_limits.clone())
        .with_end_reason(Arc::clone(&end_reason));
        let engine = if let Some(h) = self.hooks.clone() {
            engine.with_hooks(h)
        } else {
            engine
        };

        let join_handle = tokio::spawn(async move {
            engine.run(cmd_rx, event_tx).await;
        });

        self.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx,
                event_rx: Some(event_rx),
                join_handle,
                end_reason,
            },
        );
```

`create_resumed_session` 同样改造(它独立构造 engine,目前漏了 `with_context_limits`,这次一并补上 end_reason)。在 `create_resumed_session` 的 engine 构造链里,`.with_resumed_from(resumed_from_seq)` 之后加 `.with_end_reason(Arc::clone(&end_reason))` 和 `.with_context_limits(self.context_limits.clone())`(后者是补既有缺口),并把 `let abort_handle = tokio::spawn(...).abort_handle();` 改成上面同样的 `let end_reason = ...; let join_handle = tokio::spawn(...);` 模式。

- [ ] **Step 3: `remove` 改用 join_handle**

```rust
    pub fn remove(&mut self, id: &Uuid) -> bool {
        if let Some(handle) = self.sessions.remove(id) {
            handle.join_handle.abort();
            true
        } else {
            false
        }
    }
```

- [ ] **Step 4: `ReActEngine` 加 `end_reason` 字段 + builder + `run()` 读它**

`crates/parrot-core/src/engine.rs`:

struct 加字段:
```rust
    end_reason: Arc<Mutex<AgentEndReason>>,
```

`new()` 初始化:
```rust
            end_reason: Arc::new(Mutex::new(AgentEndReason::ClientDisconnect)),
```

builder:
```rust
    pub fn with_end_reason(mut self, reason: Arc<Mutex<AgentEndReason>>) -> Self {
        self.end_reason = reason;
        self
    }
```

`run()` 里删掉局部 `let end_reason: Arc<Mutex<AgentEndReason>> = Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));`,guard 用 `Arc::clone(&self.end_reason)`。`None =>` 分支 `*self.end_reason.lock().unwrap() = AgentEndReason::ClientDisconnect;`(原来写局部变量,现在写字段,语义不变)。

- [ ] **Step 5: 编译 + 全量测试**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS(行为不变,只是 reason 来源迁移)。

- [ ] **Step 6: Lint + commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
git add crates/parrot-core/src/session.rs crates/parrot-core/src/engine.rs
git commit -m "refactor(core): SessionHandle JoinHandle + end_reason Arc for graceful shutdown"
```

---

### Task 2: `SessionManager::shutdown_all`

**Files:**
- Modify: `crates/parrot-core/src/session.rs`

**Interfaces:**
- Consumes: Task 1 的 `SessionHandle`。
- Produces: `pub async fn shutdown_all(&mut self, deadline: Duration)`。

- [ ] **Step 1: 写失败测试**

在 `session.rs` 末尾加 `#[cfg(test)] mod tests;`(若已有则追加)。测试用真实的 `ReActEngine`(text-only MockProvider 已在 react_loop.rs,但这里是 session 层 —— 用最简的:spawn 一个 sleep 永不退出的 task 模拟活动 session,验证 shutdown_all 能让它 deadline 内结束)。

```rust
#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_all_aborts_active_session_within_deadline() {
        let mut mgr = SessionManager::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(ProviderRegistry::new()),
            GenerateConfig::default(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("."),
        );
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (evt_tx, _evt_rx) = mpsc::channel::<AgentEvent>(64);
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));
        let id = Uuid::new_v4();
        let join = tokio::spawn(async move {
            let mut aborted = false;
            loop {
                tokio::select! {
                    biased;
                    Some(SessionCmd::Abort) = cmd_rx.recv() => { aborted = true; break; }
                    else => break,
                }
            }
            let _ = (aborted, evt_tx);
        });
        mgr.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx: cmd_tx.clone(),
                event_rx: None,
                join_handle: join,
                end_reason: Arc::clone(&end_reason),
            },
        );

        mgr.shutdown_all(Duration::from_millis(500)).await;

        assert_eq!(*end_reason.lock().unwrap(), AgentEndReason::DaemonShutdown);
        assert!(cmd_tx.is_closed());
    }

    #[tokio::test]
    async fn shutdown_all_force_aborts_unresponsive_session() {
        let mut mgr = SessionManager::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(ProviderRegistry::new()),
            GenerateConfig::default(),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("."),
        );
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCmd>(32);
        let (_evt_tx, _evt_rx) = mpsc::channel::<AgentEvent>(64);
        let end_reason: Arc<Mutex<AgentEndReason>> =
            Arc::new(Mutex::new(AgentEndReason::ClientDisconnect));
        let id = Uuid::new_v4();
        let join = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        mgr.sessions.insert(
            id,
            SessionHandle {
                id,
                cmd_tx,
                event_rx: None,
                join_handle: join,
                end_reason,
            },
        );

        let start = std::time::Instant::now();
        mgr.shutdown_all(Duration::from_millis(100)).await;
        assert!(start.elapsed() < Duration::from_millis(500), "must not hang");
    }
}
```

- [ ] **Step 2: 验证失败**

Run: `cargo test -p parrot-core --lib session::shutdown_tests`
Expected: FAIL — `shutdown_all` not found。

- [ ] **Step 3: 实现 `shutdown_all`**

在 `impl SessionManager` 末尾(`remove` 之后)加:

```rust
    pub async fn shutdown_all(&mut self, deadline: Duration) {
        for handle in self.sessions.values() {
            *handle.end_reason.lock().unwrap() = AgentEndReason::DaemonShutdown;
            let _ = handle.cmd_tx.send(SessionCmd::Abort).await;
        }
        for handle in self.sessions.values() {
            let _ = handle.cmd_tx.send(SessionCmd::Abort).await;
        }
        let handles: Vec<(Uuid, tokio::task::JoinHandle<()>)> =
            std::mem::take(&mut self.sessions)
                .into_values()
                .map(|h| (h.id, h.join_handle))
                .collect();
        for (id, join) in handles {
            match tokio::time::timeout(deadline, join).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(session_id = %id, "shutdown deadline exceeded, force-aborting");
                    join.abort();
                }
            }
        }
    }
```

注:第二次 `cmd_tx.send(Abort)` 在第一次 send 后 channel 可能已被引擎消费且引擎正 await 中 —— 再发一次确保打断;`cmd_tx` 在循环结束 drop 后,空闲引擎的 `recv()` 返回 None 自然退出。send 失败(engine 已关)用 `let _ =` 忽略。

`use std::time::Duration;` 已在文件顶部(ConfirmConfig 用过)。

- [ ] **Step 4: 测试通过**

Run: `cargo test -p parrot-core --lib session::shutdown_tests`
Expected: PASS(2 tests)。

Run: `cargo test --workspace`
Expected: PASS。

- [ ] **Step 5: Lint + commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
git add crates/parrot-core/src/session.rs
git commit -m "feat(core): SessionManager::shutdown_all cooperative Abort + deadline"
```

---

### Task 3: `runtime::run()` 接信号 + 调 shutdown_all

**Files:**
- Modify: `crates/parrot-daemon/src/runtime.rs`

**Interfaces:**
- Consumes: Task 2 的 `shutdown_all`。
- Produces: `run()` 在 Ctrl+C/SIGTERM 时优雅退出。

- [ ] **Step 1: 改 `run()` 的 accept 循环为 select!**

把 `run_with_confirm_timeout` 里的:

```rust
    loop {
        match accept_connection(&listener).await {
            Ok(client_conn) => { ... tokio::spawn(...) }
            Err(e) => { error!(...) }
        }
    }
```

替换为:

```rust
    let shutdown = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = unix_terminate_signal() => {},
        }
    };

    loop {
        tokio::select! {
            biased;
            _ = &mut Box::pin(shutdown) => break,
            res = accept_connection(&listener) => match res {
                Ok(client_conn) => {
                    let auth = Arc::clone(&auth);
                    let session_manager = Arc::clone(&session_manager);
                    let session_store = Arc::clone(&session_store);
                    let provider_registry = Arc::clone(&provider_registry);
                    let confirm_router = Arc::clone(&confirm_router);
                    let default_config = Arc::clone(&default_config);
                    let working_dir = Arc::clone(&working_dir);
                    tokio::spawn(async move {
                        handle_connection(
                            client_conn, auth, session_manager, session_store,
                            provider_registry, confirm_router, default_config, working_dir,
                        ).await;
                    });
                }
                Err(e) => error!("Failed to accept connection: {}", e),
            },
        }
    }

    info!("Shutdown signal received, draining sessions...");
    session_manager.write().await.shutdown_all(Duration::from_secs(3)).await;
    info!("All sessions drained, exiting");
    Ok(())
}
```

`Box::pin` 是因为 `select!` 的分支 future 不能在循环里重复借用未 pinned 的 —— 第一次命中后该 future 被 consume,后续循环要重新 pin。实际上更稳的写法:把信号检测做成一个函数返回 future,循环外 `let mut shutdown = Box::pin(async {...});` 每轮 `select!` 用 `&mut shutdown`。如果编译报重复使用错误,改用下面的等价形态(更直接):

```rust
    loop {
        let sig = async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = unix_terminate_signal() => {},
            }
        };
        tokio::select! {
            _ = sig => break,
            res = accept_connection(&listener) => match res {
                Ok(client_conn) => {
                    let auth = Arc::clone(&auth);
                    let session_manager = Arc::clone(&session_manager);
                    let session_store = Arc::clone(&session_store);
                    let provider_registry = Arc::clone(&provider_registry);
                    let confirm_router = Arc::clone(&confirm_router);
                    let default_config = Arc::clone(&default_config);
                    let working_dir = Arc::clone(&working_dir);
                    tokio::spawn(async move {
                        handle_connection(
                            client_conn, auth, session_manager, session_store,
                            provider_registry, confirm_router, default_config, working_dir,
                        ).await;
                    });
                }
                Err(e) => error!("Failed to accept connection: {}", e),
            },
        }
    }
```

(每轮重新建 `sig` future —— cheap。)

- [ ] **Step 2: 加 `unix_terminate_signal` 辅助函数**

文件底部加:

```rust
async fn unix_terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}
```

Windows 上该 future 永不 ready,只有 ctrl_c 生效 —— 符合 spec(Windows 无可捕获 SIGTERM)。

- [ ] **Step 3: 编译 + 全量测试**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS。信号分支不影响测试(e2e 不发信号)。

- [ ] **Step 4: Lint + commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
git add crates/parrot-daemon/src/runtime.rs
git commit -m "feat(daemon): graceful shutdown on ctrl_c/SIGTERM via shutdown_all"
```

---

### Task 4: E2E — daemon 关闭落盘 AgentEnd

**Files:**
- Modify: `tests/integration/e2e_test.rs`

**Interfaces:**
- Consumes: Task 2 的 `shutdown_all`(经 `SessionManager` public)、Task 3 的 daemon。
- Produces: e2e 证明关闭后 events.log 末尾有 `AgentEnd{reason: DaemonShutdown}`。

- [ ] **Step 1: 写 e2e 测试**

测试不真发系统信号(脆弱),而是直接对 `SessionManager` 调 `shutdown_all`(daemon 的 `run()` 已用同一函数)。但 `run()` 是私有的内层 `session_manager` —— 走 `run_with` 起的 daemon 无法外部触达。改用:`spawn_daemon_with_provider` 起进程 → 建 session → 发 Chat 触发流式 → abort daemon task(模拟 SIGTERM 的强制路径)+ 用一个对 `shutdown_all` 的单元测试覆盖协作路径(Task 2 已做)。本 e2e 聚焦"daemon task 被 abort 后,已落盘的 events.log 在 TurnEnd 处停住且无 AgentEnd"的**回归基线**,以及"正常 drop cmd_tx 后引擎落盘 AgentEnd"的**正向**用例。

实际上最干净的正向 e2e:不通过信号,而是建 session 后**直接拿 `cmd_tx` 不发任何东西,然后 drop 它**——引擎 `recv()=None` → 落盘 AgentEnd{ClientDisconnect}。验证 events.log 末尾事件。这覆盖了优雅退出的核心路径(关通道 → 落盘),`shutdown_all` 的额外价值(Set DaemonShutdown + deadline)在 Task 2 单测已验证。

在 `e2e_test.rs` 末尾加:

```rust
#[tokio::test]
async fn e2e_engine_persists_agent_end_on_cmd_channel_close() {
    let provider = Arc::new(MockProvider::new().text_only()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) = spawn_daemon_with_provider(provider).await;

    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: Some("mock-model".to_string()),
            provider: None,
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

    tx.send(ClientMessage::Chat {
        session_id,
        message: "hi".to_string(),
    })
    .await
    .expect("send Chat");
    expect_agent_event(
        &mut rx,
        |ev| matches!(ev, AgentEvent::TurnEnd { .. }).then_some(()),
        "TurnEnd",
    )
    .await;

    drop(tx);
    tokio::time::sleep(Duration::from_millis(200)).await;
    daemon_handle.abort();

    let log_path = session_events_log_path(session_id);
    let content = std::fs::read_to_string(&log_path).expect("events.log");
    let last_line = content.lines().filter(|l| !l.is_empty()).last().expect("non-empty log");
    let entry: PersistedAgentEvent = serde_json::from_str(last_line).expect("parse last event");
    assert!(
        matches!(
            entry.event,
            AgentEvent::AgentEnd {
                reason: AgentEndReason::ClientClose | AgentEndReason::ClientDisconnect,
                ..
            }
        ),
        "expected AgentEnd as last event, got {:?}",
        entry.event
    );
}
```

`MockProvider::text_only()` 是否存在于 e2e 的 MockProvider?查 e2e_test.rs 顶部 —— e2e 的 `MockProvider` 只有 `new()`,没有 `text_only`。用 `MockProvider::new()` 即可(turn 1 会触发 echo 工具,turn 也正常结束)。把 `text_only()` 去掉:

```rust
    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
```

加辅助函数 `session_events_log_path`(e2e 里已有 `test_config` 用 data_dir,但路径不直接暴露 —— 需要从 daemon 侧读)。问题:daemon 是独立 task,它的 `data_dir` 是 `config.session.data_dir`,在 `spawn_daemon_with_provider` 里被 `std::mem::forget(tmp)` 了。测试拿不到 tmp 路径。

**修正方案:** 复用 Task 7 compaction e2e 已建立的 `spawn_daemon_with_provider_and_config` 模式 —— 用一个测试自有的 TempDir(不 forget),起 daemon 后保留 tmp,测试末尾读 `tmp.path().join("data").join("sessions").join(session_id).join("events.log")`。如果 `spawn_daemon_with_provider_and_config` 已在 e2e 文件中(来自 compaction Task 7),直接复用;否则按其形态写一个本测试内联版本。

把测试改为:

```rust
#[tokio::test]
async fn e2e_engine_persists_agent_end_on_cmd_channel_close() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut config = test_config(0, &data_dir, &token_path);

    let provider = Arc::new(MockProvider::new()) as Arc<dyn LlmProvider>;
    let (mut rx, tx, _token, daemon_handle) =
        spawn_daemon_with_provider_and_config(provider, config).await;

    tx.send(ClientMessage::CreateSession {
        config: Some(SessionConfig {
            model: Some("mock-model".to_string()),
            provider: None,
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

    tx.send(ClientMessage::Chat {
        session_id,
        message: "hi".to_string(),
    })
    .await
    .expect("send Chat");
    expect_agent_event(
        &mut rx,
        |ev| matches!(ev, AgentEvent::TurnEnd { .. }).then_some(()),
        "TurnEnd",
    )
    .await;

    drop(tx);
    tokio::time::sleep(Duration::from_millis(300)).await;
    daemon_handle.abort();

    let log_path = data_dir.join("sessions").join(session_id.to_string()).join("events.log");
    let content = std::fs::read_to_string(&log_path).expect("events.log");
    let last_line = content.lines().filter(|l| !l.is_empty()).last().expect("non-empty log");
    let entry: PersistedAgentEvent = serde_json::from_str(last_line).expect("parse last event");
    assert!(
        matches!(
            entry.event,
            AgentEvent::AgentEnd {
                reason: AgentEndReason::ClientClose | AgentEndReason::ClientDisconnect,
                ..
            }
        ),
        "expected AgentEnd as last event, got {:?}",
        entry.event
    );
}
```

确认 `spawn_daemon_with_provider_and_config` 在 e2e 文件中存在(来自 compaction Task 7);若不存在,在该测试上方加一个最小版本(按 compaction Task 7 report 的形态:capture port before move、caller owns tmp、no forget)。

- [ ] **Step 2: 验证通过**

Run: `cargo test --test e2e e2e_engine_persists_agent_end_on_cmd_channel_close`
Expected: PASS — drop(tx) → 引擎 recv None → fire_and_drop 落盘 AgentEnd → abort daemon → events.log 末尾是 AgentEnd。

Run: `cargo test --workspace`
Expected: PASS。

- [ ] **Step 3: Lint + commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
git add tests/integration/e2e_test.rs
git commit -m "test(e2e): engine persists AgentEnd when client channel closes"
```

---

## Self-Review

**Spec coverage:**
- §4.1 组件改动(JoinHandle/end_reason/shutdown_all/select+signal)→ Tasks 1,2,3。✅
- §4.2 流程(Abort → 关 cmd_tx → deadline await → abort 兜底)→ Task 2。✅
- §4.3 各引擎状态行为 → 复用现有 Abort 管道 + Task 2 单测覆盖活动/无响应两种。✅
- §5 不变量(先落盘后发、abort 仅兜底、DaemonShutdown reason)→ Tasks 1,2,4。✅
- §6 测试表 → Task 2(单元 ×2)+ Task 4(e2e)。✅
- §7 YAGNI 排除 → 无对应任务,确认未做。✅

**Placeholder scan:** 无 TBD。Task 3 的两个 select 形态是同一逻辑的等价写法,提示了编译失败时的 fallback;Task 4 给了依赖 `spawn_daemon_with_provider_and_config` 的两种处置(复用/内联)。

**Type consistency:** `SessionHandle` 字段名 `join_handle`/`end_reason` 在 Tasks 1,2 一致;`with_end_reason` 签名一致;`shutdown_all(deadline: Duration)` 一致;`AgentEndReason::DaemonShutdown` 已存在,无需新增。

**已知限制:** e2e 不发真实系统信号(跨平台脆弱),用 drop(cmd_tx) 验证优雅路径 + Task 2 单测验证 shutdown_all 的 DaemonShutdown 设置与 deadline 约束;真实信号 → shutdown_all 的端到端链路由代码路径连通(Task 3)但无 e2e 网络层覆盖,留作手动验证。
