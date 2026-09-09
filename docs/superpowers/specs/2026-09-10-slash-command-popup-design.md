# Slash 命令补全弹窗设计

日期：2026-09-10
状态：已确认（用户逐节审阅）

## 背景与目标

Parrot TUI 目前支持 4 个斜杠命令（`/help` `/usage` `/abort` `/exit`），在
`src/tui/mod.rs` 的 `handle_command` 中硬编码 match。用户输入 `/` 时没有任何
提示，且后续会持续增加命令，硬编码方式不可持续。

本设计引入：

1. 一个可扩展的斜杠命令注册表（数据驱动，新增命令即加数据条目）。
2. 一个输入框上方的补全弹窗：输入 `/` 弹出、随输入过滤（自动查找）、
   ↑/↓ 选择、Enter 立即执行。

交互决策（用户确认）：Enter 立即执行选中命令（不做 Tab 补全文本）；
弹窗悬浮在输入框上方。

## 非目标

- 不做 Tab 补全（用户明确不需要）。
- 不支持参数提示/参数补全（命令当前均无参数；将来需要时再扩展）。
- 不做模糊/子串匹配，第一版用大小写不敏感前缀匹配。
- `!` shell 命令不进弹窗，路径保持不变。
- 不涉及 daemon / parrot-protocol，纯客户端 TUI 功能。

## 1. 模块结构：`src/tui/slash.rs`

新模块，包含注册表、查找/过滤/执行、弹窗状态机三部分。

### 1.1 注册表（纯数据）

```rust
pub(crate) enum SlashAction { Help, Usage, Abort, Exit }

pub(crate) struct SlashCommand {
    pub name: &'static str,        // 不含 "/"，如 "help"
    pub description: &'static str, // 弹窗里显示的一行说明
    pub action: SlashAction,
}

pub(crate) const REGISTRY: &[SlashCommand] = &[
    SlashCommand { name: "help",  description: "显示可用命令",         action: SlashAction::Help },
    SlashCommand { name: "usage", description: "查看本会话 token 用量",  action: SlashAction::Usage },
    SlashCommand { name: "abort", description: "中断当前进行中的轮次",    action: SlashAction::Abort },
    SlashCommand { name: "exit",  description: "退出 Parrot",           action: SlashAction::Exit },
];
```

**扩展方式**：新增命令 = 加一个 `SlashAction` 变体 + 一条 `REGISTRY` 条目 +
分发 match 一个分支。穷尽匹配保证漏写 match 分支编译失败；单元测试保证
name 唯一非空、description 非空。

### 1.2 查找 / 过滤 / 执行

```rust
pub(crate) fn find(name: &str) -> Option<&'static SlashCommand>;   // 精确匹配
pub(crate) fn filter(query: &str) -> Vec<&'static SlashCommand>;   // 大小写不敏感前缀匹配；空 query 返回全量
pub(crate) async fn execute(
    cmd: &SlashCommand,
    app: &mut App,
    conn: &mut Connection,
) -> Result<Option<bool>, Box<dyn std::error::Error>>;
```

`execute` 把现有 `handle_command` 中 `/` 分支的各命令逻辑原样搬入：

| action | 行为 | 返回 |
|---|---|---|
| `Help` | 推入 `ChatEntry::Info` 列出可用命令 | `Some(false)` |
| `Usage` | 读 `app.total_usage` 推入 Info | `Some(false)` |
| `Abort` | turn 活跃发 `ClientMessage::Abort`，否则提示 | `Some(false)` |
| `Exit` | 无副作用 | `Some(true)` |

`Some(true)` 表示调用方应退出 TUI。

### 1.3 弹窗状态机

