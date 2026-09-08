# file_edit 工具 + tool call 详情回传 — Design

**Date:** 2026-09-08
**Status:** Approved (brainstorm 4 问 + 设计已确认)
**Scope:** P0「工具链补齐」——兑现备忘录 tool 第 42 条（file_edit 精准替换）与核心第 15 条（tool call 参数/结果回传 client）

## 1. 目标

1. 新增 `file_edit` 工具：字符串精准匹配替换，让 agent 能独立完成一次真实
   代码修改（产品力天花板），做完立即 dogfood。
2. tool call 详情回传：TUI 可展开查看完整参数+结果；CLI `-m` 打印参数与
   完整结果。**协议零改动**——`AgentEvent::ToolStart{arguments}` /
   `ToolEnd{result}` 已持久化并推送，缺口纯在客户端展示层。

## 2. 已确认的决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 编辑模型 | 字符串精准替换（old_string/new_string） | tree-sitter AST 方案依赖成本高（每语言一个 grammar crate）、接口仍是符号级文本转录（长输出抄错风险更高）；可靠性杠杆在护栏而非 AST。留扩展点：未来可平行加 `file_edit_symbol` |
| TUI 详情形态 | 选中可展开（Tab 循环选中，Enter 切换） | 信息密度与整洁兼得；渲染器是 `Vec<Line>` 平铺，展开只是多产出行，不动滚动几何 |
| CLI 详情 | ToolStart 打印紧凑参数，ToolEnd 打印完整结果 | 与 TUI 对齐，脚本/dogfood 场景看全量 |
| 协议 | 零改动 | ToolStart/ToolEnd 已携带 arguments/result |
| 门控 | 复用 `file_write_allowed` | file_edit 本质是写操作，两次 edit 等价整文件重写，独立开关无安全增益 |

## 3. file_edit 工具

### 3.1 归属与参数

`crates/parrot-tools/src/file_edit.rs`，沿用现有 Tool 模式
（`before_call`/`call`，钩子管线自动覆盖）。

```json
{
  "path": "string",
  "old_string": "string",
  "new_string": "string",
  "replace_all": false   // 可选，默认 false
}
```

### 3.2 执行流程（按序）

1. **read-before-edit 护栏**：目标文件不在已读集合 → 报错
   `File not read yet. Call file_read first`。
   实现：`ToolContext` 新增 `files_read: Arc<Mutex<HashSet<PathBuf>>>`
   （纯内存数据，core 保持 zero-IO）；   engine 每会话持有一份，每次构造
   `ToolContext` 时注入。已读集合以 `resolve_arg_path` 解析后的绝对路径
   为键。`file_read` 成功后写入集合；`file_edit` 成功后
   保留（允许连续编辑，陈旧性风险由精确匹配失败兜底）。resume 后是新
   engine → 集合为空，模型必须重读（正确语义：内容可能已变）。
2. 读文件（UTF-8；超 `max_file_size_bytes` 拒绝；不存在报错）。
3. **匹配**：先精确字节匹配；失败则做 CRLF 归一匹配（haystack 与 needle
   均 `\r\n → \n` 后定位）——**必需**，因为 `file_read` 输出把 CRLF 归一
   成 LF，模型抄出的 old_string 必然是 LF，Windows CRLF 文件不做归一必
   全部 miss。写回行尾规则：若原文件含 `\r\n`，替换后的内容统一以
   `\r\n` 写回；否则以 `\n` 写回（混合行尾文件统一化，属可接受行为）。
4. **唯一性护栏**：非 `replace_all` 时匹配次数 ≠ 1 → 报错并注明实际次数；
   `replace_all` 时替换全部匹配。
5. `old_string == new_string` → no-op 报错。
6. 成功输出：`Successfully replaced N occurrence(s) in "path"`。

### 3.3 失败报错带自纠线索

not-found 时：取 needle 首行 trim 后在文件中做行级查找；命中则附上该行
附近 ±2 行原文片段，帮模型发现行号前缀/空白差异导致的抄错。只提示，
不自动替换。

### 3.4 非目标

模糊自动替换、AST、二进制文件、编辑后语法校验、`file_write` 的
read-before-edit。

## 4. tool call 详情回传

### 4.1 TUI（`src/tui/`）

- `ChatEntry::Tool` 新增 `expanded: bool`（纯 UI 状态，不持久化；History
  replay 重建后默认紧凑）。
- `App` 新增 `selected_tool: Option<String>`（tool_call_id）。
- 键位（`Mode::Normal` 分支）：
  - `Tab` / `Shift+Tab`：在工具条目间循环选中（后移/前移，环绕），高亮
    该行（`▸` → `❯`）；
  - `Enter`：有选中工具且输入框为空 → 切换该条目 `expanded`；否则照旧
    发送消息；
  - 单击 `Esc`：清除选中（与现有双击 Esc 中断/退出兼容——首击仅记录
    时间戳，另加清选中副作用；双击语义不变）。
- 展开渲染：header 行 → args 紧凑 JSON（缩进、DIM 色）→ 分隔线 →
  `result.content` 全文（registry 已按策略截断，展示即全量），错误结果
  用 `ERROR_FG`。

### 4.2 CLI `-m`（`src/cli/stream.rs`）

- `ToolStart`：打印 `▸ {tool_name} {紧凑参数}`；紧凑参数逻辑与 TUI
  `compact_args` 同源，抽到 `src/` 下共用小模块，两处引用。
- `ToolEnd`：成功也打印完整 `result.content`；错误保留现有
  `[Tool error: ...]` 前缀风格。

## 5. 测试与验收

- **file_edit 单测**（`file_edit.rs` `#[cfg(test)]`）：成功替换 /
  `replace_all` / 多匹配报错 / not-found 报错含片段提示 /
  read-before-edit 拦截 / CRLF 文件 + LF needle 命中且写回保留 CRLF /
  `old == new` no-op / 门控未注册时工具不存在。
- **core 单测**：`ToolContext.files_read` 注入、`file_read` 成功标记。
- **TUI**：`app.rs` 补选中循环 / 展开切换 / Enter 优先级用例；
  `replay_test.rs` 补 History 重建后默认紧凑断言。
- **e2e**：`tests/cassettes` 新增 file_edit cassette（mock provider 驱动
  真实替换落盘）。
- **验收命令**：`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo fmt --all -- --check` 全绿。
- **收尾**：备忘录第 15、42 条标记完成；用 Parrot 自己改一处 Parrot
  代码（dogfood）。

## 6. 触及面汇总

| 位置 | 改动 |
|---|---|
| `crates/parrot-core/src/tool.rs` | `ToolContext` 加 `files_read` 字段 |
| `crates/parrot-core/src/engine.rs` | engine 持有会话级已读集合并注入 ctx |
| `crates/parrot-tools/src/file_edit.rs` | 新增工具 + 单测 |
| `crates/parrot-tools/src/file_read.rs` | 成功后标记已读 |
| `crates/parrot-tools/src/lib.rs` | `file_write_allowed` 分支注册 file_edit |
| `src/tui/app.rs` / `ui.rs` / `mod.rs` | 选中 + 展开状态、键位、渲染 |
| `src/cli/stream.rs` | ToolStart/ToolEnd 详情打印 |
| `tests/cassettes/` | file_edit cassette |
| `docs/superpowers/idea备忘录.md` | 第 15、42 条标记完成 |
