# Parrot `redact_secrets` Hook 扩充

> 扩充 `redact_secrets` hook：新增内置 JWT 与 .env `KEY=value` 脱敏 pattern，
> 并允许使用者通过 per-hook config 添加自定义 regex。
>
> 本 spec 建立在 #1 spec（`2026-07-25-parrot-hooks-crate-and-shell-denylist-design.md`）之上：
> 假定 `redact_secrets` 已随地基重构搬到 `crates/parrot-hooks/src/redact_secrets.rs`，
> 并已改为带 config 的结构体 `RedactSecrets { cfg: RedactSecretsConfig }` +
> `RedactSecrets::new(cfg)`。本 spec 只在此基础上扩字段、加 pattern、加路径。

## 1. 背景与目标

当前 `redact_secrets` 内置 4 条 pattern：AWS 访问 key（`AKIA...`）、GitHub PAT（`ghp_...`）、
Anthropic key（`sk-ant-...`）、PEM 私钥头。漏两类常见泄漏：

- **JWT**——`eyJ...` 三段 base64url token，常见于 auth debug 打印、README 示例、cookie dump。
- **.env 的 `KEY=VALUE`**——`file_read` / `shell_exec` 输出里的环境变量赋值，
  例如 `ANTHROPIC_API_KEY=sk-ant-xxx`、`DATABASE_URL=postgres://...`，常见泄漏源。

同时无配置项可让使用者针对自家 secret 格式补自定义 regex（如内部服务 token 前缀、
特定 schema 的连接串）。

本轮目标：

1. 内置加 JWT + .env `KEY=value` 脱敏 pattern（保守语义，见 §4）。
2. `RedactSecretsConfig` 加 `extra_patterns: Vec<String>` 字段，使用者可写 regex。
3. 无效 regex → warn log + skip，不构成硬错误（保护 daemon 可启动、`build_registry` 不 panic）。

`redact_secrets` 仍是 opt-in（不在默认 `[hooks].enabled`）。

## 2. 设计原则

1. **保守胜过激进**：脱敏 false negative（漏一个 secret）比 false positive（误脱敏一段文档）
   影响小。本轮新 pattern 都尽量收窄命中条件，宁可漏 100 也别误伤 1。
2. **内置无法覆盖一切**：核心场景用内置解决；个性化/内部 key 前缀走 `extra_patterns`，
   不在 `redact_secrets` 里堆 case-by-case pattern。
3. **per-hook config 一致性**：`extra_patterns` 是 `RedactSecretsConfig` 字段，通过
   `[hooks.redact_secrets].extra_patterns` 配置，由 `parrot-config::HooksConfig.configs` HashMap
   在 #1 spec 建立的机制传递。不引入新的 config crate 字段。
4. **错误隔离**：使用者写错 regex（语法非法）不会导致 daemon 启动失败——warn 即跳过该
   pattern，其余 pattern 继续生效。

## 3. `RedactSecretsConfig` 字段扩

```rust
// crates/parrot-hooks/src/redact_secrets.rs
#[derive(Debug, Clone, Deserialize)]
pub struct RedactSecretsConfig {
    /// 使用者自定义 regex 列表。匹配到的内容统一替换为 `[REDACTED]`。
    /// 无效 regex 会被 warn + skip，不影响其它 pattern 生效。
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

impl Default for RedactSecretsConfig {
    fn default() -> Self {
        Self { extra_patterns: Vec::new() }
    }
}
```

`RedactSecrets::new(cfg)` 把 cfg 存到结构体内，`handle()` 读取。

TOML 示例：

```toml
[hooks]
enabled = ["shell_denylist", "dangerous_command_blocker", "redact_secrets"]

[hooks.redact_secrets]
extra_patterns = [
    "MY-CUSTOM-SERVICE-TOKEN-[a-z0-9]{32}",
    "internal-service-key-\\d+",
]
```

## 4. 新增内置 pattern

### 4.1 通用 pattern 数据结构

把原本 `Vec<Regex>` 升级为 `Vec<SecretPattern>` —— 每个 pattern 自带 replacement template，
使 .env pattern 能保留 KEY 名：

