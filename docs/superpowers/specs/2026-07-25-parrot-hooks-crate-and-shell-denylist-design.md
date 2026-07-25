# Parrot Hooks Crate Refactor + `shell_denylist` Hook

> 地基重构：把 daemon 内置 hook 实现抽到独立 `parrot-hooks` crate，每个 hook 自带 typed config；
> 第一个落地的新 hook `shell_denylist` 接管 `ShellExecTool` 的 denylist 职责，并把
> `dangerous_command_blocker` 纳入默认 `enabled`。
>
> 对应 idea 备忘录的多条「可扩展性」分支；本轮只做 crate 拆分 + 第一个 hook 迁移，
> `redact_secrets` 扩充 / `context_ready` 等后续 hook 在各自独立 spec 中处理。

## 1. 背景与目标

Parrot 生命週期 hook 系统已落地（见 `2026-07-25-parrot-lifecycle-hooks-design.md`），但当前
所有内置 hook 实现住在 `crates/parrot-daemon/src/hooks/`，配置仅靠 `[hooks].enabled` 单一开关，
没有 per-hook 参数。本轮针对三个即将到来的新 hook（`shell_denylist`、`redact_secrets` 扩充、
`context_ready`）做地基重构：

1. **新建 `parrot-hooks` crate**——把所有内置 hook 集中到独立 crate，daemon 仅引用。
2. **per-hook config 容器**——`HooksConfig` 持 `HashMap<String, toml::Value>`，每个 hook
   在新 crate 内自带 typed config struct，从对应子表反序列化。
3. **`shell_denylist` hook**——迁移 `ShellExecTool::denylist` 字符串黑名单到 hook 层，
   让工具回归「纯执行」。
4. **`dangerous_command_blocker` 加入默认 enabled**——把现有最常用的 bail hook
   纳入零配置默认安全策略。

`redact_secrets` 扩充和 `context_ready` hook point 不在本轮；它们各自有独立 spec，但都会
复用本轮建立的 `parrot-hooks` crate 与 per-hook config 容器机制。

## 2. 设计原则

1. **单一职责的 crate**：`parrot-hooks` 只放内置 hook 实现 + 它们的 typed config +
   `build_registry()` 注册入口；不含 IO 编排、不含 daemon 启动逻辑。
2. **配置与实现就近**：每个 hook 的 config struct 与 hook impl 在同一模块，便于加新 hook
   时只动一个文件。
3. **config crate 不识别具体 hook**：`parrot-config::HooksConfig` 只持 `enabled` /
   `timeout_seconds` / `configs: HashMap<String, toml::Value>`，不引用任何具体 hook 类型。
   加新 hook 不需要改 `parrot-config`。
4. **默认安全**：新装使用者拿到 `parrot.toml` 即开 `dangerous_command_blocker`
   + `shell_denylist` 默认双防护；`redact_secrets` 等可能误脱敏的 hook 保持 opt-in。
5. **微创**：不重构生命週期 hook 调度核心（`HookRegistry::run`）；不修 engine 主干；
   `build_registry` 签名保持不变，只是换了宿主 crate。

## 3. 新 crate `parrot-hooks`

### 3.1 文件布局

```
crates/parrot-hooks/
  Cargo.toml
  src/
    lib.rs                              # pub use 重导出 + pub fn build_registry()
    dangerous_command_blocker.rs        # 从 parrot-daemon/src/hooks/ 搬来
    redact_secrets.rs                   # 从 parrot-daemon/src/hooks/ 搬来（不改逻辑）
    shell_denylist.rs                   # 新增
```

### 3.2 依赖

```toml
[dependencies]
parrot-core   = { path = "../parrot-core" }
parrot-config = { path = "../parrot-config" }
async-trait   = { workspace = true }
regex         = "1"
serde         = { workspace = true }
toml          = { workspace = true }   # 反序列化 per-hook config 子表
tracing       = { workspace = true }
```

`regex`、`async-trait` 等「hook 专用」依赖从 `parrot-daemon` 的 `Cargo.toml` 移除（如果
daemon 本身不再需要），daemon 只多一个 `parrot-hooks` 依赖。Workspace `Cargo.toml` 的
`[workspace.dependencies]` 视情况补 `parrot-hooks` 别名；member 列表加入新 crate。

### 3.3 `build_registry` 迁移

`pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry>` 从 `parrot-daemon/src/hooks/mod.rs`
迁到 `parrot-hooks/src/lib.rs`，签名不变。流程：

```rust
pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let mut reg = HookRegistry::new(Duration::from_secs(cfg.timeout_seconds.max(1)));
    for id in &cfg.enabled {
        match id.as_str() {
            "dangerous_command_blocker" => {
                let _cfg = cfg.configs.get("dangerous_command_blocker");
                reg.register(Arc::new(DangerousCommandBlocker));
            }
            "redact_secrets" => {
                let hook_cfg: RedactSecretsConfig = cfg.configs.get("redact_secrets")
                    .and_then(|v| RedactSecretsConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(RedactSecrets::new(hook_cfg)));
            }
            "shell_denylist" => {
                let hook_cfg: ShellDenylistConfig = cfg.configs.get("shell_denylist")
                    .and_then(|v| ShellDenylistConfig::deserialize(v.clone()).ok())
                    .unwrap_or_default();
                reg.register(Arc::new(ShellDenylist::new(hook_cfg)));
            }
            other => warn!("unknown hook id in [hooks].enabled: {other} (skipping)"),
        }
    }
    Arc::new(reg)
}
```

`parrot-daemon/src/runtime.rs` 改为 `use parrot_hooks::build_registry;`，移除
`use crate::hooks::build_registry;`。`parrot-daemon/src/lib.rs` 的 `pub mod hooks;` 删除，
整个 `crates/parrot-daemon/src/hooks/` 目录删除。

### 3.4 现有 hook 搬家

`dangerous_command_blocker.rs` 和 `redact_secrets.rs` 的逻辑本轮**不动**——只换宿主 crate。
唯一允许的改动：`redact_secrets.rs` 从 unit struct 改为带 config 的结构体——
`pub struct RedactSecrets { cfg: RedactSecretsConfig }` + `RedactSecrets::new(cfg)`，
本轮 `RedactSecretsConfig` 字段集合为空（`extra_patterns` 等扩展字段在 #2 spec 加），
`#[derive(Default)]` 给出空配置，行为与今天一致。`DangerousCommandBlocker` 维持 unit struct
无 config 构造。

## 4. per-hook config 容器

### 4.1 `parrot-config::HooksConfig`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_seconds: u64,
    #[serde(default, flatten)]
    pub configs: HashMap<String, toml::Value>,
}
```

**已知实现风险**：`#[serde(flatten)]` + `HashMap<String, toml::Value>` + `toml` 的组合
在 toml-rs 老版本上有边界 case。初步在 plan 阶段第一步加一个 deserialize 单元测试验证
`[hooks.shell_denylist].denylist` 能正确进入 `configs["shell_denylist"]`；若失败，回退到
对 `HooksConfig` 手写 `Deserialize`（从 `toml::Value::Table` 抽 `enabled`/`timeout_seconds`，
剩余 keys 进 `configs`）。本设计不强推某个反序列化路径；「`configs` 接住子表」是接口契约。

### 4.2 TOML 示例

```toml
[hooks]
enabled = ["shell_denylist", "dangerous_command_blocker"]
timeout_seconds = 5

[hooks.shell_denylist]
denylist = ["rm -rf /", "sudo", "chmod 777"]

[hooks.redact_secrets]        # 未启用，配置也无害
extra_patterns = ["MY-CUSTOM-SECRET-\\d+"]
```

### 4.3 每个 hook 自带 typed config

每个 hook 模块内声明自己的 `Config` struct + `Default`，`Config::deserialize(toml::Value)`
对应 `[hooks.<id>]` 子表；缺失或空 table → `Default`。

```rust
// crates/parrot-hooks/src/shell_denylist.rs
#[derive(Debug, Clone, Deserialize)]
pub struct ShellDenylistConfig {
    #[serde(default = "default_denylist")]
    pub denylist: Vec<String>,
}
fn default_denylist() -> Vec<String> {
    vec!["rm -rf /".into(), "sudo".into(), "chmod 777".into()]
}
impl Default for ShellDenylistConfig {
    fn default() -> Self {
        Self { denylist: default_denylist() }
    }
}
```

本轮 `ShellDenylistConfig` 是唯一有字段的 hook config；`RedactSecretsConfig`
本轮 `Default` 给空结构（`extra_patterns` 字段在 #2 spec 加）；`DangerousCommandBlocker`
无 config 参数。

