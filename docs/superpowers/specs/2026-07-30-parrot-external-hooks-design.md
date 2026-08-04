# Parrot External Hooks — Design

**Date:** 2026-07-30
**Status:** Draft / pending user review
**Branch:** `feat/external-hooks` (to be created from `feat/lifecycle-hooks`)

## 1. Goal

讓 daemon 在不重新編譯的前提下，於運行期載入用戶自定義 hook。
外部 hook 是與 daemon 同機運行的子進程，調用時被 fork，事件 payload
走 stdin（一行 JSON），hook 的決策走 stdout（一行 JSON），進程退出。

頭腦風暴會話所定的約束:

- **語言**：任意（外部進程，非嵌入腳本 runtime）。
- **進程生命週期**：每次事件 fork 新進程，超時 kill，不長駐。
- **信任等級**：完全信任（同機、用戶本人配置）。無 env 隔離、無沙箱。
- **註冊路徑**：`parrot.toml` `[[hooks.external]]` 顯式條列，不做目錄掃描。
- **返回值語意**：只支援現有 5 種 `HookAction` 變體，未知 action 不生效。
- **失敗策略**：fail-open + 遙測。timeout / spawn 失敗 / 非 JSON / 非零
  退出 / 未知 action → 一律等價於 `NoOp`，但 `HookExecution` 與 daemon
  log 記錄詳細原因。
- **Payload 粒度**：傳整個序列化 `HookEvent` + config + 調用元數據。
- **MVP 範圍**：只做外部 hook 機制一項能力。內置 hook、協議、引擎調用
  點零改動。
- **stderr 處理**：截斷後寫 `tracing::warn!`，不進入事件通道。
- **enabled 語意**：`enabled` 只控內置 hook；`[[hooks.external]]` 列
  即生效，不與 `enabled` 交互（與 `[[providers]]` 一致的格式）。

## 2. 範圍

### In scope

- `parrot.toml` 加 `[[hooks.external]]` 陣列子表（`crates/parrot-config`）。
- `crates/parrot-hooks/src/external.rs` 新檔：`ExternalHook` struct + impl `Hook`。
- `crates/parrot-hooks/src/lib.rs::build_registry()` 擴展分支，loop external
  configs 完成註冊。
- `crates/parrot-core/src/hooks.rs` trait `fn id(&self) -> &'static str`
  → `fn id(&self) -> &str`，放寬生命週期以容許 `ExternalHook::id: String`。
- `crates/parrot-core/src/error.rs` 新增 `AgentError::ExternalHook` 變體，
  讓 `ExternalHook::handle` 失敗走 `Err` 路徑，最終落入 `HookExecution`
  的 `result_kind = "error"` 分支（保留失敗遙測到前端）。變體命名刻意
  與 `HookExecution` 遙測 struct 區分，避免讀者混淆。
- 配置解析測試（`crates/parrot-config/tests/config_test.rs`）。
- 純函數 parse_action 測試 + 跨平台 mock bin integration 測試
  （`crates/parrot-hooks/tests/external_hook_test.rs` + `tests/fixtures/`）。

### Out of scope

- Hook 進程長駐模式（stdin 流）。
- Hook 進程沙箱（landlock / seccomp / WASM）。
- 未知 action 透傳payload 讓其他 hook 互通信 — 仍為已知動作集合。
- `ContextReady` 大 payload 壓縮策略（MVP 接受整條 context 走 stdin）。
- 守護進程級別 e2e（自信心< 4/5 條測試 + 已有 registry unit 測試覆蓋）。
- Per-hook 動態 reload 配置。
- daemon 線風 requests 的 hook 實例 — 每事件 fork 已支援並發安全。

## 3. 架構概覽

```
parrot.toml              HooksConfig              ExternalHook (impl Hook)
[[hooks.external]]  ───►  external: Vec<...>  ───► ┌──── handle() ────┐
  id = "my"               (in parrot-config)        │ spawn child      │
  command = [...]                                    │ write event JSON │
  events = ["..."]                                   │ read stdout JSON │
  timeout_seconds = N                                │ parse → HookAction
  [hooks.external.config]                           │ fail-open NoOp   │
  [hooks.external.config.foo]                       └──────────────────┘
      k = "v"

                            ┌── existing match id.as_str() ──► ShellDenylist
build_registry(&cfg)───┐    ├── existing match id.as_str() ──► RedactSecrets
                       │    │
                       └────┴── for cfg in &cfg.external { ──► ExternalHook::new(cfg)
                                  registry.register(Arc::new(...));
                              }
                            └──► Arc<HookRegistry>      (內外混合，統一聚合)
```

