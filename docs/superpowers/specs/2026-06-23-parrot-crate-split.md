# Parrot — Crate 拆分详设

> 对应问题 2：架构划分不清晰。`provider` 实现混在 `src/daemon/`、
> `src/lib.rs` 靠 `#[path]` hack 重新导出 daemon 模块给测试用、
> daemon binary 和 lib 逻辑混在同一目录。
>
> 本文档规划新 crate 布局，明确每个 crate 的边界、依赖图、迁移步骤。
> 命名统一（问题 3）在本批完成后单独处理。

---

## 1. 诊断：四个具体问题

### 1.1 `#[path]` hack 是边界破洞

`src/lib.rs`：

```rust
#[path = "daemon/auth.rs"]       pub mod auth;
#[path = "daemon/server.rs"]     pub mod server;
#[path = "daemon/session_store.rs"] pub mod session_store;
#[path = "daemon/providers/mod.rs"] pub mod providers;
#[path = "daemon/tools/mod.rs"]  pub mod tools;
```

集成测试通过 `parrot::server::run_with(...)` 启动 daemon。`#[path]`
让同一份源文件被两个 crate（`parrot` lib + `parrotd` bin）分别编译，
编译器合法但语义混乱——类型既属于 `parrot::server::...` 也属于
daemon 的内部 `server::...`，不是同一个类型系统。

### 1.2 Provider / Tool 实现无法独立测试

`cargo test -p parrot-providers` 不存在。测试 Anthropic SSE 解析需要
跑整个 `parrot` 包或写 e2e 测试。将来要加 OpenAI / Ollama provider，
每个 provider 都要在同一 `anthropic.rs` 旁边新增文件，没有隔离。

### 1.3 `src/daemon/main.rs` 是目前唯一的「daemon binary」入口

18 行，非常薄。但 daemon 的**全部业务逻辑**（session、auth、WS 管理）
在 `server.rs`（577 行），而 `server.rs` 既是 lib 又是 bin 的一部分，
没有清晰的 lib API。

### 1.4 Workspace 根 `Cargo.toml` 承载所有依赖

`reqwest`（HTTP）、`rustyline`（CLI REPL），全部在同一个 `[dependencies]`
下，daemon 和 CLI 不分彼此。

---

## 2. 目标布局

### 2.1 最终 workspace 成员

```
crates/
  parrot-protocol/      # 纯数据类型，无 IO              (现有，不动)
  parrot-config/        # 配置加载                        (现有，不动)
  parrot-transport/     # WS 传输抽象                     (现有，不动)
  parrot-core/          # 引擎 + trait 定义，无具体实现   (现有，不动)
  parrot-providers/     # ← 新：LlmProvider 具体实现
  parrot-tools/         # ← 新：内置 Tool 具体实现
  parrot-daemon/        # ← 新：daemon lib (server/auth/session_store)

src/
  daemon/main.rs        # ← 剩下 ~10 行的 binary 入口
  cli/main.rs           # 不动（CLI binary，Phase 2 再拆）
  lib.rs                # ← 删除（#[path] hack 的源头）
```

### 2.2 依赖图

```
parrot-protocol
    ↑
parrot-core          parrot-config
    ↑                     ↑
parrot-providers    parrot-tools    parrot-transport
         ↑                ↑              ↑
            parrot-daemon ────────────────
                    ↑
             src/daemon/main.rs  (binary)
             tests/integration/  (dev-dep only)
```

- **无反向依赖**：`parrot-core` 不依赖 `parrot-providers`/`parrot-tools`/`parrot-daemon`。
- `src/cli/main.rs` 不变，仍直接依赖 `parrot-transport`/`parrot-protocol`/`parrot-config`。

---

## 3. 各 crate 详细说明

### 3.1 `crates/parrot-providers`

**职责：** `LlmProvider` trait 的具体实现，即所有 HTTP/SSE 网络 IO。

**迁移来源：**