```rust
struct SecretPattern {
    re: Regex,
    /// `re.replace_all` 的 replacement template。
    /// 内置普通 secret 用 `"[REDACTED]"`；.env pattern 用 `"${key}=[REDACTED]"` 保留 key。
    repl: &'static str,
}
```

注意：`regex` crate 的 `replace_all` 接受 `${name}` 模板语法，因此 template 字符串内可引用
named capture group。本次升级对现有 4 个内置 pattern 行为不变（repl 仍是 `"[REDACTED]"`）。

### 4.2 JWT pattern

- Regex：`eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+`
- repl：`"[REDACTED]"`
- 理由：JWT 形如 `<header>.<payload>.<signature>`，header 与 payload 都是 base64url 编码的
  JSON，解码后必定以 `{"..."}` 开头，base64url encoded 必以 `eyJ` 起首。三段 + `.` 分隔
  + base64url 字符集构成足够特异的指纹，误判率极低。
- 不在最短长度门槛（`+` 即 1+，但 JWT 通常每段长度 ≥ 4 字符）；保守起见不限制最小长度，
  避免遗漏刻意短 header 的测试 token。

### 4.3 `.env` KEY=value pattern

**保守语义——只脱敏敏感 key 的 value，保留 key 名与结构。**

- Regex（含 named group）：
  ```
  (?m)^(?P<key>[A-Z_][A-Z0-9_]{2,}(?:_KEY|_TOKEN|_SECRET|_PASSWORD|_CREDENTIALS?))\s*=\s*(?P<val>[^\r\n#]+)
  ```
- repl：`"${key}=[REDACTED]"`
- 命中条件：行首、大写/下划线 key + 至少 3 字符 + key 后缀是
  `_KEY` / `_TOKEN` / `_SECRET` / `_PASSWORD` / `_CREDENTIAL` / `_CREDENTIALS`、等号、值
  到行尾（或 `#` 前）。
- 覆盖示例（命中并脱敏 value）：
  - `ANTHROPIC_API_KEY=sk-ant-xxx` → `ANTHROPIC_API_KEY=[REDACTED]`
  - `GITHUB_TOKEN=ghp_xxx` → `GITHUB_TOKEN=[REDACTED]`
  - `DB_PASSWORD="hunter2"` → `DB_PASSWORD=[REDACTED]`（连引号一起脱去，可接受）
- 不命中（保留原样）：
  - `PORT=8080`（key 无敏感后缀）
  - `DEBUG=true`（同上）
  - `NODE_ENV=production`（同上）
  - 行中间出现 `FOO_KEY=bar`（不在行首，`^` 不匹配）
- 不可绕过：多处命中如多行 `.env` 都会被脱敏（`(?m)^` 在每行生效）。

> **已知约束**：`regex` crate 不支持 `\K` / lookahead，pattern 用 `[^\r\n#]+` 在 `#`
> 前停下，因此**注释本身被保留**，但 value 与 `#` 之间的尾随空白会被 match 吞掉。例如
> `API_KEY=secret   # production` 会变成 `API_KEY=[REDACTED]# production`
> （`secret` 与 `#` 之间 3 个空格丢失，`# production` 保留）。这是可接受的保守取舍；
> 使用者想要更精细可改用 `extra_patterns` 写自定义 regex。

### 4.4 完整内置 pattern 列表

| # | 名称 | Pattern | Replacement |
|---|---|---|---|
| 1 | AWS 访问 key | `AKIA[0-9A-Z]{16}` | `[REDACTED]` |
| 2 | GitHub PAT | `ghp_[A-Za-z0-9]{36}` | `[REDACTED]` |
| 3 | Anthropic key | `sk-ant-[A-Za-z0-9_\-]{20,}` | `[REDACTED]` |
| 4 | PEM 私钥头 | `-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----` | `[REDACTED]` |
| 5 | JWT | `eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+` | `[REDACTED]` |
| 6 | .env 敏感值 | （见 §4.3） | `${key}=[REDACTED]` |

## 5. `extra_patterns` 用户自定义

`RedactSecrets::handle` 流程：

