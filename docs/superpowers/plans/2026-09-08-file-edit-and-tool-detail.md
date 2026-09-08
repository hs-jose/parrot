# file_edit 工具 + tool call 详情回传 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `file_edit` 字符串精准替换工具（含 read-before-edit / 唯一性 / CRLF 三重护栏），并让 TUI 可展开查看工具调用完整参数+结果、CLI `-m` 打印参数与完整结果。

**Architecture:** file_edit 落在 `crates/parrot-tools`，复用现有 `Tool` trait 与 `before_call/after_call` 管线；read-before-edit 状态是 `ToolContext` 上新增的纯内存共享集合（engine 每会话一份），core 保持 zero-IO。详情回传不动协议（`ToolStart{arguments}`/`ToolEnd{result}` 已推送），只补 TUI 选中展开与 CLI 打印；参数紧凑化逻辑抽到 bin 内共用模块。

**Tech Stack:** Rust 2021 workspace、tokio、serde_json、ratatui（TUI）、parrot-core 的 `Tool`/`ToolRegistry`。

**Spec:** `docs/superpowers/specs/2026-09-08-parrot-file-edit-and-tool-detail-design.md`

## Global Constraints

- `parrot-core` 零 IO：不得引入 `reqwest` / `tokio::fs` / `std::env`（`files_read` 是纯内存数据，允许）。
- 跨 crate 错误用 `thiserror` 枚举；工具内部失败统一产出 `AgentError::ToolExecution` 或 `ToolOutput { is_error: true }`（沿用现有工具的写法）。
- 新增注释一律中文（仓库惯例，见 commit bebc959）。
- 本仓库 Windows 检出为 CRLF，工具必须换行感知。
- 每个任务结束必须通过：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、相关 crate 的 `cargo test`。
- 不新增 runtime 依赖；允许为测试新增 dev-dependency（本计划给 `crates/parrot-tools` 加 `tempfile`）。
- 协议层（`parrot-protocol`）零改动。

---

### Task 1: `ToolContext` 扩展（`files_read` 共享集合）

**Files:**
- Modify: `crates/parrot-core/src/tool.rs`（struct 定义约 19-22 行、tests 模块 165-250 行的 7 处构造）

**Interfaces:**
- Produces: `pub type SharedFilesRead = Arc<Mutex<HashSet<PathBuf>>>`；`ToolContext { working_dir, max_file_size_bytes, files_read: SharedFilesRead }`；`ToolContext::new(working_dir: PathBuf, max_file_size_bytes: u64) -> Self`；`ToolContext::with_files_read(self, files_read: SharedFilesRead) -> Self`。后续 Task 2/3/5 依赖这些名字。

- [ ] **Step 1: 写失败测试**

在 `crates/parrot-core/src/tool.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn files_read_set_is_shared_via_builder() {
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 1024);
        let shared = std::sync::Arc::clone(&ctx.files_read);

        // with_files_read 注入的是外部集合：往注入集合写，ctx 里可见。
        let injected: crate::tool::SharedFilesRead = Default::default();
        injected.lock().unwrap().insert(std::path::PathBuf::from("a.rs"));
        let ctx2 = ToolContext::new(std::path::PathBuf::from("."), 1024)
            .with_files_read(std::sync::Arc::clone(&injected));

        assert!(ctx2.files_read.lock().unwrap().contains(std::path::Path::new("a.rs")));
        assert!(shared.lock().unwrap().is_empty(), "默认集合独立于注入集合");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p parrot-core tool::tests::files_read_set_is_shared_via_builder`
Expected: 编译失败（`ToolContext::new` / `files_read` / `SharedFilesRead` 不存在）。

- [ ] **Step 3: 最小实现**

`tool.rs` 顶部，把 `use std::sync::Arc;` 改为：

```rust
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
```

在 `use` 块之后、`ToolDefinition` 之前加类型别名：

```rust
/// 会话级"已读文件"集合的共享句柄。file_read 写入、file_edit 检查。
/// 纯内存状态，core 保持 zero-IO。
pub type SharedFilesRead = Arc<Mutex<HashSet<PathBuf>>>;
```

把 `ToolContext` 定义（含 `ToolContext` 上方注释保留）改为：

```rust
pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub max_file_size_bytes: u64,
    /// 本会话已成功读取过的文件（`resolve_arg_path` 解析后的路径）。
    /// file_edit 的 read-before-edit 护栏数据源。
    pub files_read: SharedFilesRead,
}

impl ToolContext {
    pub fn new(working_dir: std::path::PathBuf, max_file_size_bytes: u64) -> Self {
        Self {
            working_dir,
            max_file_size_bytes,
            files_read: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 注入外部共享的已读集合（engine 每会话持有一份）。
    pub fn with_files_read(mut self, files_read: SharedFilesRead) -> Self {
        self.files_read = files_read;
        self
    }
}
```

同文件 tests 模块里 7 处字面量构造（`register_and_get_tool` 之外的每个用 `let ctx = ToolContext {` 开头的测试），用 replaceAll 方式统一替换：

旧：
```rust
        let ctx = ToolContext {
            working_dir: std::path::PathBuf::from("."),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
```
新：
```rust
        let ctx = ToolContext::new(std::path::PathBuf::from("."), 10 * 1024 * 1024);
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p parrot-core`
Expected: 全部 PASS（含新测试与原有 7 个 tool 测试）。

- [ ] **Step 5: 提交**

```bash
git add crates/parrot-core/src/tool.rs
git commit -m "feat(core): ToolContext 增加会话级已读文件集合 files_read"
```

---

### Task 2: engine 注入会话级已读集合

**Files:**
- Modify: `crates/parrot-core/src/engine.rs`（struct 定义 33-61 行、`new()` 77-95 行、`run_one_tool` 的 ToolContext 构造 694-697 行）

**Interfaces:**
- Consumes: Task 1 的 `ToolContext::new(...).with_files_read(...)` 与 `SharedFilesRead`。
- Produces: `ReActEngine.files_read: SharedFilesRead` 字段（每会话一份，resume 产生新 engine 时空集合）。

- [ ] **Step 1: 实现**

`engine.rs` 顶部 import 加 `HashSet`：

```rust
use std::collections::HashSet;
```

（`use std::sync::{Arc, Mutex};` 已存在，第 20 行。）

`ReActEngine` struct 末尾（`end_reason` 字段后）追加字段：

```rust
    /// 本会话已成功 file_read 过的文件集合（`ToolContext.files_read` 的
    /// 数据源）。resume 创建新 engine → 集合为空，模型必须重读。
    files_read: SharedFilesRead,
```

import 处补别名（第 10 行改为）：

```rust
use crate::tool::{SharedFilesRead, ToolContext, ToolRegistry};
```