> **重要**：`ShellDenylistConfig` 必须**手写** `Default` impl 调用 `default_denylist()`，
> 不能用 `#[derive(Default)]`（派生的 `Default` 会给 `denylist = Vec::new()` 空数组，
> 达不到「缺失 subtable → 3 条 baseline」的契约）。同理 `RedactSecretsConfig` 默认空结构，
> `#[derive(Default)]` 足够。

## 5. `shell_denylist` hook

### 5.1 文件

`crates/parrot-hooks/src/shell_denylist.rs`

### 5.2 匹配语义

沿用旧 `ShellExecTool::is_denied` 的 lowercase substring contains：

```rust
fn is_denied(&self, command: &str) -> bool {
    let command_lower = command.to_lowercase();
    self.cfg.denylist.iter().any(|p| command_lower.contains(&p.to_lowercase()))
}
```

命中后构造 reason：`format!("denied by sandbox policy: {}", command)`。

### 5.3 hook trait 实现

`HookPoints::TOOL_CALL`，bail 策略。仅对 `shell_exec` / `bash` / `shell` 的
`arguments.command` 生效（与 `dangerous_command_blocker` 一致的工具白名单）：

```rust
#[async_trait]
impl Hook for ShellDenylist {
    fn id(&self) -> &'static str { "shell_denylist" }
    fn supported(&self) -> HookPoints { HookPoints::TOOL_CALL }
    async fn handle(&self, ev: HookEvent<'_>, _ctx: &HookCtx<'_>)
        -> Result<HookAction, AgentError>
    {
        if let HookEvent::ToolCall { tool_name, arguments, .. } = ev {
            if matches!(tool_name, "shell_exec" | "bash" | "shell") {
                if let Some(cmd) = arguments.get("command").and_then(|v| v.as_str()) {
                    if self.is_denied(cmd) {
                        return Ok(HookAction::Block {
                            reason: format!("denied by sandbox policy: {}", cmd),
                        });
                    }
                }
            }
        }
        Ok(HookAction::NoOp)
    }
}
```

非 shell 工具 / 无 command 字段一律 `NoOp`，不阻挡。

### 5.4 与 `dangerous_command_blocker` 的关系

两个 hook 都挂 `tool_call` bail 策略，bail 在 `HookRegistry::run` 内
「首个 Block 立即 short-circuit」。执行顺序由 `[hooks].enabled` 顺序决定——
哪个排前哪个先跑。哪个 hook 先命中就先 bail，另一个不执行。文档建议
`dangerous_command_blocker`（regex）排在 `shell_denylist`（substring）前，
让更精确的 regex 先判，substring 兜底。

## 6. `ShellExecTool` 简化

`crates/parrot-tools/src/shell_exec.rs` 需要的改动：

```diff
 pub struct ShellExecTool {
-    denylist: Vec<String>,
 }

 impl ShellExecTool {
-    pub fn new(_working_dir: std::path::PathBuf, denylist: Vec<String>) -> Self {
-        Self { denylist }
+    pub fn new(_working_dir: std::path::PathBuf) -> Self {
+        Self
     }
-
-    fn is_denied(&self, command: &str) -> bool { ... }
 }
```

`call()` 内 `if self.is_denied(command)` 那一段（命中返回 `ToolOutput{is_error:true, ...}`）
整段删除。工具今后是「纯执行」。`parrot_tools::register_all` 传给 `ShellExecTool::new`
的 `config.tools.sandbox.denylist.clone()` 改为不传。

## 7. 配置迁移与兼容

### 7.1 `SandboxConfig.denylist` 移除

```diff
 #[derive(Debug, Clone, Serialize, Deserialize)]
 pub struct SandboxConfig {
     pub working_dir: String,
     pub allowlist: Vec<String>,
-    pub denylist: Vec<String>,
     pub require_confirmation: Vec<String>,
 }
```

`deny_unknown_fields` 默认不开（与现 `SandboxConfig` 一致），因此旧 `parrot.toml` 写了
`denylist = [...]` 行的会被 silently 忽略——保护丢失直到使用者迁到 `[hooks.shell_denylist]`。
这是已确认的取舍（用户决策 #1：完全移除旧字段）。

### 7.2 默认 config 迁移