關鍵不變量:

- `Hook` trait、`HookAction`、`HookResult`、`HookEvent`、`HookPoints`、
  `HookExecution`、`HookRegistry::run()` 一律零改動。
- `parrot-core` 7 處 `HookRegistry::run()` 調用點零改動。
- `AgentEvent::HookFired` 協議零改動。
- 內置 hook 三個檔案（`shell_denylist.rs` / `redact_secrets.rs` /
  `dangerous_command_blocker.rs`）零改動（除 `fn id` 簽名清理）。

## 4. 組件接口

### 4.1 `ExternalHookConfig` （`crates/parrot-config/src/config.rs`）

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExternalHookConfig {
    pub id: String,
    pub command: Vec<String>,
    pub events: Vec<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub config: toml::Value,
}
```

對應 TOML:

```toml
[[hooks.external]]
id = "compliance-log"
command = ["python", "/home/me/hook.py"]
events = ["tool_call", "tool_result"]
timeout_seconds = 2            # 可選；預設使用 [hooks] timeout_seconds
[hooks.external.config]        # 可選；整塊透傳給 hook 進程
sink = "stderr"
```

`HooksConfig` 加新字段:

```rust
pub struct HooksConfig {
    #[serde(default)] pub enabled: Vec<String>,
    #[serde(default = "default_hook_timeout")] pub timeout_seconds: u64,
    #[serde(default, flatten)] pub configs: HashMap<String, toml::Value>,
    #[serde(default)] pub external: Vec<ExternalHookConfig>,   // 新增
}
```

`enabled` 只控內置 hook；`external` 陣列裡只要列了即生效，與 `[[providers]]`
語意一致。`configs` HashMap 仍只對內置 hook 生效（外部 hook 的 per-config
走 `ExternalHookConfig.config`，不混用 `configs` HashMap）。

### 4.2 `ExternalHook` （`crates/parrot-hooks/src/external.rs`）

```rust
pub struct ExternalHook {
    id: String,
    command: Vec<String>,
    points: HookPoints,
    timeout_override: Option<Duration>,
    config: serde_json::Value,   // 從 toml::Value 轉來，調用時連同 event 透傳
}

impl ExternalHook {
    /// 解析 events 字串到 HookPoints bitflag。未知字串直接 Err，
    /// build_registry 把 Err 轉 warn 並跳過該 hook。
    pub fn new(cfg: &ExternalHookConfig, global_timeout: Duration)
        -> Result<Self, String>;
}

#[async_trait]
impl Hook for ExternalHook {
    fn id(&self) -> &str { &self.id }
    fn supported(&self) -> HookPoints { self.points }
    async fn handle(&self, event: HookEvent<'_>, ctx: &HookCtx<'_>)
        -> Result<HookAction, AgentError>;
}
```

### 4.3 `Hook` trait 簽名調整（`crates/parrot-core/src/hooks.rs`）

現狀 `fn id(&self) -> &'static str` 要求編譯期常量字串。
`ExternalHook` 的 id 是運行期 `String`，不能返回 `&'static str`。

放寬為:

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &str;     // 從 &'static str → &str
    fn supported(&self) -> HookPoints;
    async fn handle(&self, ev: HookEvent<'_>, ctx: &HookCtx<'_>)
        -> Result<HookAction, AgentError>;
}
```

對 3 個內置 hook impl 的影響:把 `fn id(&self) -> &'static str`
改為 `fn id(&self) -> &str`，body 不變（`"shell_denylist"` 仍可用，因為
`&'static str` 自动降级到 `&str`）。**無 call site 改動**：所有現有
`h.id().to_string()` / `h.id()` 比較仍 work。

### 4.4 `AgentError` 新變體（`crates/parrot-core/src/error.rs`）