`new()` 的 `Self { ... }` 初始化块中（`end_reason: ...` 行之后）加：

```rust
            files_read: Arc::new(Mutex::new(HashSet::new())),
```

`run_one_tool` 中（原 694-697 行）：

旧：
```rust
        let tool_ctx = ToolContext {
            working_dir: self.working_dir.clone(),
            max_file_size_bytes: 10 * 1024 * 1024,
        };
```
新：
```rust
        let tool_ctx = ToolContext::new(self.working_dir.clone(), 10 * 1024 * 1024)
            .with_files_read(Arc::clone(&self.files_read));
```

- [ ] **Step 2: 验证**

Run: `cargo test -p parrot-core && cargo clippy -p parrot-core --all-targets -- -D warnings`
Expected: 测试全绿，clippy 无警告。行为级验证由 Task 3（read-before-edit）与 Task 10（e2e）覆盖。

- [ ] **Step 3: 提交**

```bash
git add crates/parrot-core/src/engine.rs
git commit -m "feat(core): engine 注入会话级已读集合到 ToolContext"
```

---

### Task 3: `file_read` 成功后标记已读

**Files:**
- Modify: `crates/parrot-tools/src/file_read.rs`（`call` 内 `read_to_string` 之后，约 67-70 行）

**Interfaces:**
- Consumes: `ToolContext.files_read`（Task 1）。
- Produces: `file_read` 成功（含 offset/limit 部分读取）后向集合写入解析后路径。

- [ ] **Step 1: 写失败测试**