| 原路径 | 新路径 |
|--------|--------|
| `src/daemon/providers/anthropic.rs` | `crates/parrot-providers/src/anthropic.rs` |
| `src/daemon/providers/retry.rs` | `crates/parrot-providers/src/retry.rs` |
| `src/daemon/providers/mod.rs` | `crates/parrot-providers/src/lib.rs` |

**对外 API（`pub`）：**

```rust
// crates/parrot-providers/src/lib.rs
pub mod anthropic;
pub mod retry;

// 注册函数——daemon 启动时调用
pub async fn register_all(
    registry: &parrot_core::provider::ProviderRegistry,
    config: &parrot_config::AppConfig,
);
```

`register_all` 是从 `src/daemon/providers/mod.rs` 直接搬来的，签名不变。

**`Cargo.toml` 依赖：**

```toml
[dependencies]
parrot-core     = { path = "../parrot-core" }
parrot-config   = { path = "../parrot-config" }
parrot-protocol = { path = "../parrot-protocol" }
tokio           = { workspace = true }
reqwest         = { workspace = true }
serde           = { workspace = true }
serde_json      = { workspace = true }
async-trait     = { workspace = true }
tracing         = { workspace = true }
rand            = { workspace = true }   # retry jitter
futures-util    = { workspace = true }
async-stream    = { workspace = true }
```

**不向外暴露的内部类型：** `AnthropicRequest`/`AnthropicResponse`/SSE 帧类型。

---

### 3.2 `crates/parrot-tools`

**职责：** `Tool` trait 的具体实现，即所有文件系统 / shell / HTTP IO。

**迁移来源：**

| 原路径 | 新路径 |
|--------|--------|
| `src/daemon/tools/file_read.rs` | `crates/parrot-tools/src/file_read.rs` |
| `src/daemon/tools/file_write.rs` | `crates/parrot-tools/src/file_write.rs` |
| `src/daemon/tools/file_glob.rs` | `crates/parrot-tools/src/file_glob.rs` |
| `src/daemon/tools/file_grep.rs` | `crates/parrot-tools/src/file_grep.rs` |
| `src/daemon/tools/shell_exec.rs` | `crates/parrot-tools/src/shell_exec.rs` |
| `src/daemon/tools/web_fetch.rs` | `crates/parrot-tools/src/web_fetch.rs` |
| `src/daemon/tools/web_search.rs` | `crates/parrot-tools/src/web_search.rs` |
| `src/daemon/tools/mod.rs` | `crates/parrot-tools/src/lib.rs` |

**对外 API（`pub`）：**

```rust
pub mod file_read;
pub mod file_write;
pub mod file_glob;
pub mod file_grep;
pub mod shell_exec;
pub mod web_fetch;
pub mod web_search;

// daemon 启动时调用
pub async fn register_all(
    registry: &parrot_core::tool::ToolRegistry,
    config: &parrot_config::AppConfig,
);
```

**`Cargo.toml` 依赖：**

```toml
[dependencies]
parrot-core     = { path = "../parrot-core" }
parrot-config   = { path = "../parrot-config" }
parrot-protocol = { path = "../parrot-protocol" }
tokio           = { workspace = true, features = ["full"] }
reqwest         = { workspace = true }   # web_fetch / web_search
serde           = { workspace = true }
serde_json      = { workspace = true }
async-trait     = { workspace = true }
tracing         = { workspace = true }
glob            = { workspace = true }   # file_glob
regex           = { workspace = true }   # file_grep
```

---

### 3.3 `crates/parrot-daemon`

**职责：** daemon 的业务 lib——WS 服务主循环、auth、session 持久化。
binary 入口 `src/daemon/main.rs` 只调用这里的 `run()` 函数。

**迁移来源：**

| 原路径 | 新路径 |
|--------|--------|
| `src/daemon/server.rs` | `crates/parrot-daemon/src/server.rs` |
| `src/daemon/auth.rs` | `crates/parrot-daemon/src/auth.rs` |
| `src/daemon/session_store.rs` | `crates/parrot-daemon/src/session_store.rs` |