1. 先跑 6 条内置 pattern（按 §4 顺序）；
2. 再跑 `cfg.extra_patterns` 中每条 user regex：
   - 每次 **runtime 都 `Regex::new(&pattern_str)`**——不缓存，因为 `extra_patterns` 是
     per-hook config 中等大小（典型 < 10），且 hook 调用频率远低于 LLM call。
     若未来出现性能问题，可改 OnceLock 缓存（key = pattern_str，value = Result<Regex>）。
   - 若 `Regex::new` 报 `Err`：`tracing::warn!("invalid regex in [hooks.redact_secrets].extra_patterns: {pattern} -> {err}; skipping")`，
     继续 next pattern。不影响其它 pattern 与 daemon 启动。
   - repl 统一 `"[REDACTED]"`（`extra_patterns` 不支持 `${name}` 模板，避免误用 named group
     制造模板错配；想保留 key 走内置 .env 即可，复杂场景自己写 hook）。
3. 任一 pattern 命中 (`new != content`) → `changed = true`，最后整体返回 `HookAction::ReplaceResult`。

```rust
// crates/parrot-hooks/src/redact_secrets.rs（节选）
fn sanitize(&self, content: &str) -> Option<String> {
    let mut buf = content.to_string();
    let mut changed = false;

    for SecretPattern { re, repl } in built_in_patterns() {
        let next = re.replace_all(&buf, *repl).to_string();
        if next != buf { changed = true; buf = next; }
    }

    for pattern_str in &self.cfg.extra_patterns {
        let re = match Regex::new(pattern_str) {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!(
                    pattern = pattern_str,
                    error = %e,
                    "invalid regex in [hooks.redact_secrets].extra_patterns; skipping"
                );
                continue;
            }
        };
        let next = re.replace_all(&buf, "[REDACTED]").to_string();
        if next != buf { changed = true; buf = next; }
    }

    if changed { Some(buf) } else { None }
}
```

## 6. `handle` 路径调整

`tool_name` 白名单与原版一致：`"read" | "shell_exec" | "bash" | "shell" | "file_read"`；
对 `result.content` 调用 `sanitize()`；命中则返回
`HookAction::ReplaceResult { content: sanitized, is_error: result.is_error }`（保持 is_error 不变），
不命中返回 `HookAction::NoOp`。行为与原版一致，只是 pattern 集变大 + 多一段 user pattern 路径。

> **多 hook waterfall 一致性**：若用户在 `[hooks].enabled` 里把多个脱敏相关 hook 排在一起，
> `tool_result` 点走 waterfall，下一个 hook 的 `HookEvent::ToolResult.result` 看到的是上个
> hook 已脱敏后的 content。这点与 #1 spec §5.4 「bail 顺序由 enabled 决定」对应——waterfall
> 顺序同理由 enabled 决定。

## 7. 测试

### 7.1 单元（在 `crates/parrot-hooks/src/redact_secrets.rs`）

- `redacts_jwt`——内容含 `eyJhbGci.eyJzdWI.xxxxx` → 被替换为 `[REDACTED]`，原 JWT 子串消失。
- `redacts_env_value_keeps_key`——`ANTHROPIC_API_KEY=sk-ant-xxx` → `ANTHROPIC_API_KEY=[REDACTED]`，
  断言保留 key、移除原 value。
- `env_does_not_redact_non_sensitive_keys`——`PORT=8080` / `DEBUG=true` → `NoOp`，
  对照位置完好。
- `env_only_matches_line_start`——`some text FOO_KEY=barbaz` → `NoOp`（行中不算 .env 行）
- `extra_pattern_redacts`——config 给 `["MY-CUSTOM-\\d+"]`、content `MY-CUSTOM-12345` → 被
  `[REDACTED]`。
- `extra_pattern_invalid_regex_warns_skips`——config 给 `["("]`（语法非法）→ 其他内置 pattern
  仍生效、daemon 测试不 panic（断言命中其它内置 pattern）。
- `redacts_aws_key` / `no_change_when_clean`——沿用原版两个测试，确保搬家 + 新增 pattern
  不破坏既有行为。

### 7.2 config 解析（在 `crates/parrot-config`）