```rust
pub enum AgentError {
    // ... existing variants ...
    ExternalHook { hook_id: String, detail: String },
}
```

為何不讓 `ExternalHook::handle` 失敗直接返回 `Ok(NoOp)`：那會讓 `HookExecution`
記 `result_kind = "noop"`，與主動 NoOp 無法區分，前端 (CLI/TUI) 看不出失敗。

走 `Err(AgentError::ExternalHook{hook_id, detail})`，`HookRegistry::call_with_timeout`
把 `Err` 轉 `HookFailure::Error(detail)`，`HookExecution::from_outcome` 已有
`"error"` 分支記 `summary = Some(detail)`。**前端現有對 `"error"` result_kind
的處理邏輯直接生效**。變體名用 `ExternalHook` 而非 `HookExecution`，避免
與 `HookExecution` 結構體撞名。

### 4.5 `build_registry()` 擴展（`crates/parrot-hooks/src/lib.rs`）

```rust
pub fn build_registry(cfg: &HooksConfig) -> Arc<HookRegistry> {
    let global_timeout = Duration::from_secs(cfg.timeout_seconds.max(1));
    let mut registry = HookRegistry::new(global_timeout);

    // 既有: 內置 hook 按 enabled 列表
    for id in &cfg.enabled {
        match id.as_str() {
            "shell_denylist" => { /* ... 既有 ... */ }
            "redact_secrets" => { /* ... 既有 ... */ }
            "dangerous_command_blocker" => { /* ... 既有 ... */ }
            other => tracing::warn!(hook_id = other, "unknown built-in hook id"),
        }
    }

    // 新增: 外部 hook 不依賴 enabled，列即生效
    for ext_cfg in &cfg.external {
        match ExternalHook::new(ext_cfg, global_timeout) {
            Ok(h) => {
                tracing::info!(
                    hook_id = %h.id, points = ?h.supported(),
                    "registered external hook"
                );
                registry.register(Arc::new(h));
            }
            Err(detail) => tracing::warn!(
                hook_id = %ext_cfg.id, error = %detail,
                "failed to build external hook; skipping"
            ),
        }
    }

    Arc::new(registry)
}
```

Events 字串到 `HookPoints` 的解析（在 `ExternalHook::new` 內）:

```rust
fn parse_points(events: &[String]) -> Result<HookPoints, String> {
    let mut p = HookPoints::empty();
    for ev in events {
        let bit = match ev.as_str() {
            "agent_start" => HookPoints::AGENT_START,
            "agent_end" => HookPoints::AGENT_END,
            "turn_start" => HookPoints::TURN_START,
            "tool_call" => HookPoints::TOOL_CALL,
            "tool_execution_start" => HookPoints::TOOL_EXECUTION_START,
            "tool_result" => HookPoints::TOOL_RESULT,
            "context_ready" => HookPoints::CONTEXT_READY,
            other => return Err(format!("unknown event kind: {other}")),
        };
        p |= bit;
    }
    Ok(p)
}
```

`parrot-config` 不依賴 `parrot-core`，所以這個解析在 `parrot-hooks` 層。

## 5. 事件 payload 與返回值契約

### 5.1 Payload（daemon → hook 進程 stdin）

單行 JSON，schema:

```json
{
  "event": {                       // 整個 HookEvent enum (#[serde(tag="type")])
    "type": "tool_call",
    "session_id": "uuid",
    "turn_id": "uuid",
    "parent_message_id": "uuid",
    "tool_call_id": "tc_1",
    "tool_name": "shell_exec",
    "arguments": { "command": "rm -rf /" }
  },
  "config": { "sink": "stderr" },  // [hooks.external.config.*] 整塊 toml→json
  "hook_id": "compliance-log",
  "working_dir": "/home/me/project",
  "timeout_ms": 3000
}
```

- `event` 來自 `serde_json::to_value(&event)`（`HookEvent` 已 `#[serde(tag = "type")]`）。
- `config`：`toml::Value` 經 `toml::to_string` 再 `serde_json::from_str` 轉 JSON Value。
  注意 `toml::Value::Datetime` 會丟类型但變 ISO 字串；MVP 接受。