`src/daemon/providers/` 和 `src/daemon/tools/` 已迁移到各自 crate，原目录删除。

**对外 API（`pub`）：**

```rust
// crates/parrot-daemon/src/lib.rs
pub mod auth;
pub mod server;
pub mod session_store;

// §3.3 public API 的 re-export——给 binary 和测试用
pub use server::{run, run_with, run_with_confirm_timeout};

// 给 binary 用的主入口
pub async fn run(config: parrot_config::AppConfig)
    -> Result<(), Box<dyn std::error::Error>>;

// 给集成测试用的可注入入口（现在是 pub，不再需要 #[path] hack）
pub async fn run_with(
    config: parrot_config::AppConfig,
    auth: Arc<auth::Auth>,
    provider_registry: Arc<parrot_core::provider::ProviderRegistry>,
    tool_registry: Arc<parrot_core::tool::ToolRegistry>,
) -> Result<(), Box<dyn std::error::Error>>;

pub async fn run_with_confirm_timeout(
    config: parrot_config::AppConfig,
    auth: Arc<auth::Auth>,
    provider_registry: Arc<parrot_core::provider::ProviderRegistry>,
    tool_registry: Arc<parrot_core::tool::ToolRegistry>,
    confirm_timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>>;
```

这三个函数就是从 `src/daemon/server.rs` 直接搬来的（签名完全一致），
现在是 lib crate 的 public API。

**`Cargo.toml` 依赖：**

```toml
[dependencies]
parrot-core      = { path = "../parrot-core" }
parrot-protocol  = { path = "../parrot-protocol" }
parrot-config    = { path = "../parrot-config" }
parrot-transport = { path = "../parrot-transport" }
parrot-providers = { path = "../parrot-providers" }
parrot-tools     = { path = "../parrot-tools" }
tokio            = { workspace = true }
serde            = { workspace = true }
serde_json       = { workspace = true }
tracing          = { workspace = true }
uuid             = { workspace = true }
chrono           = { workspace = true }
rand             = { workspace = true }   # auth token 生成
hex              = { workspace = true }   # auth token hex 编码
```

> `anyhow` 不在 `parrot-daemon` 的依赖里——`AGENTS.md` 规定 `anyhow` 只允许在
> binary 中使用，`parrot-daemon` 是 lib crate，其 public API 返回
> `Box<dyn std::error::Error>`。

---

## 4. Binary 入口瘦身

### 4.1 `src/daemon/main.rs`（保留，瘦身后约 12 行）

```rust
use parrot_config::AppConfig;
use parrot_daemon::run;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("parrotd=info".parse()?),
        )
        .init();
    let config = AppConfig::load().map_err(|e| format!("Config error: {e}"))?;
    run(config).await
}
```

`src/daemon/` 目录改造后仅保留 `main.rs`，其余文件全部迁移到 crates。

### 4.2 `src/cli/main.rs`

不动。Phase 2 可以抽 `crates/parrot-cli/`，但本批不做。

---

## 5. `src/lib.rs` 删除 + 测试迁移

### 5.1 问题现状

集成测试（`tests/integration/e2e_test.rs`、`phase15_test.rs`）通过
`parrot::server::run_with(...)` 启动 daemon in-process。这要求
`src/lib.rs` 存在并暴露 daemon internals。

### 5.2 迁移后

`parrot-daemon` 是 lib crate，`run_with` 是它的 public API。
集成测试通过 `[dev-dependencies]` 使用它：

**根 `Cargo.toml` 的 `[dev-dependencies]`：**

```toml
[dev-dependencies]
parrot-daemon    = { path = "crates/parrot-daemon" }
parrot-core      = { path = "crates/parrot-core" }
parrot-config    = { path = "crates/parrot-config" }
parrot-protocol  = { path = "crates/parrot-protocol" }
parrot-transport = { path = "crates/parrot-transport" }
tempfile         = "3"
```