- `hooks_parse_extra_patterns`：TOML
  ```toml
  [hooks]
  enabled = ["redact_secrets"]
  [hooks.redact_secrets]
  extra_patterns = ["CUSTOM-\\d+"]
  ```
  反序列化 → `configs["redact_secrets"]` 是 table 含 `extra_patterns = ["CUSTOM-\\d+"]`。
- 缺失 `[hooks.redact_secrets]` 子表 → `configs` 不含 key → `RedactSecretsConfig::default()`
  生效（extra_patterns=空），既有 hook 仍只跑内置 pattern。

### 7.3 e2e（可选，后置）

- `e2e_redact_secrets_redacts_jwt_in_read`：mock provider 触发 `file_read` 一个内容含 JWT 的
  假文件，断言 `HookFired{result_kind:"replace_result"}` + `ToolEnd.result.content` 不含 JWT
  含 `[REDACTED]`。可选，既有 e2e 模板就够了，本步可由 unit 替代不强制。计划阶段决定。

## 8. 不在范围

- **更多内置 secret 类型**（Google/GCP/Slack/Stripe/database URL/Private SSH host key 等）：
  本轮不加；使用者用 `extra_patterns` 兜底。
- **`.env` 行中间出现的 `FOO_KEY=bar`**：行中不算 .env 配置项，本设计不脱敏。若需要可改
  去掉 `^` 约束，但会大幅提高误判率。
- **基于 secret 命名约定的 allowlist 而非 suffix**（如内置与 `AWS_*` / `GITHUB_*`
  字面 allowlist）：本设计只走后缀匹配，不维护字面 key 名清单（后续若需更多 case 再加）。
- **`extra_patterns` 支持 `${name}` 模板 / 自定义 replacement**：只支持 `[REDACTED]` 替换，
  保持简单。使用者要更精细的 replacement 自己写 hook。
- **redact_secrets 进默认 enabled**：保持 opt-in（#1 spec §8 矩阵）。
- **JWT header/payload base64 解码后再扫描 secret**：纯 pattern 扫描不解码，避免引入 base64
  解码依赖与误判风险。

## 9. 实施步骤（高层）

1. 扩 `RedactSecretsConfig` 加 `extra_patterns: Vec<String>` 字段，加 `Default` impl。
2. 把 `redact_secrets.rs` 中的 `Vec<Regex>` 改为 `Vec<SecretPattern>`，加 JWT pattern 与
   .env pattern（含 named group template）。
3. 实现 `RedactSecrets::sanitize(content)` 方法（内置 + extra_patterns 两段），含 invalid regex
   warn 路径。
4. `RedactSecrets::handle` 改为调用 `sanitize`，命中返回 `ReplaceResult`。
5. 写 7 个单元测试（§7.1）。
6. 写 2 个 config 解析测试（§7.2）。
7. （可选）写 e2e（§7.3）。
8. `cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` 全绿。

## 10. 风险与权衡

- **.env value 与注释间空白丢失**：§4.3 约束——pattern 用 `[^\r\n#]+` 在 `#` 前停下，
  注释本身保留，但 value 与 `#` 之间的尾随空白被 match 吞掉。是 `regex` crate 不支持
  lookahead 的妥协；注释完整性保留 > 空白精度，保守脱敏优先。
- **`extra_patterns` invalid regex 不阻塞**：warn + skip 设计保证 daemon 启动不挂。但若使用者
  拼错配置且不影响启动，他可能不知道——warn log 是唯一信号。文档建议配置完跑一次
  `cargo test` 验证。
- **JWT `eyJ` 误命中**：base64url 序列以 `eyJ` 开头很普遍（任何 `{"...}` JSON 头），
  但要求 triple-segment + 点分隔 + base64url 字符集，组合够特异。极少见的巧合（如 README
  里写了 `eyJ...eyJ....`）会被误脱，可接受。
- **`extra_patterns` 不支持 named-group template**：使用者要保留 key 只能用内置 .env pattern；
  个性化需求需另写 hook。简化换更可预测行为。
- **不缓存 user regex**：runtime 每次 `Regex::new`。短 pattern 影响小；若使用者堆上百条，
  hook 会有可观 CPU 开销。已在 §5 标注后续优化路径（OnceLock 缓存）。