- `timeout_ms` = `resolve_timeout(ctx).as_millis()`，讓 hook 知道 budget。
- payload 末尾換行 `\n`，hook 按行讀。
- `ContextReady` 事件攜帶整條 context（可能 KB-MB）。MVP 不優化。

### 5.2 返回值（hook 進程 stdout → daemon）

hook 進程在 exit code 0 之前，stdout **最後一行** 必須是合法 JSON。
其他行可為 debug print。schema 對齊 `HookAction` `#[serde(tag="kind", rename_all="snake_case")]`:

實際外部 hook 回的是 `{"action": "...", ...fields...}` 形式。Daemon 內用一個
輕量 adapter struct 接收，再轉 `HookAction`：

```rust
#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WireAction {
    Noop,
    Block { reason: String },
    InjectMessages { messages: Vec<ChatMessage> },
    ReplaceResult { content: String, is_error: bool },
    ReplaceContext { messages: Vec<ChatMessage> },
}

impl WireAction {
    fn into_hook_action(self) -> HookAction {
        match self {
            WireAction::Noop => HookAction::NoOp,
            WireAction::Block { reason } => HookAction::Block { reason },
            WireAction::InjectMessages { messages } => HookAction::InjectMessages { messages },
            WireAction::ReplaceResult { content, is_error } => HookAction::ReplaceResult { content, is_error },
            WireAction::ReplaceContext { messages } => HookAction::ReplaceContext { messages },
        }
    }
}
```

`WireAction` 純為外部 wire 協議用；因為 `HookAction` 自己 serde tag 是 `kind`
（命名 `HookAction` 內部 IR），外置 wire 用 `action` 更符合 hook 作者直覺。
兩者結構相同，只是 tag 名不同，避免 hook 作者誤寫 `{"kind":...}` 才出問題。

範例（Python hook）:

```python
import json, sys
input_line = sys.stdin.readline()
event = json.loads(input_line)
if event["event"]["type"] == "tool_call":
    sys.stdout.write(json.dumps({"action": "block", "reason": "blocked by my hook"}))
```

### 5.3 解析容忍度

| 情況 | 行為 |
|---|---|
| 末行合法 JSON，action 為已知 5 種，field 完整 | `Ok(action)` |
| 末行合法 JSON，action 字串未知 | `Err(AgentError::HookExecution{detail = "unknown action: ..."})`; fail-open to NoOp |
| 末行合法 JSON，必填 field 缺/型错 | `Err(AgentError::HookExecution{detail = "missing/invalid field: ..."})`; fail-open to NoOp |
| 末行非法 JSON | `Err(AgentError::HookExecution{detail = "parse error: ..."})`; fail-open to NoOp |
| stdout 完全空 | `Ok(NoOp)`，記 `result_kind = "noop"`，無 detail；視為 silent allow |
| 多行 stdout | 取最後非空行解析 |

## 6. 失敗處理

### 6.1 失敗分類矩陣

| 情況 | `HookExecution` result_kind | summary | tracing level | stderr 截斷輸出 |
|---|---|---|---|---|
| Timeout 超時 | `"timeout"` | None | `warn` | 有則頭 4KB |
| spawn 失敗（command 不存在） | `"error"` | `"spawn: {err}"` | `warn` | n/a |
| exit code ≠ 0 | `"error"` | `"exit={code}"` | `warn` | 有則頭 4KB |
| stdout 非 JSON | `"error"` | `"parse: {err}"` | `warn` | 有則頭 4KB |
| JSON 解析成功但 action 字串未知 | `"error"` | `"unknown action: {raw}"` | `warn` | 有則頭 4KB |
| JSON 解析成功但 field 缺/型错 | `"error"` | `"invalid field: {name}"` | `warn` | 有則頭 4KB |
| stdout 空行 | `"noop"` | None | `debug`（不是 warn） | 有则头 4KB |
| 合法解析返回 | 由 `from_outcome` 沿用 ("block"/"inject_messages"/...) | 由 from_outcome 沿用 | n/a | 有則頭 4KB |