**集成测试 import 变化（唯一改动）：**

```rust
// 改造前
use parrot::server;
use parrot::auth::Auth;

// 改造后
use parrot_daemon::server;
use parrot_daemon::auth::Auth;
```

`src/lib.rs` 整个删除，`#[path]` hacks 归零。

### 5.3 `cassette_test.rs` 也需要迁移

`tests/cassette_test.rs` 目前通过 `parrot::auth::Auth` 和
`parrot::server::run_with` 启动 daemon，同样需要改为 `parrot_daemon::*`。
迁移后 `src/lib.rs` 被删除，所有经 `parrot::*` 的集成测试路径都需要更新。

---

## 6. 根 `Cargo.toml` 瘦身

### 6.1 `[dependencies]` 变化

迁移前根 `[dependencies]` 包含 daemon 和 CLI 所有依赖（reqwest、glob 等），
因为 `src/lib.rs` 把 daemon 模块编进了 lib crate。

迁移后 `parrot` 根 package 只剩 binary-glue 依赖，实际上可以是极简的：

```toml
[dependencies]
# parrotd binary 只依赖 parrot-daemon（它自己拉 transitive deps）
parrot-daemon      = { path = "crates/parrot-daemon" }
parrot-config      = { path = "crates/parrot-config" }
tracing-subscriber = { workspace = true }

# parrot CLI binary 依赖 transport + protocol + config
parrot-transport   = { path = "crates/parrot-transport" }
parrot-protocol    = { path = "crates/parrot-protocol" }
tokio              = { workspace = true }
clap               = { workspace = true }
rustyline          = { workspace = true }
atty               = { workspace = true }
uuid               = { workspace = true }
```

reqwest、glob、regex、rand 等 **完全从根 `[dependencies]` 移除**，
只出现在各自负责的 crate 的 `Cargo.toml` 里。

---

## 7. workspace members 更新

```toml
[workspace]
resolver = "2"
members = [
    "crates/parrot-protocol",
    "crates/parrot-config",
    "crates/parrot-transport",
    "crates/parrot-core",
    "crates/parrot-providers",   # ← 新
    "crates/parrot-tools",       # ← 新
    "crates/parrot-daemon",      # ← 新
]
```

---

## 8. 关键取舍

### 8.1 为什么 `parrot-providers` 和 `parrot-tools` 分开而不合并成 `parrot-impls`？

| 理由 | 说明 |
|------|------|
| **trait 边界清晰** | 一个实现 `LlmProvider`，一个实现 `Tool`，概念上无重叠 |
| **依赖不同** | providers 需要 `rand`（retry jitter）；tools 需要 `glob`/`regex`/`tokio::process`，不应污染 providers |
| **独立测试** | `cargo test -p parrot-providers` vs `cargo test -p parrot-tools` |
| **未来扩展** | 未来 MCP server 只需依赖 `parrot-tools`，不需要 `parrot-providers` |

### 8.2 为什么 `parrot-daemon` 是 lib 而不是直接 binary？

- **in-process 集成测试**：测试可以直接调用 `parrot_daemon::run_with(mock_provider)`，无需 WS 连接，速度快。
- **未来的 embedding API**：其他 Rust 程序可以把 parrot daemon 作为库引入。
- **可单独测试**：`cargo test -p parrot-daemon` 覆盖 auth、session_store、server 业务逻辑。

### 8.3 为什么 CLI 不同步拆？

`src/cli/main.rs`（638 行）包含 clap 子命令展开、交互式 REPL、流式事件渲染——
这些逻辑在测试中从不被注入、也不依赖 daemon 的 `#[path]` hack。拆 CLI 不解决
任何当前存在的边界问题，收益小、噪音大。推迟到 Phase 2 视 TUI 工作一起处理。

### 8.4 为什么 trait 不放在 `parrot-providers` 或 `parrot-tools`？

