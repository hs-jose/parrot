# Parrot — 命名统一详设

> 对应问题 3：`server` 与 `daemon` 术语混用。
>
> crate 拆分（问题 2）完成后，混乱已收敛到一处：
> `parrot-daemon` 的业务主循环文件仍叫 `server.rs`，
> 导致 `parrot_daemon::server::run_with(...)` 这个调用路径
> 无法区分"传输层的 WS server"和"daemon 业务逻辑"两个概念。
>
> 本批变更极小：**5 个文件，逻辑零变更。**

---

## 1. 诊断

### 1.1 crate 拆分后仅剩一处混乱

```
parrot-transport/src/ws_server.rs   ← "server" = WS 传输端点   ✅ 正确
parrot-daemon/src/server.rs         ← "server" = daemon 业务逻辑  ❓ 混淆
```

测试调用路径：

```rust
parrot_daemon::server::run_with(config, auth, ...)
```

`parrot_daemon` 已经表达了"这是 daemon 层"，`::server::` 在中间完全多余，
且和 `parrot_transport::WsTransportServer` 的"server"语义不同，却使用了同一个词。

### 1.2 不需要动的东西

下表列出看起来涉及 server/daemon 混用、但**不应修改**的标识符：

| 标识符 | 位置 | 不动的理由 |
|--------|------|----------|
| `ServerMessage` / `ClientMessage` | `parrot-protocol` | wire 协议惯例——发往客户端的叫 ServerMessage，这是标准命名 |
| `TransportServer` / `WsTransportServer` | `parrot-transport` | transport 层 endpoint，"server" 描述的是传输角色，正确 |
| `server_version` in `HelloAck` | `parrot-protocol` | wire 字段名，改动破坏协议兼容性 |
| `"Connected to server v{}"` | `src/cli/main.rs` | 面向用户的提示语，用户视角"连接到服务器"是自然表达 |
| `"Parrot daemon starting on ..."` | `parrot-daemon/src/server.rs` | 日志内容已经用了 daemon，正确 |
| `tracing` filter `"parrotd=info"` | `src/daemon/main.rs` | binary 名，正确 |

---

## 2. 变更内容

### 2.1 `server.rs` → `runtime.rs`

`crates/parrot-daemon/src/server.rs` 改名为 `runtime.rs`。

"runtime"准确描述这个文件的职责：daemon 的运行时主循环——
接受 WS 连接、分发消息、管理 session 生命周期。
它不是传输层的"server"，而是整个 daemon 进程的运行时。

```
crates/parrot-daemon/src/
  server.rs   →  runtime.rs      # git mv
  lib.rs      →  pub mod 从 server 改为 runtime（其余不变）
```

### 2.2 `lib.rs` 更新

```rust
// 变更前
pub mod auth;
pub mod server;
pub mod session_store;

pub use server::{run, run_with, run_with_confirm_timeout};

// 变更后
pub mod auth;
pub mod runtime;
pub mod session_store;

pub use runtime::{run, run_with, run_with_confirm_timeout};
```

`run` / `run_with` / `run_with_confirm_timeout` 的 re-export 路径不变，
调用方只写 `parrot_daemon::run_with(...)` 就够了，不需要知道内部模块名。

### 2.3 测试调用路径更新（4 处）

| 文件 | 变更前 | 变更后 |
|------|--------|--------|
| `tests/integration/e2e_test.rs`（第 328 行） | `parrot_daemon::server::run_with` | `parrot_daemon::run_with` |
| `tests/integration/e2e_test.rs`（第 585 行） | `parrot_daemon::server::run_with` | `parrot_daemon::run_with` |
| `tests/integration/phase15_test.rs`（第 272 行） | `parrot_daemon::server::run_with_confirm_timeout` | `parrot_daemon::run_with_confirm_timeout` |
| `tests/cassette_test.rs`（第 419 行） | `parrot_daemon::server::run_with` | `parrot_daemon::run_with` |

改完后调用统一为顶层路径，`::server::` 中间层彻底消失。

---

## 3. 统一后的术语表

本批完成后，整个代码库的 server/daemon 用法如下：

| 术语 | 含义 | 使用位置 |
|------|------|---------|
| **daemon** | 长驻后台进程的身份 | binary 名 `parrotd`、crate 名 `parrot-daemon`、配置字段 `config.daemon.*`、日志 "Parrot daemon starting" |
| **server**（传输层） | WS 传输端点，接受 TCP 连接并升级为 WS | `parrot-transport` 内部：`WsTransportServer`、`TransportServer` trait、`ws_server.rs` |
| **server**（协议层） | 消息方向标识 | `ServerMessage`（daemon 发往 client）、`server_version`（HelloAck 字段） |
| **runtime** | daemon 的运行时主循环 | `parrot-daemon/src/runtime.rs`（本批新名） |

---

## 4. 变更文件清单

| 文件 | 操作 | 变更内容 |
|------|------|---------|
| `crates/parrot-daemon/src/server.rs` | **重命名** → `runtime.rs` | 文件名，内容不动 |
| `crates/parrot-daemon/src/lib.rs` | 编辑 | `pub mod server` → `pub mod runtime`；`use server::` → `use runtime::` |
| `tests/integration/e2e_test.rs` | 编辑 | 2 处 `parrot_daemon::server::run_with` → `parrot_daemon::run_with` |
| `tests/integration/phase15_test.rs` | 编辑 | 1 处 `parrot_daemon::server::run_with_confirm_timeout` → `parrot_daemon::run_with_confirm_timeout` |
| `tests/cassette_test.rs` | 编辑 | 1 处 `parrot_daemon::server::run_with` → `parrot_daemon::run_with` |

---

## 5. 实施步骤

```
1. git mv crates/parrot-daemon/src/server.rs crates/parrot-daemon/src/runtime.rs
2. 编辑 lib.rs（2 行改动）
3. 编辑 3 个测试文件（共 4 处）
4. cargo test — 全绿
```

---

## 6. 范围

| 子项 | 状态 |
|------|------|
| `server.rs` → `runtime.rs` 重命名 | ✅ |
| `lib.rs` 模块引用更新 | ✅ |
| 测试调用路径更新（4 处） | ✅ |
| `cargo test` 全绿 | ✅ |