注：除「stdout 空行 = silent allow」走 `Ok(NoOp)` 觸發 debug log 外，
所有失敗都走 `Err(AgentError::HookExecution)` 並通過 `HookRegistry::call_with_timeout`
轉 `HookFailure::Error(detail)`，最終記錄為 `result_kind = "error"`。

### 6.2 stderr 截斷輸出策略

```rust
fn log_stderr(&self, stderr: Vec<u8>) {
    let stderr = String::from_utf8_lossy(&stderr);
    if stderr.is_empty() { return; }
    let trimmed = if stderr.len() > 4096 {
        format!("{}...(truncated {} bytes total)", &stderr[..4096], stderr.len())
    } else {
        stderr.to_string()
    };
    tracing::warn!(
        target: "parrotd::ext_hook",
        hook_id = %self.id,
        stderr = %trimmed,
        "external hook produced stderr"
    );
}
```

- stdout 永不進 tracing（已被 JSON 解析消費）。
- stderr 不論成功失敗，非空都走 warn。Hook 作者用 stderr 是常見 debug 路徑。
- 截斷阈 4KB（Claude Code 經驗值）。

### 6.3 Timeout 行為細節

- 用 `tokio::time::timeout(self.resolve_timeout(ctx), child.wait_with_output())`。
- stdin 寫入放 `tokio::spawn` 的 writer task，避免阻塞。child stdin pipe 在 spawn 後由
  writer task 持有；writer 寫完後 drop 即關閉 stdin。
- 超時則 `child.kill().await` + 顯式 `child.wait()` 收屍，避免 zombie
  （Unix 上 tokio 命令已 kill-on-drop 但顯式 kill 更穩）。
- timeout 數值：`self.timeout_override.unwrap_or(ctx.timeout)`。

## 7. 測試策略

### 7.1 Config 解析單元測試（`crates/parrot-config/tests/config_test.rs`）

| TOML 輸入 | 預期 |
|---|---|
| 完整 `[[hooks.external]]` 塊 (id/command/events/timeout_seconds/config) | `ExternalHookConfig` 解析正確 |
| 缺 id | toml de error |
| 缺 command | toml de error |
| 缺 events | toml de error |
| `enabled` 與 `external` 並存 | 兩者各 register 各自，不衝突 |
| 多個 `[[hooks.external]]` 子表 | `external` Vec 長度等于子表數 |

### 7.2 純函數 unit 測試（`crates/parrot-hooks/src/external.rs` `#[cfg(test)]`）

**`parse_points(&[String]) -> Result<HookPoints, String>`**:

| 輸入 | 預期 |
|---|---|
| `["tool_call","tool_result"]` | `Ok(TOOL_CALL \| TOOL_RESULT)` |
| `["agent_start","agent_end","turn_start","tool_call","tool_execution_start","tool_result","context_ready"]` | `Ok(全 bits)` |
| `["foo_bar"]` | `Err("unknown event kind: foo_bar")` |
| `[]` | `Ok(empty)`（Hooks 不會被觸發） |

**`parse_action(last_line: &str) -> Result<HookAction, String>`**：

| stdout 末行 | 預期 HookAction |
|---|---|
| `{"action":"block","reason":"x"}` | `Block{reason="x"}` |
| `{"action":"inject_messages","messages":[{"role":"system","content":"hi","tool_call_id":null,"tool_name":null,"tool_calls":null}]}` | `InjectMessages{1 msg}` |
| `{"action":"replace_result","content":"y","is_error":true}` | `ReplaceResult{y,true}` |
| `{"action":"replace_context","messages":[...]}` | `ReplaceContext` |
| `{"action":"noop"}` | `NoOp` |
| `(空字串)` | `NoOp`（silent allow） |
| `{"action":"record_kv"}` | `Err("unknown action: record_kv")` |
| `{"action":"block"}` | `Err("missing field: reason")` |
| `"not json"` | `Err("parse error ...")` |

### 7.3 Build registry 整合測試（`crates/parrot-hooks/tests/`）

覆蓋 `build_registry` 對 external config 的錯誤兜底:

- 未知 events 字串 → warn + skip。
- 同 id 內外撞名 → 兩者都 register（不互擠），可 observability 看 HookFired hook_id 區分。

### 7.4 跨平台 mock bin integration 測試