直觉上"trait 和实现放一起"看起来内聚，但会造成依赖方向倒置：

若 `LlmProvider` trait 放在 `parrot-providers`，引擎（`parrot-core`）要调用
`chat_stream` 就必须依赖 providers——编译时把整个 Anthropic reqwest 客户端拉进引擎，
mock provider 测试也要编译网络 IO。`Tool` trait 同理。

trait 必须放在**同时被引擎层和实现层依赖，而本身不依赖任何一方**的位置：

```
trait 所在层        ← 两个选项
   ↑        ↑
引擎层    实现层
```

两个合法选项：

| 选项 | 代价 |
|------|------|
| **现状：trait 留在 `parrot-core`**（引擎同住） | providers/tools 编译时带上引擎代码，当前规模无实际痛点 |
| **未来：新增 `parrot-api`**（只放 trait + 关联类型，零实现） | 多一个 crate，需决定哪些类型属于这层 |

当前选择现状。若未来 providers/tools 编译时间成为问题，可以把 `LlmProvider` /
`Tool` / `ChatMessage` / `ToolDefinition` 等提取到 `parrot-api`，让 providers
和 tools 只依赖薄接口层而非整个引擎。届时再做，不增加当前复杂度。

### 8.5 `parrot-daemon` 中是否应该内联 `providers`/`tools`？

**不应该**。如果把 providers/tools 注册逻辑放进 daemon，daemon 就无法被测试时
替换成 mock——这是当前 `#[path]` hack 要解决的同一个问题。
daemon 依赖 `parrot-providers`/`parrot-tools`，在 `run()` 里调用
`register_all`；测试调用 `run_with(mock_registry)` 绕过注册，
这个 DI 边界是整个测试策略的基础。

---

## 9. 文件移动清单

所有移动均为**纯搬迁（不修改逻辑）**，只改 module path 和 Cargo.toml。

| 原路径 | 新路径 | 行动 |
|--------|--------|------|
| `src/daemon/providers/anthropic.rs` | `crates/parrot-providers/src/anthropic.rs` | 移动 |
| `src/daemon/providers/retry.rs` | `crates/parrot-providers/src/retry.rs` | 移动 |
| `src/daemon/providers/mod.rs` | `crates/parrot-providers/src/lib.rs` | 移动 + 改模块路径 |
| `src/daemon/tools/file_read.rs` | `crates/parrot-tools/src/file_read.rs` | 移动 |
| `src/daemon/tools/file_write.rs` | `crates/parrot-tools/src/file_write.rs` | 移动 |
| `src/daemon/tools/file_glob.rs` | `crates/parrot-tools/src/file_glob.rs` | 移动 |
| `src/daemon/tools/file_grep.rs` | `crates/parrot-tools/src/file_grep.rs` | 移动 |
| `src/daemon/tools/shell_exec.rs` | `crates/parrot-tools/src/shell_exec.rs` | 移动 |
| `src/daemon/tools/web_fetch.rs` | `crates/parrot-tools/src/web_fetch.rs` | 移动 |
| `src/daemon/tools/web_search.rs` | `crates/parrot-tools/src/web_search.rs` | 移动 |
| `src/daemon/tools/mod.rs` | `crates/parrot-tools/src/lib.rs` | 移动 + 改模块路径 |
| `src/daemon/server.rs` | `crates/parrot-daemon/src/server.rs` | 移动 |
| `src/daemon/auth.rs` | `crates/parrot-daemon/src/auth.rs` | 移动 |
| `src/daemon/session_store.rs` | `crates/parrot-daemon/src/session_store.rs` | 移动 |
| `src/daemon/main.rs` | `src/daemon/main.rs` | 保留，瘦身为 ~12 行 |
| `src/lib.rs` | — | **删除** |
| `src/daemon/providers/` | — | 目录删除（文件已移走） |
| `src/daemon/tools/` | — | 目录删除（文件已移走） |

---

## 10. 测试