`AppConfig::default_config()` 用 `hooks: HooksConfig::default()` 即可——
`HooksConfig::default()` 的 `configs` 是空 HashMap，`build_registry` 在
`configs.get("shell_denylist")` 缺失时回退到 `ShellDenylistConfig::default()`，自动套用
3 条 baseline pattern。`tools.sandbox` 默认 `denylist` 字段一起删：

```diff
 hooks: HooksConfig::default(),
 // tools.sandbox:
 sandbox: SandboxConfig {
     working_dir: ".".into(),
     allowlist: vec![],
-    denylist: vec!["rm -rf /".into(), "sudo".into(), "chmod 777".into()],
     require_confirmation: vec!["git push".into(), "rm".into()],
 },
```

### 7.3 文档同步

`AGENTS.md` 加一条 note（或 CHANGELOG 起一条）：`tools.sandbox.denylist` 已迁至
`[hooks.shell_denylist].denylist`，旧位置不再生效。`parrot.toml` 模板与
`docs/quickstart.md` 同步示例。

## 8. 默认 enabled 安全策略

`HooksConfig::default()` 的 `enabled`：

```rust
impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: vec!["shell_denylist".into(), "dangerous_command_blocker".into()],
            timeout_seconds: default_hook_timeout(),
            configs: HashMap::new(),   // 缺 subtable → 各 hook 默认 config 生效
        }
    }
}
```

含义分层：

| 场景 | 行为 |
|---|---|
| 新装、用 `default_config()` | `enabled` 含两 hook + `shell_denylist` 默认 3 条 denylist |
| 旧 `parrot.toml` 无 `[hooks]` 段 | `#[serde(default)]` 让 `hooks` 用 `HooksConfig::default()`，得到双 hook 默认安全 |
| 旧 `parrot.toml` 有 `[hooks] enabled = []` | 显式 opt-out，行为仍为空（保留用户的明确选择） |
| 旧 `parrot.toml` 有 `[hooks] enabled = ["dangerous_command_blocker"]` | 只要没写 `[hooks.shell_denylist]`，shell_denylist 不在 enabled，不开启；用户升级时建议补 `shell_denylist` 进 enabled |
| 旧 `parrot.toml` 有 `[tools.sandbox].denylist` | silently 忽略；用户需迁移到 `[hooks.shell_denylist].denylist` |

## 9. 测试

### 9.1 `parrot-hooks` 单元测试

每个 hook 一个 `#[cfg(test)] mod tests`：

- `shell_denylist`：命中 `rm -rf /` → `Block`；安全 `ls -la` → `NoOp`；非 shell 工具
  → `NoOp`；空 `ShellDenylistConfig` Default 包含 3 条 baseline；自定义 denylist 覆盖 default。
- `dangerous_command_blocker`：套用现有测试，确保搬家没破。
- `redact_secrets`：现有测试搬来。
- `build_registry`：`enabled` 含未知 id → warn + skip；`enabled` 空 → empty registry；
  `enabled` 含 `shell_denylist` 但 `configs` 缺该子表 → 用 `ShellDenylistConfig::default()`
  （3 条 baseline 仍然生效）。

### 9.2 `parrot-config` 测试

- `hooks_parse_per_hook_config`：TOML
  ```toml
  [hooks]
  enabled = ["shell_denylist"]
  timeout_seconds = 3
  [hooks.shell_denylist]
  denylist = ["foo"]
  ```
  反序列化 → `enabled == ["shell_denylist"]`，`configs["shell_denylist"]` 是 table 含
  `denylist = ["foo"]`。
- `hooks_missing_subtable`：TOML 只有 `[hooks] enabled=[]`，`configs` 为空 HashMap（不 panic）。
- `hooks_default_contains_baseline_hooks`：`HooksConfig::default().enabled` 含
  `shell_denylist` 和 `dangerous_command_blocker`。

### 9.3 e2e

新增 `e2e_shell_denylist_blocks_rm_rf`（`tests/integration/e2e_test.rs`，沿用
`e2e_hook_blocks_tool_call` 既有 mock provider 框架）：

- 构造 `AppConfig`，`hooks.enabled = ["shell_denylist"]`，
  `[hooks.shell_denylist].denylist = ["rm -rf /"]`。