`crates/parrot-hooks/tests/fixtures/` 下放 cross-platform Rust 小 bin：

- `mock_hook_block`:讀 stdin line，輸出 `{"action":"block","reason":"test"}` exit 0
- `mock_hook_noop`:讀 stdin line，輸出 `{"action":"noop"}` exit 0
- `mock_hook_silent`:讀 stdin line，stdout 空 exit 0
- `mock_hook_exit1`:讀 stdin line，stdout 寫 `{"action":"noop"}` 但 exit 1
- `mock_hook_sleep`:讀 stdin line，sleep 10s（給 100ms timeout 觸發）
- `mock_hook_unknown`:輸出 `{"action":"record_kv"}`

Cargo.toml 用 `[[bin]]` 宣告 + `[[test]]` 標 `path = "tests/..."`。測試用
`env!("CARGO_BIN_EXE_mock_hook_block")` 取 bin 路徑（cargo 標準模式，跨平台乾淨）。

### 7.5 Registry run() 整合測試

混合 RegistrationHook + ExternalHook mock bin block，驗 waterfall 聚合
正確：內置 NoOp 在前，外置 Block 在後，最終 `HookResult::Block{hook_id=external}`。

### 7.6 守護進程 e2e

YAGNI — 上面 Registry 集成測試已足以保證守護進程正確性（Registry 對所有
impl `Hook` 平等）。

## 8. 配置兼容性與遷移

- 現狀配置（無 `[[hooks.external]]`）0 改也跑 100% 向後兼容：build_registry
  跳過空 `external` Vec。
- 新 `[[hooks.external]]` 與 `enabled` 完全正交：`enabled` 只控內置 hook，
  `external` 列即生效。
- `configs: HashMap<String, toml::Value>` 仍只對內置 hook 生效。外部 hook 的
  per-config 走 `ExternalHookConfig.config: toml::Value`，避免 ID 命名空間衝突。

## 9. 依賴變更

### `crates/parrot-hooks/Cargo.toml`

新增:

- `tokio = { version = "1", features = ["process", "time", "io-util"] }`
- `serde_json = "1"`

### 其他 crate

零新增依賴。

## 10. 風險與緩解

| 風險 | 緩解 |
|---|---|
| 進程 fork 每次冷啟動 1-10ms 延遲（Node/Python） | MVP 接受；未來有 long-running 變體再 spec |
| `ContextReady` 大 payload 經 stdin 流 | MVP 接受；後續可加 `compact_context: bool` 配置 |
| 惡意配置 token 文件被 hook 進程讀 | 用戶選擇「完全信任」風險；與「用戶能直接 cat token」等價 |
| 未知 action 退化為 NoOp 讓用戶誤以為生效 | 提供清晰文檔 + warn tracing；前端 result_kind 區分 |
| 每事件 fork 在 message 流高頻場景退化 | MVP hook 站點設計已是 session/turn/tool level，非 message delta |
| Windows 上 tokio::process 兼容性 | 已驗證 parrot-tools 使用 `tokio::process::Command` 正常 |
| Hook 作者 verbosely stderr 淹沒 daemon log | 4KB 截斷 + `tracing` EnvFilter 可進一步降級 |

## 11. 文檔

- 更新 `docs/superpowers/specs/2026-07-25-parrot-lifecycle-hooks-design.md` §3
  與 §6:說明 `HookRegistry::run` 仍單一 entry point，但 `Hook` impl 現包含
  「ExternalHook」一種新分類。
- 新增本 spec 被列為 `[[hooks.external]]` 用戶手冊的引用源 (docs/usage/hooks.md
  如未來建立)。

## 12. 鑑收條件

- [ ] `cargo build --workspace` 過
- [ ] `cargo test --workspace` 145 → N（加新測試）全綠
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 過
- [ ] `cargo fmt --all -- --check` 過
- [ ] 新增 mock bin 5 個通過
- [ ] 配置新增 `[[hooks.external]]` 但未啟用 enabled，能 backward 包含現有配置
- [ ] 手動：寫一個 Python one-line hook sample，配進 parrot.toml，跑 daemon
      + 一個 tool_call，確認 HookFired {result_kind: "block"} 出現在前端 CLI