`file_read.rs` 末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn successful_read_marks_file_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        let out = FileReadTool.call(json!({"path": "sample.txt"}), &ctx).await.unwrap();
        assert!(!out.is_error);

        assert!(
            ctx.files_read.lock().unwrap().contains(&path),
            "file_read 成功后应把解析后的绝对路径标记为已读"
        );
    }

    #[tokio::test]
    async fn failed_read_does_not_mark_file_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        let out = FileReadTool.call(json!({"path": "missing.txt"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(ctx.files_read.lock().unwrap().is_empty());
    }
}
```

（`Tool` / `ToolContext` / `FileReadTool` 经 `use super::*;` 从本文件顶部 use 继承，勿重复引入。）

- [ ] **Step 2: 运行测试确认失败**

先在 `crates/parrot-tools/Cargo.toml` 末尾加 dev-dependency：

```toml

[dev-dependencies]
tempfile = "3"
```

Run: `cargo test -p parrot-tools file_read`
Expected: `successful_read_marks_file_as_read` FAIL（断言 `contains` 为 false）。

- [ ] **Step 3: 最小实现**

`file_read.rs` 的 `call` 中，`let content = std::fs::read_to_string(...)` 成功之后（原 67-70 行的 map_err 块后、`let offset` 之前）插入：

```rust
        // 标记为已读：file_edit 的 read-before-edit 护栏依赖此集合。
        // 部分读取（offset/limit）同样计入——模型至少见过该文件的内容。
        ctx.files_read.lock().unwrap().insert(path.clone());
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p parrot-tools`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/parrot-tools/Cargo.toml crates/parrot-tools/src/file_read.rs crates/parrot-tools/Cargo.lock
git commit -m "feat(tools): file_read 成功后标记文件为已读"
```

（`Cargo.lock` 若有变动一并提交。）

---

### Task 4: `file_edit` 纯替换逻辑（`replace_exact` / `replace_crlf_normalized` / `nearest_context_hint`）

**Files:**
- Create: `crates/parrot-tools/src/file_edit.rs`（本任务只写纯函数与测试；`lib.rs` 模块声明也在此任务加）

**Interfaces:**
- Produces: `enum MatchOutcome { Replaced { content: String, count: usize }, Ambiguous(usize), NotFound }`（crate 内私有）；`fn replace_exact(original: &str, old: &str, new: &str, replace_all: bool) -> MatchOutcome`；`fn replace_crlf_normalized(original: &str, old: &str, new: &str, replace_all: bool) -> MatchOutcome`；`fn nearest_context_hint(original: &str, old: &str) -> Option<String>`。Task 5 的 `impl Tool` 依赖这三个函数。

- [ ] **Step 1: 建文件并写失败测试**

创建 `crates/parrot-tools/src/file_edit.rs`。先写占位实现（签名照抄 Step 4，函数体 `todo!()`——下一步运行测试看它们 FAIL），再接测试模块：

```rust
//! file_edit：字符串精准匹配替换。
//!
//! 三重护栏：read-before-edit（见 call）、唯一性（Ambiguous）、
//! CRLF 归一匹配（file_read 输出把 CRLF 归一成 LF，模型抄出的
//! old_string 必然是 LF）。

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MatchOutcome {
    Replaced { content: String, count: usize },
    Ambiguous(usize),
    NotFound,
}

fn replace_exact(_original: &str, _old: &str, _new: &str, _replace_all: bool) -> MatchOutcome {
    todo!()
}

fn replace_crlf_normalized(
    _original: &str,
    _old: &str,
    _new: &str,
    _replace_all: bool,
) -> MatchOutcome {
    todo!()
}

fn nearest_context_hint(_original: &str, _old: &str) -> Option<String> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_exact_replaces_unique() {
        let out = replace_exact("a b c", "b", "X", false);
        assert_eq!(
            out,
            MatchOutcome::Replaced { content: "a X c".to_string(), count: 1 }
        );
    }

    #[test]
    fn replace_exact_replace_all_counts_every_occurrence() {
        let out = replace_exact("a b b c", "b", "X", true);
        assert_eq!(
            out,
            MatchOutcome::Replaced { content: "a X X c".to_string(), count: 2 }
        );
    }

    #[test]
    fn replace_exact_multiple_without_replace_all_is_ambiguous() {
        assert_eq!(replace_exact("a b b c", "b", "X", false), MatchOutcome::Ambiguous(2));
    }

    #[test]
    fn replace_exact_not_found() {
        assert_eq!(replace_exact("abc", "zzz", "X", false), MatchOutcome::NotFound);
    }

    #[test]
    fn crlf_normalized_matches_lf_needle_and_restores_crlf() {
        let original = "fn main() {\r\n    println!(\"hello\");\r\n}\r\n";
        let out = replace_crlf_normalized(original, "println!(\"hello\");", "println!(\"edited\");", false);
        match out {
            MatchOutcome::Replaced { content, count } => {
                assert_eq!(count, 1);
                assert_eq!(
                    content,
                    "fn main() {\r\n    println!(\"edited\");\r\n}\r\n",
                    "写回必须保留 CRLF 行尾"
                );
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn crlf_normalized_skips_lf_only_file() {
        assert_eq!(
            replace_crlf_normalized("a\nb\n", "b", "X", false),
            MatchOutcome::NotFound
        );
    }

    #[test]
    fn crlf_normalized_ambiguous_propagates() {
        let original = "x\r\ny\r\nx\r\n";
        assert_eq!(
            replace_crlf_normalized(original, "x", "X", false),
            MatchOutcome::Ambiguous(2)
        );
    }

    #[test]
    fn context_hint_shows_neighborhood_of_first_line_match() {
        let original = "fn a() {}\nfn b() {\n    todo!()\n}\n";
        // 模型抄出的 old_string 首行与文件一致但后续行有偏差
        let hint = nearest_context_hint(original, "fn b() {\n    todo !()\n}");
        let hint = hint.expect("应给出线索");
        assert!(hint.contains("fn b() {"));
        assert!(hint.contains("todo!()"));
        assert!(hint.contains('2'), "片段应带行号");
    }

    #[test]
    fn context_hint_none_when_first_line_absent() {
        assert!(nearest_context_hint("a\nb\n", "zzz\nx").is_none());
    }
}
```

- [ ] **Step 2: 挂载模块**

`crates/parrot-tools/src/lib.rs` 模块声明区（`pub mod file_glob;` 之前按字母序）加：

```rust
pub mod file_edit;
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p parrot-tools file_edit`
Expected: 编译失败或测试 FAIL（函数体是 `todo!()`）。

- [ ] **Step 4: 实现（把 Step 1 占位函数体替换为真实现；enum 保持 Step 1 的唯一一份，勿重复定义；`use` 语句移到文件顶部）**

函数体（enum 已在 Step 1 定义，此处只替换三个函数）：

```rust
/// 精确匹配替换。命中则返回 (新内容, 次数)；未命中 NotFound；
/// 命中多次且未 replace_all 时 Ambiguous。
fn replace_exact(original: &str, old: &str, new: &str, replace_all: bool) -> MatchOutcome {
    let count = original.matches(old).count();
    if count == 0 {
        return MatchOutcome::NotFound;
    }
    if count > 1 && !replace_all {
        return MatchOutcome::Ambiguous(count);
    }
    MatchOutcome::Replaced {
        content: original.replace(old, new),
        count,
    }
}

/// CRLF 归一匹配：haystack 与 needle 均 `\r\n → \n` 后定位替换，再按
/// 原文件含 \r\n 与否统一恢复行尾（混合行尾文件统一化，属可接受行为）。
/// 仅当原文件确实含 \r\n 时才可能命中。
fn replace_crlf_normalized(
    original: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> MatchOutcome {
    if !original.contains('\r') {
        return MatchOutcome::NotFound;
    }
    let norm_original = original.replace("\r\n", "\n");
    let norm_old = old.replace("\r\n", "\n");
    let norm_new = new.replace("\r\n", "\n");
    let count = norm_original.matches(&norm_old).count();
    if count == 0 {
        return MatchOutcome::NotFound;
    }
    if count > 1 && !replace_all {
        return MatchOutcome::Ambiguous(count);
    }
    let updated_lf = norm_original.replace(&norm_old, &norm_new);
    MatchOutcome::Replaced {
        content: updated_lf.replace('\n', "\r\n"),
        count,
    }
}

/// not-found 自纠线索：取 old_string 首行 trim 后在文件中做行级查找，
/// 命中则返回该行附近 ±2 行的原文片段（带行号），帮模型发现行号前缀 /
/// 空白差异导致的抄错。只提示，不自动替换。
fn nearest_context_hint(original: &str, old: &str) -> Option<String> {
    let needle = old.lines().next()?.trim();
    if needle.is_empty() {
        return None;
    }
    let lines: Vec<&str> = original.lines().collect();
    let hit = lines.iter().position(|l| l.trim() == needle)?;
    let start = hit.saturating_sub(2);
    let end = (hit + 3).min(lines.len());
    let mut out = String::from("\nClosest match near:\n");
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("  {:>4}: {}\n", start + i + 1, line));
    }
    Some(out)
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p parrot-tools file_edit`
Expected: 9 个测试全 PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/parrot-tools/src/file_edit.rs crates/parrot-tools/src/lib.rs
git commit -m "feat(tools): file_edit 纯替换逻辑（精确/CRLF归一/自纠线索）"
```

---

### Task 5: `file_edit` Tool 实现（read-before-edit + 落盘）

**Files:**
- Modify: `crates/parrot-tools/src/file_edit.rs`（追加 `FileEditTool` 与 `impl Tool`，以及集成测试）

**Interfaces:**
- Consumes: Task 4 的三个函数；`super::str_arg` / `super::resolve_arg_path`（lib.rs 已有）；`ToolContext.files_read`（Task 1）。
- Produces: `pub struct FileEditTool` + `impl Tool`（`name() == "file_edit"`）。Task 6 注册、Task 10 e2e 依赖。

- [ ] **Step 1: 写失败测试（追加到 file_edit.rs 的 tests 模块）**

```rust
    // ---- Tool 集成测试（追加到上方 tests 模块内）----
    // FileEditTool / Tool / ToolContext / ToolOutput 均经 `use super::*;`
    // 继承自本文件顶部与 impl，勿重复引入。
    use serde_json::json;

    fn fixture_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    async fn run_edit(
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> parrot_protocol::types::ToolOutput {
        FileEditTool.call(args, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn edit_replaces_unique_occurrence() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "println!(\"hello\");", "new_string": "println!(\"edited\");"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Successfully replaced 1 occurrence"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    println!(\"edited\");\n}\n"
        );
    }

    #[tokio::test]
    async fn edit_requires_read_first() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);

        let out = run_edit(&ctx, json!({"path": "a.txt", "old_string": "hello", "new_string": "X"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("File not read yet"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_ambiguous_match_errors() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\nx\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(&ctx, json!({"path": "a.txt", "old_string": "x", "new_string": "y"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("2 locations"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_not_found_includes_hint() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "fn a() {}\nfn b() {\n    todo!()\n}\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "fn b() {\n    todo !()\n}", "new_string": "X"}),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("Closest match near"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_crlf_file_with_lf_needle_succeeds_and_preserves_crlf() {
        let dir = fixture_dir();
        let path = dir.path().join("win.rs");
        std::fs::write(&path, "fn main() {\r\n    println!(\"hello\");\r\n}\r\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "win.rs", "old_string": "println!(\"hello\");", "new_string": "println!(\"edited\");"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\r\n    println!(\"edited\");\r\n}\r\n"
        );
    }

    #[tokio::test]
    async fn edit_identical_strings_is_noop_error() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(&ctx, json!({"path": "a.txt", "old_string": "same", "new_string": "same"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("identical"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_empty_old_string_is_error() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(&ctx, json!({"path": "a.txt", "old_string": "", "new_string": "y"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("must not be empty"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_replace_all_rewrites_every_occurrence() {
        let dir = fixture_dir();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\ny\nx\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), 10 * 1024 * 1024);
        ctx.files_read.lock().unwrap().insert(path.clone());

        let out = run_edit(
            &ctx,
            json!({"path": "a.txt", "old_string": "x", "new_string": "z", "replace_all": true}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("2 occurrence"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "z\ny\nz\n");
    }
```

注意：`use crate::file_edit::FileEditTool;` 等三行 use 若触发 "unused import" 顺序问题，可并入测试模块顶部已有的 `use super::*;` 之后。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p parrot-tools file_edit`
Expected: 编译失败（`FileEditTool` 未定义）。

- [ ] **Step 3: 实现 Tool（追加在 `nearest_context_hint` 之后、`#[cfg(test)]` 之前）**

```rust
use async_trait::async_trait;
use parrot_core::error::AgentError;
use parrot_core::tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;

#[derive(Default)]
pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. The file must be read with file_read first. \
         Copy old_string exactly from the file_read output WITHOUT the line-number prefixes; \
         it must be unique in the file unless replace_all is true."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace; must be unique in the file unless replace_all is true"
                },
                "new_string": {
                    "type": "string",
                    "description": "Text to replace old_string with"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match (optional, default false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput, AgentError> {
        let tool = "file_edit";
        let path_str = super::str_arg(&arguments, "path", tool)?;
        let old_string = super::str_arg(&arguments, "old_string", tool)?;
        let new_string = super::str_arg(&arguments, "new_string", tool)?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Ok(ToolOutput {
                content: "old_string must not be empty".to_string(),
                is_error: true,
            });
        }
        if old_string == new_string {
            return Ok(ToolOutput {
                content: "old_string and new_string are identical; nothing to do".to_string(),
                is_error: true,
            });
        }

        let path = super::resolve_arg_path(path_str, &ctx.working_dir);

        // read-before-edit 护栏：没读过就编辑，大概率是凭空构造 old_string。
        if !ctx.files_read.lock().unwrap().contains(&path) {
            return Ok(ToolOutput {
                content: format!("File not read yet: {:?}. Call file_read first.", path),
                is_error: true,
            });
        }

        let metadata = std::fs::metadata(&path).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot access file {:?}: {}", path, e),
        })?;
        if metadata.len() > ctx.max_file_size_bytes {
            return Ok(ToolOutput {
                content: format!(
                    "File too large: {} bytes (max: {} bytes)",
                    metadata.len(),
                    ctx.max_file_size_bytes
                ),
                is_error: true,
            });
        }

        let original = std::fs::read_to_string(&path).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot read file {:?}: {}", path, e),
        })?;

        // 先精确匹配；失败再 CRLF 归一匹配。
        let outcome = match replace_exact(&original, old_string, new_string, replace_all) {
            MatchOutcome::NotFound => {
                replace_crlf_normalized(&original, old_string, new_string, replace_all)
            }
            other => other,
        };

        let (content, count) = match outcome {
            MatchOutcome::Replaced { content, count } => (content, count),
            MatchOutcome::Ambiguous(n) => {
                return Ok(ToolOutput {
                    content: format!(
                        "old_string matches {n} locations in {:?}. Add surrounding context to make it unique, or set replace_all=true.",
                        path
                    ),
                    is_error: true,
                });
            }
            MatchOutcome::NotFound => {
                let msg = match nearest_context_hint(&original, old_string) {
                    Some(hint) => format!("old_string not found in {:?}.{hint}", path),
                    None => format!(
                        "old_string not found in {:?}. Re-read the file and copy the text exactly.",
                        path
                    ),
                };
                return Ok(ToolOutput {
                    content: msg,
                    is_error: true,
                });
            }
        };

        std::fs::write(&path, content).map_err(|e| AgentError::ToolExecution {
            tool: tool.to_string(),
            message: format!("Cannot write file {:?}: {}", path, e),
        })?;

        // 编辑成功后保留已读状态，允许对同一文件连续编辑（陈旧性由精确
        // 匹配失败兜底）。
        ctx.files_read.lock().unwrap().insert(path.clone());

        Ok(ToolOutput {
            content: format!("Successfully replaced {} occurrence(s) in {:?}", count, path),
            is_error: false,
        })
    }
}
```

（`use` 语句按 rustfmt 习惯放文件顶部即可，不必紧贴 impl。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p parrot-tools file_edit && cargo clippy -p parrot-tools --all-targets -- -D warnings`
Expected: 全部 PASS（Task 4 的 9 个 + 本任务 8 个），clippy 无警告。

- [ ] **Step 5: 提交**

```bash
git add crates/parrot-tools/src/file_edit.rs
git commit -m "feat(tools): file_edit Tool 实现（read-before-edit + 唯一性 + CRLF 护栏）"
```

---

### Task 6: 注册 `file_edit`（`file_write_allowed` 门控）

**Files:**
- Modify: `crates/parrot-tools/src/lib.rs`（`register_all` 50-54 行的 file_write 分支）

**Interfaces:**
- Consumes: `FileEditTool::new()`（Task 5）。

- [ ] **Step 1: 写失败测试**

`crates/parrot-tools/src/lib.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    // ToolRegistry / register_all 经 `use super::*;` 继承自 lib.rs 顶层。
    use super::*;
    use parrot_config::AppConfig;

    #[tokio::test]
    async fn file_edit_gated_by_file_write_allowed() {
        let mut config = AppConfig::default_config();
        config.tools.file_write_allowed = false;
        let registry = ToolRegistry::new();
        register_all(&registry, &config).await;
        assert!(registry.get("file_edit").await.is_none());

        let mut config = AppConfig::default_config();
        config.tools.file_write_allowed = true;
        let registry = ToolRegistry::new();
        register_all(&registry, &config).await;
        assert!(registry.get("file_edit").await.is_some());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p parrot-tools file_edit_gated`
Expected: FAIL（第二个断言 `file_edit` 不存在）。

- [ ] **Step 3: 实现**

`register_all` 中（50-54 行）：

旧：
```rust
    if config.tools.file_write_allowed {
        registry
            .register(std::sync::Arc::new(file_write::FileWriteTool::new()))
            .await;
    }
```
新：
```rust
    if config.tools.file_write_allowed {
        registry
            .register(std::sync::Arc::new(file_write::FileWriteTool::new()))
            .await;
        registry
            .register(std::sync::Arc::new(file_edit::FileEditTool::new()))
            .await;
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p parrot-tools`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/parrot-tools/src/lib.rs
git commit -m "feat(tools): 按 file_write_allowed 注册 file_edit"
```

---

### Task 7: TUI App 状态（`expanded` + `selected_tool` + 三个方法）

**Files:**
- Modify: `src/tui/app.rs`（`ChatEntry::Tool` 28-33 行、`App` struct 58-82 行、`new()` 85-104 行、`apply_event` 的 ToolStart 分支 278-291 行、tests 模块）

**Interfaces:**
- Produces: `ChatEntry::Tool { expanded: bool, .. }`；`App.selected_tool: Option<String>`；`App::select_next_tool(&mut self, forward: bool)`；`App::toggle_selected_tool(&mut self) -> bool`；`App::clear_tool_selection(&mut self)`。Task 8（mod.rs/ui.rs）依赖这些签名。

- [ ] **Step 1: 写失败测试（app.rs tests 模块末尾追加）**

```rust
    fn tool_start_app() -> (App, uuid::Uuid) {
        let sid = uuid::Uuid::new_v4();
        let mut app = App::new(sid);
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid,
            turn_id: uuid::Uuid::new_v4(),
            parent_message_id: uuid::Uuid::new_v4(),
            tool_call_id: "tc_1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        app.apply_event(AgentEvent::ToolStart {
            session_id: sid,
            turn_id: uuid::Uuid::new_v4(),
            parent_message_id: uuid::Uuid::new_v4(),
            tool_call_id: "tc_2".into(),
            tool_name: "file_edit".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        (app, sid)
    }

    #[test]
    fn select_next_tool_cycles_forward_and_backward() {
        let (mut app, _) = tool_start_app();
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_1"));
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_2"));
        app.select_next_tool(true);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_1"), "正向应环绕");

        app.clear_tool_selection();
        app.select_next_tool(false);
        assert_eq!(app.selected_tool.as_deref(), Some("tc_2"), "无选中时反向取最后一个");
    }

    #[test]
    fn toggle_selected_tool_toggles_expansion() {
        let (mut app, _) = tool_start_app();
        app.select_next_tool(true);
        assert!(app.toggle_selected_tool(), "首次切换应成功");
        assert!(matches!(
            &app.entries[0],
            ChatEntry::Tool { expanded: true, .. }
        ));
        assert!(app.toggle_selected_tool());
        assert!(matches!(
            &app.entries[0],
            ChatEntry::Tool { expanded: false, .. }
        ));
    }

    #[test]
    fn toggle_without_selection_is_noop() {
        let (mut app, _) = tool_start_app();
        assert!(!app.toggle_selected_tool());
    }
```

（若 tests 模块已有等价的 ToolStart 构造 helper，可复用；保持断言不变。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot select_next_tool`
Expected: 编译失败（字段/方法不存在）。

- [ ] **Step 3: 实现**

`ChatEntry::Tool` 变体加字段：

```rust
    Tool {
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        result: Option<ToolOutput>,
        /// 是否展开显示完整参数与结果（纯 UI 状态，不持久化；replay 默认紧凑）。
        expanded: bool,
    },
```

`App` struct（`compacting` 字段后）加：

```rust
    /// 当前选中的工具条目（tool_call_id）。Tab 循环选中、Enter 展开。
    pub selected_tool: Option<String>,
```

`new()` 初始化（`compacting: false,` 后）：

```rust
            selected_tool: None,
```

`apply_event` 的 `AgentEvent::ToolStart` 分支，push 时加字段：

```rust
                self.entries.push(ChatEntry::Tool {
                    tool_call_id,
                    tool_name,
                    arguments,
                    result: None,
                    expanded: false,
                });
```

`impl App` 内（`scroll_to_bottom` 等滚动方法附近）追加三个方法：

```rust
    /// Tab / Shift+Tab：在工具条目间循环移动选中。
    pub fn select_next_tool(&mut self, forward: bool) {
        let ids: Vec<&str> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                ChatEntry::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            self.selected_tool = None;
            return;
        }
        let next = match self
            .selected_tool
            .as_deref()
            .and_then(|sel| ids.iter().position(|id| *id == sel))
        {
            Some(i) => {
                if forward {
                    (i + 1) % ids.len()
                } else {
                    (i + ids.len() - 1) % ids.len()
                }
            }
            None => {
                if forward {
                    0
                } else {
                    ids.len() - 1
                }
            }
        };
        self.selected_tool = Some(ids[next].to_string());
    }

    /// 展开/收起选中的工具条目。无选中时返回 false（调用方据此让 Enter
    /// 走发送消息的原路径）。
    pub fn toggle_selected_tool(&mut self) -> bool {
        let Some(sel) = self.selected_tool.clone() else {
            return false;
        };
        for e in self.entries.iter_mut() {
            if let ChatEntry::Tool {
                tool_call_id,
                expanded,
                ..
            } = e
            {
                if *tool_call_id == sel {
                    *expanded = !*expanded;
                    return true;
                }
            }
        }
        false
    }

    /// 单击 Esc 清除选中（双击 Esc 的中断/退出语义不变）。
    pub fn clear_tool_selection(&mut self) {
        self.selected_tool = None;
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --bin parrot`
Expected: 全部 PASS（含原有 app/ui/replay 测试——`ChatEntry::Tool` 构造点只有 `apply_event` 一处，replay_test 只做模式匹配不受影响）。

- [ ] **Step 5: 提交**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): 工具条目展开状态与 Tab 循环选中的 App 状态"
```

---

### Task 8: TUI 渲染（marker + 展开块）与键位（Tab / Esc / Enter）

**Files:**
- Modify: `src/tui/ui.rs`（`entry_lines` 201 行起、`draw_entries` 调用点 145 行、tests 模块）
- Modify: `src/tui/mod.rs`（Esc 分支 146-171 行、Normal 键位 240 行起）

**Interfaces:**
- Consumes: Task 7 的 `selected_tool` / 方法；`ChatEntry::Tool.expanded`。
- Produces: `entry_lines(e: &ChatEntry, selected_tool: Option<&str>, lines: &mut Vec<Line<'_>>)`（私有，ui 内部）。

- [ ] **Step 1: 写失败测试（ui.rs tests 模块追加）**

```rust
    #[test]
    fn expanded_tool_entry_renders_args_and_result() {
        let entry = ChatEntry::Tool {
            tool_call_id: "t1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            result: Some(parrot_protocol::types::ToolOutput {
                content: "1: fn main()".into(),
                is_error: false,
            }),
            expanded: true,
        };
        let mut lines = Vec::new();
        entry_lines(&entry, Some("t1"), &mut lines);

        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>())
            .collect();
        let joined = texts.join("\n");
        assert!(joined.contains("▾"), "展开后 marker 应为 ▾：{joined}");
        assert!(joined.contains("\"path\": \"src/lib.rs\""), "应展示完整参数：{joined}");
        assert!(joined.contains("1: fn main()"), "应展示完整结果：{joined}");
    }

    #[test]
    fn selected_compact_entry_uses_pointer_marker() {
        let entry = ChatEntry::Tool {
            tool_call_id: "t1".into(),
            tool_name: "file_read".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            result: None,
            expanded: false,
        };
        let mut lines = Vec::new();
        entry_lines(&entry, Some("t1"), &mut lines);
        let joined = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>())
            .collect::<String>();
        assert!(joined.contains("❯"), "选中未展开应为 ❯：{joined}");
        assert!(!joined.contains("src/lib.rs"), "未展开不应显示完整参数");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --bin parrot expanded_tool_entry`
Expected: 编译失败（`entry_lines` 参数个数不符）。

- [ ] **Step 3: 实现**

`entry_lines` 签名改为（201 行）：

```rust
fn entry_lines(e: &ChatEntry, selected_tool: Option<&str>, lines: &mut Vec<Line<'_>>) {
```

`ChatEntry::Tool` 分支整体替换为：

```rust
        ChatEntry::Tool {
            tool_call_id,
            tool_name,
            arguments,
            result,
            expanded,
        } => {
            // 单行紧凑展示：`▸ 工具名 关键参数 ✓/✗/…`，避免原始 JSON 和长
            // 结果内容刷屏。选中显示 ❯，展开显示 ▾。
            let args = compact_args(arguments);
            let args_str = if args.is_empty() {
                String::new()
            } else {
                format!(" {}", truncate_str(&args, 48))
            };
            let marker = if *expanded {
                "▾"
            } else if selected_tool == Some(tool_call_id.as_str()) {
                "❯"
            } else {
                "▸"
            };
            let mut spans = vec![Span::styled(
                format!("{marker} {tool_name}{args_str}"),
                Style::default().fg(palette::TOOL_FG),
            )];
            match result {
                Some(r) if r.is_error => {
                    spans.push(Span::styled(
                        format!("  ✗ {}", truncate_str(&r.content, 60)),
                        Style::default().fg(palette::ERROR_FG),
                    ));
                }
                Some(_) => {
                    spans.push(Span::styled("  ✓", Style::default().fg(palette::OK_FG)));
                }
                None => {
                    spans.push(Span::styled("  …", Style::default().fg(palette::DIM)));
                }
            }
            lines.push(Line::from(spans));
            if *expanded {
                // 展开块：完整参数（pretty JSON，DIM）→ 分隔线 → 完整结果。
                let args_json =
                    serde_json::to_string_pretty(arguments).unwrap_or_default();
                for l in args_json.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(palette::DIM),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "  ────────────────",
                    Style::default().fg(palette::DIM),
                )));
                match result {
                    Some(r) => {
                        let fg = if r.is_error {
                            palette::ERROR_FG
                        } else {
                            palette::BODY_FG
                        };
                        if r.content.is_empty() {
                            lines.push(Line::from(Span::styled(
                                "  (empty result)",
                                Style::default().fg(palette::DIM),
                            )));
                        } else {
                            for l in r.content.lines() {
                                lines.push(Line::from(Span::styled(
                                    format!("  {l}"),
                                    Style::default().fg(fg),
                                )));
                            }
                        }
                    }
                    None => {
                        lines.push(Line::from(Span::styled(
                            "  (no result yet)",
                            Style::default().fg(palette::DIM),
                        )));
                    }
                }
                lines.push(Line::from(""));
            }
        }
```

`draw_entries` 的调用点（145 行）改为：

```rust
        entry_lines(e, app.selected_tool.as_deref(), &mut lines);
```

`mod.rs` 的 `handle_key` Normal 分支，在 `KeyCode::PageUp` 之前插入 Tab 处理（Tab 原先落进 `_` 分支会往输入框插入制表符，改作选中导航）：

```rust
            KeyCode::Tab => {
                app.select_next_tool(!k.modifiers.contains(KeyModifiers::SHIFT));
                Ok(None)
            }
```

`KeyCode::Enter` 分支（254-269 行）替换为：

```rust
            KeyCode::Enter => {
                let text = input.lines().join("\n");
                // 有选中工具且输入框为空时，Enter 优先切换该条目展开/收起。
                if text.trim().is_empty() && app.selected_tool.is_some() {
                    app.toggle_selected_tool();
                    return Ok(None);
                }
                if !text.trim().is_empty() {
                    *input = TextArea::default();
                    if let Some(should_quit) = handle_command(app, conn, &text).await? {
                        return Ok(Some(should_quit));
                    }
                    conn.sender
                        .send(ClientMessage::Chat {
                            session_id: app.session_id,
                            message: text,
                        })
                        .await?;
                }
                Ok(None)
            }
```

`run()` 事件循环的 Esc 分支（164-166 行）：

旧：
```rust
                                } else {
                                    last_esc = Some(now);
                                }
```
新：
```rust
                                } else {
                                    last_esc = Some(now);
                                    app.clear_tool_selection();
                                }
```

- [ ] **Step 4: 运行测试并手测**

Run: `cargo test --bin parrot && cargo clippy -p parrot --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全绿。手测（可选但推荐）：`cargo run --bin parrot`，Tab 循环选中工具行，Enter 展开/收起。

- [ ] **Step 5: 提交**

```bash
git add src/tui/ui.rs src/tui/mod.rs
git commit -m "feat(tui): 工具调用详情可展开（Tab 选中 + Enter 切换 + Esc 取消）"
```

---

### Task 9: 抽取共用 `tool_display` 模块 + CLI 详情打印

**Files:**
- Create: `src/tool_display.rs`
- Modify: `src/cli/main.rs`（mod 声明，1-5 行）
- Modify: `src/tui/ui.rs`（删除 `compact_args` 305-321 行与 `truncate_str` 473-482 行，改 use；tests 541-563 行不动）
- Modify: `src/tui/confirm.rs`（第 2 行 use 路径）
- Modify: `src/cli/stream.rs`（MessageDelta ToolCallStart 分支 44-47、ToolStart 51、ToolEnd 53-58）

**Interfaces:**
- Produces: `pub(crate) fn compact_args(args: &Value) -> String` 与 `pub(crate) fn truncate_str(s: &str, max: usize) -> String`（`crate::tool_display`）。TUI 与 CLI 共用。

- [ ] **Step 1: 创建 `src/tool_display.rs`**

内容 = 从 ui.rs 原样搬移的两个函数：

```rust
//! 工具调用参数/文本的展示辅助，TUI 与 CLI 共用。
use serde_json::Value;

/// 将工具参数压缩成单行展示串：单字段对象直接取值（如 `{"path":"x"}` → `x`），
/// 多字段对象退化为紧凑 JSON，由调用方再截断。
pub(crate) fn compact_args(args: &Value) -> String {
    if args.is_null() {
        return String::new();
    }
    if let Some(obj) = args.as_object() {
        if obj.len() == 1 {
            if let Some(v) = obj.values().next() {
                if let Some(s) = v.as_str() {
                    return s.to_string();
                }
            }
        }
    }
    serde_json::to_string(args).unwrap_or_default()
}

/// 按字符数截断（超出补 `…`）。ui 渲染、confirm modal 与 CLI 流式输出共用。
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
```

- [ ] **Step 2: 更新引用**

`src/cli/main.rs` 模块声明区（`mod stream;` 后）加：

```rust
mod tool_display;
```

`src/tui/ui.rs`：删除 `compact_args` 与 `truncate_str` 两个函数定义，顶部 use 区加：

```rust
use crate::tool_display::{compact_args, truncate_str};
```

`src/tui/confirm.rs` 第 2 行改为：

```rust
use crate::tool_display::truncate_str;
```

（ui.rs tests 模块经 `use super::*;` 仍能解析到这两个名字，其测试不用改。）

- [ ] **Step 3: CLI 流式输出**

`src/cli/stream.rs`：

`MessageDelta` 的 `ToolCallStart` 分支改为 no-op（工具打印统一由 `AgentEvent::ToolStart` 负责，那里才有 arguments）：

```rust
                    MessageDeltaPayload::ToolCallStart { .. } => {}
```

`AgentEvent::ToolStart` 分支（原 `{}`）改为：

```rust
                AgentEvent::ToolStart {
                    tool_name, arguments, ..
                } => {
                    let args = crate::tool_display::compact_args(arguments);
                    let args_str = if args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", truncate(&args, 96))
                    };
                    writeln!(stdout, "\n▸ {tool_name}{args_str}")?;
                    stdout.flush()?;
                }
```

`AgentEvent::ToolEnd` 分支改为（成功也打印完整结果；结果已被 registry 策略截断，展示即全量）：

```rust
                AgentEvent::ToolEnd { result, .. } => {
                    if result.is_error {
                        writeln!(stdout, "[Tool error: {}]", result.content)?;
                    } else {
                        writeln!(stdout, "{}", result.content)?;
                    }
                    stdout.flush()?;
                }
```

- [ ] **Step 4: 验证**

Run: `cargo test --bin parrot && cargo clippy -p parrot --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: 全绿。手测（可选）：`cargo run --bin parrot -- -m "list files"`，应看到 `▸ file_read ...` 与完整结果。

- [ ] **Step 5: 提交**

```bash
git add src/tool_display.rs src/cli/main.rs src/cli/stream.rs src/tui/ui.rs src/tui/confirm.rs
git commit -m "feat(cli): tool call 参数与完整结果回传打印；抽取 tool_display 共用模块"
```

---

### Task 10: cassette e2e —— file_edit 真实落盘

**Files:**
- Create: `tests/cassettes/anthropic/chat_stream_file_read.json`
- Create: `tests/cassettes/anthropic/chat_stream_file_edit.json`
- Modify: `tests/cassette_test.rs`（末尾追加测试；dev-dependency 加 parrot-tools）
- Modify: `Cargo.toml`（root `[dev-dependencies]`）

**Interfaces:**
- Consumes: `FileReadTool` / `FileEditTool`（Task 5/6）；cassette 框架（`CassetteProvider::load` / `expect_agent_event` 等，文件内已有）。

- [ ] **Step 1: 两个 cassette**

`tests/cassettes/anthropic/chat_stream_file_read.json`：

```json
{
  "name": "chat_stream_file_read",
  "description": "Emits a single tool_use block for file_read(parrot_file_edit_e2e.txt) then stops with ToolUse.",
  "events": [
    { "ToolCallStart": { "id": "tc_1", "name": "file_read" } },
    { "ToolCallDelta": { "id": "tc_1", "args_delta": "{\"path\":\"parrot_file_edit_e2e.txt\"}" } },
    { "ToolCallEnd": { "id": "tc_1", "arguments": { "path": "parrot_file_edit_e2e.txt" } } },
    { "Finish": { "stop_reason": "ToolUse", "usage": { "input_tokens": 10, "output_tokens": 5 } } }
  ]
}
```

`tests/cassettes/anthropic/chat_stream_file_edit.json`：

```json
{
  "name": "chat_stream_file_edit",
  "description": "Emits a single tool_use block for file_edit (replace println! text) then stops with ToolUse.",
  "events": [
    { "ToolCallStart": { "id": "tc_1", "name": "file_edit" } },
    { "ToolCallDelta": { "id": "tc_1", "args_delta": "{\"path\":\"parrot_file_edit_e2e.txt\",\"old_string\":\"println!(\\\"hello\\\");\",\"new_string\":\"println!(\\\"edited\\\");\"}" } },
    { "ToolCallEnd": { "id": "tc_1", "arguments": { "path": "parrot_file_edit_e2e.txt", "old_string": "println!(\"hello\");", "new_string": "println!(\"edited\");" } } },
    { "Finish": { "stop_reason": "ToolUse", "usage": { "input_tokens": 10, "output_tokens": 5 } } }
  ]
}
```

- [ ] **Step 2: root dev-dependency**

`Cargo.toml` 的 `[dev-dependencies]`（76-80 行）加一行：

```toml
parrot-tools     = { path = "crates/parrot-tools" }
```

- [ ] **Step 3: 写 e2e 测试（tests/cassette_test.rs 末尾追加）**

```rust
// ---------------------------------------------------------------------------
// file_edit e2e：真实 file_read / file_edit 工具经 cassette 驱动，完成一次
// 真实落盘编辑（read-before-edit 护栏必须放行：file_read 先成功）。
// 引擎 working_dir = 测试进程 CWD（包根），故用包根下专名文件并在结束时清理。
// ---------------------------------------------------------------------------

const E2E_FILE: &str = "parrot_file_edit_e2e.txt";

#[tokio::test]
async fn cassette_file_edit_rewrites_file_on_disk() {
    let path = std::path::PathBuf::from(E2E_FILE);
    let _ = std::fs::remove_file(&path); // 清理上次失败残留
    std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").expect("seed file");

    let provider = CassetteProvider::load(&[
        "chat_stream_file_read",
        "chat_stream_file_edit",
        "chat_stream_end_turn",
    ]);

    // spawn 逻辑与 spawn_daemon_with_cassette_provider 相同，但注册真实的
    // file_read / file_edit 工具（该文件自包含，不做共享重构）。
    let port = free_port();
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let token_path = tmp.path().join("token");
    std::fs::create_dir_all(&data_dir).unwrap();

    let auth = parrot_daemon::auth::Auth::new(&token_path)
        .await
        .expect("init auth");
    let token = std::fs::read_to_string(&token_path)
        .unwrap()
        .trim()
        .to_string();
    let auth = Arc::new(auth);

    let provider_registry = Arc::new(ProviderRegistry::new());
    provider_registry
        .register(
            Arc::new(provider) as Arc<dyn LlmProvider>,
            vec!["claude-sonnet-4-6".to_string()],
        )
        .await;

    let tool_registry = Arc::new(ToolRegistry::new());
    tool_registry
        .register(Arc::new(parrot_tools::file_read::FileReadTool::new()) as Arc<dyn Tool>)
        .await;
    tool_registry
        .register(Arc::new(parrot_tools::file_edit::FileEditTool::new()) as Arc<dyn Tool>)
        .await;

    let config = test_config(port, &data_dir, &token_path);
    let daemon_handle = tokio::spawn(async move {
        parrot_daemon::run_with(config, auth, provider_registry, tool_registry)
            .await
            .expect("daemon run_with");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let url = format!("ws://127.0.0.1:{port}");
    let client = parrot_transport::WsTransportClient::new();
    let mut conn = match client.connect(&url, &token).await {
        Ok(c) => c,
        Err(e) => {
            daemon_handle.abort();
            let _ = std::fs::remove_file(&path);
            panic!("client connect: {e}");
        }
    };
    expect_server_message(
        &mut conn.receiver,
        |m| {
            if let ServerMessage::HelloAck { .. } = m {
                Some(())
            } else {
                None
            }
        },
        "HelloAck",
    )
    .await;

    std::mem::forget(tmp);

    let session_id = create_session(&conn.sender, &mut conn.receiver).await;
    conn.sender
        .send(ClientMessage::Chat {
            session_id,
            message: "edit the file".to_string(),
        })
        .await
        .expect("send Chat");

    // 1. ToolStart(file_read) → ToolEnd 成功
    let name = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolStart {
                session_id: sid,
                tool_name,
                ..
            } if *sid == session_id => Some(tool_name.clone()),
            _ => None,
        },
        "ToolStart(file_read)",
    )
    .await;
    assert_eq!(name, "file_read");

    let (content, is_error) = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } if *sid == session_id => Some((result.content.clone(), result.is_error)),
            _ => None,
        },
        "ToolEnd(file_read)",
    )
    .await;
    assert!(!is_error, "file_read failed: {content}");

    // 2. ToolStart(file_edit) → ToolEnd "Successfully replaced 1 occurrence"
    let name = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolStart {
                session_id: sid,
                tool_name,
                ..
            } if *sid == session_id => Some(tool_name.clone()),
            _ => None,
        },
        "ToolStart(file_edit)",
    )
    .await;
    assert_eq!(name, "file_edit");

    let (content, is_error) = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::ToolEnd {
                session_id: sid,
                result,
                ..
            } if *sid == session_id => Some((result.content.clone(), result.is_error)),
            _ => None,
        },
        "ToolEnd(file_edit)",
    )
    .await;
    assert!(!is_error, "file_edit failed: {content}");
    assert!(
        content.contains("Successfully replaced 1 occurrence"),
        "unexpected edit output: {content}"
    );

    // 3. TurnEnd(EndTurn)
    let stop = expect_agent_event(
        &mut conn.receiver,
        |ev| match ev {
            AgentEvent::TurnEnd {
                session_id: sid,
                stop_reason,
                ..
            } if *sid == session_id => Some(stop_reason.clone()),
            _ => None,
        },
        "TurnEnd(EndTurn)",
    )
    .await;
    assert_eq!(stop, TurnStopReason::EndTurn);

    // 4. 落盘断言：文件被真实改写
    let after = std::fs::read_to_string(&path).expect("read edited file");
    assert!(
        after.contains("println!(\"edited\")"),
        "file was not edited: {after}"
    );
    assert!(
        !after.contains("println!(\"hello\")"),
        "old text still present: {after}"
    );

    let _ = std::fs::remove_file(&path);
    daemon_handle.abort();
}
```

- [ ] **Step 4: 运行测试**

Run: `cargo test --test cassette cassette_file_edit_rewrites_file_on_disk`
Expected: PASS。若失败，先看 daemon 日志与 ToolEnd 内容（护栏报错会出现在 `content` 里）。

- [ ] **Step 5: 提交**

```bash
git add tests/cassettes/anthropic/chat_stream_file_read.json tests/cassettes/anthropic/chat_stream_file_edit.json tests/cassette_test.rs Cargo.toml Cargo.lock
git commit -m "test(e2e): cassette 驱动 file_edit 真实落盘编辑"
```

---

### Task 11: replay 紧凑断言 + 备忘录收尾 + 全量验证 + dogfood

**Files:**
- Modify: `src/tui/replay_test.rs`（追加测试）
- Modify: `docs/superpowers/idea备忘录.md`（第 15、42 条）

- [ ] **Step 1: replay 默认紧凑断言（replay_test.rs 末尾追加）**

```rust
#[test]
fn replay_tool_entries_default_compact() {
    let events = load_fixture("multi_turn_session");
    let sid: SessionId = "11111111-1111-1111-1111-111111111111".parse().unwrap();
    let mut app = App::new(sid);
    replay(&mut app, &events);

    match &app.entries[4] {
        ChatEntry::Tool { expanded, .. } => {
            assert!(!expanded, "History replay 重建的工具条目应默认紧凑");
        }
        other => panic!("expected Tool entry, got {:?}", other),
    }
}
```

Run: `cargo test --bin parrot replay_tool_entries_default_compact`
Expected: PASS。

- [ ] **Step 2: 备忘录标记**

`docs/superpowers/idea备忘录.md`：
- 第 15 行 `- [ ] p1 tool call没有返回给client工具调用的参数和tool call结果...` 改为 `- [x] p1 tool call没有返回给client工具调用的参数和tool call结果...`（正文不动）。
- 第 42 行 `- [ ] 需要一个能实现精准代码修改的tool file_edit...` 改为 `- [x] 需要一个能实现精准代码修改的tool file_edit...`。

- [ ] **Step 3: 全量验证**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: 三条全绿。

- [ ] **Step 4: dogfood**

配置 `parrot.toml` 打开 `file_write_allowed = true`，用 Parrot 自己改一处 Parrot 代码（例如让 agent 用 file_edit 改一行注释），验证 TUI 展开详情 + file_edit 真实生效。发现问题回到对应任务修复后再验证。

- [ ] **Step 5: 提交**

```bash
git add src/tui/replay_test.rs docs/superpowers/idea备忘录.md
git commit -m "test(tui): replay 工具条目默认紧凑；备忘录标记 file_edit/详情回传完成"
```