- Mock provider 返回 `shell_exec` tool_call，`arguments.command = "rm -rf /var/log/old"`。
- 客户端断言：
  - 收到 `ToolStart`
  - 收到 `HookFired { hook_id: "shell_denylist", event_kind: "tool_call", result_kind: "block" }`
  - 收到 `ToolEnd { is_error: true, content: "denied by sandbox policy: rm -rf /var/log/old" }`
  - mock tool call 计数为 0（shell_exec 实际未执行）

### 9.4 既有测试不变

`react_loop.rs` / `cassette_test.rs` / 既有 `e2e_hook_blocks_tool_call`（走
`dangerous_command_blocker`）不受影响——`build_registry` 签名与 `Hook` trait 不变。

## 10. 不在范围

- `redact_secrets` 扩充（加 JWT / .env KEY=VALUE pattern / `extra_patterns` 配置字段）→ 见 #2 spec。
- 新 hook point `context_ready` + `ReplaceContext` 动作 → 见 #3 spec。
- `ContextManager::prune` 策略的外挂化（compaction）→ 见 #3 spec（`context_ready` 解锁）。
- `tools.sandbox.allowlist` / `require_confirmation` 的 hook 迁移——本轮只迁 `denylist`；
  `require_confirmation` 是 engine 内建 confirm 流程的门控配置，不在本轮。
- 第三方 hook 注册机制（plugin crate）：本轮 `build_registry` 仍 switch on 固定 id；后续按需。

## 11. 实施步骤（高层 — 详细步骤进 plan 文档）

1. 新建 `crates/parrot-hooks/`：Cargo.toml + lib.rs（先放 `build_registry` 空骨架 + 三 mod 声明）。
2. 把 `dangerous_command_blocker.rs` / `redact_secrets.rs` 从 daemon 搬到 parrot-hooks；
   `redact_secrets` 改成接 `RedactSecretsConfig`（本轮空 Default）。
3. 改 `parrot-config::HooksConfig` 加 `configs: HashMap<String, toml::Value>`
   + `#[serde(flatten)]`；先加 deserialize 单元测试验证 toml 路径；失败则手写 Deserialize。
4. 写 `shell_denylist.rs`：`ShellDenylistConfig` + `ShellDenylist` impl + 单元测试。
5. 改 `ShellExecTool`：删 `denylist` 字段 / `is_denied` / 构造参数。
6. 改 `parrot-config`：删 `SandboxConfig.denylist`；迁 default config 的 baseline 3 条到
   `HooksConfig` default 含 `shell_denylist` + `dangerous_command_blocker`。
7. 改 `parrot-daemon` runtime：`use parrot_hooks::build_registry`；删 daemon/src/hooks。
8. 改 workspace Cargo.toml：member + workspace.dependencies 加 `parrot-hooks`；
   daemon Cargo.toml 加 `parrot-hooks`，移除直接 `regex` 依赖。
9. 改 `parrot.toml` 模板和 `docs/quickstart.md` 示例。
10. e2e `e2e_shell_denylist_blocks_rm_rf`。
11. `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` 全绿。
12. AGENTS.md 加一条迁移 note。

## 12. 风险与权衡

- **`#[serde(flatten)]` + `toml::Value` 边界**：toml-rs 老版本对 flatten+map 有边界 case。
  本设计承认此风险，要求 plan 第一步加 deserialize 测试验证；失败回退手写 Deserialize。
  回退方案不影响接口契约，只是实现路径不同。
- **默认 enabled 改变现有用户行为**：原默认 `enabled=空`，新默认含双 hook。对没写 `[hooks]`
  段的旧 `parrot.toml`，新装默认会多开 dangerous_command_blocker。这是有意的「默认安全」
  改动，但属行为变化——要在 CHANGELOG 标注。
- **`dangerous_command_blocker` 误判风险**：它是 regex-based，可能挡掉
  `rm -rf /var/log/old` 这类使用者真实需要的清理命令。本轮把它纳入默认 enabled 是
  权衡决定：危险命令防护 > 偶发误判。使用者可在 `parrot.toml` 把它从 `enabled` 拿掉。
- **多 hook bail 顺序**：`shell_denylist` 与 `dangerous_command_blocker` 都挂 `tool_call` bail。
  谁先命中谁 short-circuit。`enabled` 顺序就是执行顺序。文档建议 dangerous 排前、denylist
  兜底，但本轮不强制。