```rust
pub(crate) struct SlashPopup {
    items: Vec<&'static SlashCommand>, // filter 后的列表
    selected: usize,                   // 选中索引
}

impl SlashPopup {
    pub fn new() -> Self;                    // 全量列表，selected = 0
    pub fn filtered(query: &str) -> Self;    // 按 query 过滤后构造（sync 复活弹窗用）
    pub fn filter(&mut self, query: &str);   // 重算 items，selected 归 0
    pub fn move_up(&mut self);               // 循环
    pub fn move_down(&mut self);             // 循环
    pub fn selected_cmd(&self) -> Option<&'static SlashCommand>;
    pub fn is_empty(&self) -> bool;          // items 为空 → UI 不画
}
```

`App` 新增字段：

```rust
pub slash_popup: Option<SlashPopup>,
pub slash_dismissed_query: Option<String>, // Esc 关闭时的 query，用于防复活
```

## 2. 触发与同步规则（权威推导，无冗余状态位）

弹窗是否活跃完全由当前输入文本推导：

```rust
/// 文本以 "/" 开头、单行、不含空白时返回 "/" 后的子串，否则 None。
fn popup_query(text: &str) -> Option<&str>;
```

每次按键（或粘贴）处理后调用 `sync_slash_popup(app, text)`：

1. `popup_query` 返回 `None` → `slash_popup = None`，清 `slash_dismissed_query`。
2. 返回 `Some(q)` 且弹窗开着 → `popup.filter(q)`。
3. 返回 `Some(q)` 且弹窗已关：若 `slash_dismissed_query != Some(q)` 则重新
   弹出（`SlashPopup::filtered(q)`）；否则保持关闭。

规则 3 的目的：Esc 关闭弹窗后，同一 query 不会在下一个事件循环被重新弹出
（防闪烁）；用户继续输入或退格改变了 query，弹窗自然恢复。

调用点：`run_loop` 的 `UiEvent::Key` 分支在 `handle_key` 返回后统一调用
`sync_slash_popup`；`UiEvent::Paste` 分支同样调用。文本没变时 filter 为幂等
空转，无正确性影响。

## 3. 按键路由

`handle_key`（`Mode::Normal`）中，弹窗存活时（`app.slash_popup.is_some()`）
以下按键优先拦截，先于现有分支：

| 按键 | 行为 |
|---|---|
| `Up`（无 Shift） | `popup.move_up()`（列表循环） |
| `Down`（无 Shift） | `popup.move_down()`（列表循环） |
| `Enter`（无修饰键） | 执行选中命令（见下） |
| `Esc` | 仅关闭弹窗：`slash_popup = None`，`slash_dismissed_query = popup_query(text)`；不改输入文本 |

- `Shift+Up/Down` 不拦截，仍走聊天区滚动（现有分支）。
- `Enter` 执行：`selected_cmd()` 取命令 → 清空输入框（`*input = TextArea::default()`）
  → `execute(cmd, app, conn)` → 返回 `Some(true)` 则退出 TUI。执行后弹窗随
  sync 关闭（输入已空）。
- **Esc 与双击 Esc 的关系（既有事件流的自然结果）**：run_loop 现有的双击
  Esc 计数块在 `handle_key` 之前执行。因此弹窗打开时按 Esc：第一次按下
  会设置 `last_esc` 计数、清除工具选中（既有逻辑），随后 `handle_key` 关闭
  弹窗；若 500ms 内再按一次 Esc，则按既有语义触发中断/退出。这是既有双击
  语义在弹窗场景下的延续，不新增特殊分支。
- `Mode::ConfirmPending` 模式下弹窗不拦截按键（确认模态优先，现有 match
  顺序已保证）。

`handle_command` 改动：`/` 分支改为 `find(cmd)` → 命中则 `execute`；未命中
保持现有"未知命令"Info 提示。`!` 分支与普通消息路径不动。

## 4. 渲染（`src/tui/ui.rs`）

`draw()` 在画完输入卡片之后、ConfirmPending 模态之前，若
`app.mode == Mode::Normal && app.slash_popup 存活且非空`，调用
`draw_slash_popup`。

- **位置**：输入卡片正上方，左边缘与输入卡片对齐，底边贴输入卡片顶边
  （无间隙），覆盖在聊天区之上。