| 测试 | 类型 | 覆盖点 | 改动 |
|------|------|--------|------|
| 所有已有测试 | — | — | import 从 `parrot::` 改 `parrot_daemon::` / `parrot_providers::` |
| `cargo test -p parrot-providers` | unit | Anthropic SSE 解析 + retry 逻辑 | 新增（之前无法单独跑） |
| `cargo test -p parrot-tools` | unit | 文件工具 POSIX 行为，shell denylist | 新增 |
| `cargo test -p parrot-daemon` | unit | auth token 生成/验证，session_store CRUD | 新增 |
| `tests/integration/e2e_test.rs` | integration | 端到端行为不变 | import 修改 |
| `tests/integration/phase15_test.rs` | integration | 端到端行为不变 | import 修改 |
| `tests/cassette_test.rs` | integration | cassette 回放行为不变 | import 修改（`parrot::` → `parrot_daemon::`） |
| `crates/parrot-core/tests/` | unit | 引擎 / session 逻辑 | **不改** |

---

## 11. 实施顺序

顺序设计原则：每步独立可编译，不留中间态破损。

```
步骤 1 — 建 crates/parrot-providers
  新建目录 + Cargo.toml
  移动 anthropic.rs / retry.rs / mod.rs → lib.rs
  调整 use 路径（super:: → crate::）
  workspace 加入新成员
  cargo build -p parrot-providers 通过

步骤 2 — 建 crates/parrot-tools
  新建目录 + Cargo.toml
  移动所有 tool 文件 → src/
  调整 use 路径
  cargo build -p parrot-tools 通过

步骤 3 — 建 crates/parrot-daemon
  新建目录 + Cargo.toml（deps 含 parrot-providers + parrot-tools）
  新建 src/lib.rs：pub mod auth; pub mod server; pub mod session_store;
                   pub use server::{run, run_with, run_with_confirm_timeout};
  移动 server.rs / auth.rs / session_store.rs
  调整 server.rs 内对 providers::register_all / tools::register_all 的引用
    → 改为 parrot_providers::register_all / parrot_tools::register_all
  cargo build -p parrot-daemon 通过

步骤 4 — 瘦身 src/daemon/main.rs
  替换为 ~12 行（use parrot_daemon::run; main()）
  cargo build --bin parrotd 通过

步骤 5 — 更新根 Cargo.toml
  移除已不属于根的 deps（reqwest / glob / regex ...）
  更新 [dev-dependencies] 加入 parrot-daemon
  cargo build 通过

步骤 6 — 删除 src/lib.rs
  修改集成测试 imports：parrot:: → parrot_daemon:: / parrot_providers::
  cargo test 全部通过

步骤 7 — 删除 src/daemon/providers/ 和 src/daemon/tools/ 目录
  确认无残留引用
  cargo test 通过，全绿
```

每步结束后执行 `cargo test`；失败不进下一步。

---

## 12. 范围

| 子项 | 状态 |
|------|------|
| 建 `parrot-providers`，迁移 Anthropic adapter | ✅ |
| 建 `parrot-tools`，迁移内置工具 | ✅ |
| 建 `parrot-daemon` lib，迁移 server/auth/session_store | ✅ |
| 瘦身 `src/daemon/main.rs` | ✅ |
| 删 `src/lib.rs`，更新集成测试 imports | ✅ |
| 清理 src/daemon/ 残留目录 | ✅ |
| 根 Cargo.toml 依赖瘦身 | ✅ |
| `cargo test -p parrot-providers/tools/daemon` 新增 | ✅ |

---

## 13. 不在本批的事项

- **CLI 拆分**（`crates/parrot-cli/`）：Phase 2，和 TUI 一起处理
- **命名统一**（server vs daemon 术语）：问题 3，最后一步
- **新增 provider**（OpenAI / Ollama）：本批完成后加入 `parrot-providers` 很自然
- **MCP server**：届时可依赖 `parrot-tools` 不依赖 `parrot-daemon`