- **尺寸**：宽 = `max(最长行宽 + padding, 40)`，高 = `min(items.len() + 2 边框, 10)`
  （即最多显示 8 项）。项数超过可视高度时用滚动窗口，保证选中项始终可见
  （实现为渲染时的行偏移计算，不改 `SlashPopup` 状态）。
- **样式**：
  - `Clear` widget 先擦底（沿用 `draw_confirm_modal` 先例）。
  - 圆角边框，边框色用 `palette::DIM`，标题 `" 斜杠命令 "`。
  - 每行：`/name`（`palette::USER_FG`）+ 两空格 + description（`palette::DIM`）。
  - 选中行：`❯ ` 前缀（与工具选中标记一致），整行背景 `palette::TITLE_BG`、
    文字 `palette::TITLE_FG`（反色高亮）。
- 状态栏与输入框不显示额外提示（弹窗本身即提示）。

## 5. 边界情况

| 场景 | 行为 |
|---|---|
| 无匹配项 | 不画弹窗；弹窗状态仍存活，退格恢复匹配后重新显示 |
| 粘贴 `/` 开头单行文本 | 走同一 sync 规则，触发弹窗 |
| turn 进行中 | 弹窗照常可用（`/abort` 正是为此设计） |
| 输入 `/help me`（`/` 后含空格）| popup_query 返回 None，弹窗关闭；Enter 走原 `handle_command` 路径报未知命令（注意：`/help ` 尾随空格会被 handle_command trim 成 `help` 而正常执行，这是既有行为） |
| 多行输入（Shift+Enter 换行后） | popup_query 返回 None，弹窗关闭 |
| 空输入 `/` | query 为空，显示全量命令 |
| ConfirmPending 弹出确认 | 弹窗不绘制不响应；期间输入文本不变，弹窗状态原样保留，回到 Normal 后随下一个按键由 sync 校正 |

## 6. 测试

`src/tui/slash.rs` 内联 `#[cfg(test)]`：

- `filter`：前缀命中、大小写不敏感、空 query 全量、无匹配空列表。
- `SlashPopup`：new 全量、filter 后 selected 归 0、move_up/move_down 循环
  （含空列表安全）、selected_cmd。
- 注册表不变量：name 唯一、非空、不以 `/` 开头、description 非空。
- `popup_query`：以 `/` 开头返回子串；多行返回 None；含空格返回 None；
  非 `/` 开头返回 None。
- `sync_slash_popup`：query 变化重新 filter；Esc 关闭后同 query 不复活；
  文本变化后复活；query 失效清 dismissed。
- `execute`：`/exit` → `Some(true)`；`/help` 推 Info 且不发送任何
  ClientMessage；`/usage` 读取 total_usage（复用现有 `test_conn` 模式）。

`src/tui/mod.rs` 测试：

- `handle_key` 路由：弹窗存活时 Up/Down 改变选中而非滚动聊天；Enter 执行
  命令并清空输入框；Esc 关弹窗且输入文本不变；弹窗打开时双击 Esc 的中断/
  退出语义与既有行为一致（第一次关弹窗并计数，500ms 内第二次触发中断/退出）。

`src/tui/ui.rs` 测试（TestBackend）：

- 弹窗激活时渲染出 `/help` 文本与选中标记 `❯`。
- `Mode::ConfirmPending` 时不绘制弹窗。
- 弹窗在输入卡片上方（弹窗底行在输入框顶行之前）。

## 7. 涉及文件清单

| 文件 | 改动 |
|---|---|
| `src/tui/slash.rs` | 新增：注册表、filter/find/execute、SlashPopup、sync_slash_popup、popup_query |
| `src/tui/mod.rs` | `mod` 声明；App 增加 popup 字段（在 app.rs）；`handle_key` 拦截分支；`handle_command` `/` 分支改查注册表；run_loop 接 sync |
| `src/tui/app.rs` | `App` 新增 `slash_popup`、`slash_dismissed_query` 字段 |
| `src/tui/ui.rs` | `draw` 增加弹窗绘制；`draw_slash_popup` |
