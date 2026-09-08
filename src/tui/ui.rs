use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use serde_json::Value;

use crate::tui::app::{App, ChatEntry, Mode};
use crate::tui::confirm::format_confirmation;

/// Tokyo Night 风格调色板。所有 UI 颜色集中管理，方便整体调整。
mod palette {
    use ratatui::style::Color;

    pub const TITLE_BG: Color = Color::Rgb(36, 58, 100); // 标题栏底：深蓝
    pub const TITLE_FG: Color = Color::Rgb(192, 202, 245); // #c0caf5
    pub const STATUS_BG: Color = Color::Rgb(36, 40, 59); // #24283b
    pub const STATUS_FG: Color = Color::Rgb(169, 177, 214); // 较暗文本
    pub const USER_FG: Color = Color::Rgb(125, 207, 255); // 青 #7dcfff
    pub const AI_FG: Color = Color::Rgb(255, 158, 100); // 暖橙 #ff9e64
    pub const TOOL_FG: Color = Color::Rgb(224, 175, 104); // 黄 #e0af68
    pub const OK_FG: Color = Color::Rgb(158, 206, 106); // 绿 #9ece6a
    pub const ERROR_FG: Color = Color::Rgb(247, 118, 142); // 红 #f7768e
    pub const WARN_FG: Color = Color::Rgb(187, 154, 247); // 紫 #bb9af7
    pub const BODY_FG: Color = Color::Rgb(171, 178, 191); // 柔和正文
    pub const DIM: Color = Color::Rgb(86, 95, 137); // 注释灰 #565f89
    pub const INPUT_BORDER: Color = Color::Rgb(122, 162, 247); // 蓝 #7aa2f7
}

pub(crate) fn draw(
    f: &mut ratatui::Frame<'_>,
    app: &mut App,
    input: &ratatui_textarea::TextArea<'_>,
) {
    let area = f.area();
    // 输入区按内容行数动态增长，+2 为上下边框；上限不超过终端高度的 1/3，
    // 避免长输入把对话区挤没。
    let input_lines = input.lines().len().max(1) as u16;
    let max_input = (area.height / 3).max(3);
    let input_lines = input_lines.min(max_input);
    let input_height = input_lines + 2; // 上下边框

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // 标题栏
            Constraint::Min(3),               // 对话卡片（占满剩余）
            Constraint::Length(input_height), // 输入卡片（动态）
            Constraint::Length(1),            // 状态栏
        ])
        .split(area);
    draw_title(f, chunks[0], app);
    draw_entries(f, chunks[1], &mut *app);
    draw_input(f, chunks[2], input);
    draw_status(f, chunks[3], app);
    if app.mode == Mode::ConfirmPending {
        if let Some(p) = &app.pending_confirmation {
            draw_confirm_modal(f, area, &format_confirmation(p));
        }
    }
}

/// 标题栏：深蓝底 + 白色加粗 "Parrot"，后跟会话标题（首条用户消息截断）。
fn draw_title(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        " Parrot",
        Style::default()
            .fg(palette::TITLE_FG)
            .add_modifier(Modifier::BOLD),
    ));
    let session_title = app
        .entries
        .iter()
        .find_map(|e| match e {
            ChatEntry::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .map(|t| truncate_str(&t, 48))
        .unwrap_or_default();
    if !session_title.is_empty() {
        spans.push(Span::styled(
            format!(" · {session_title}"),
            Style::default().fg(palette::TITLE_FG),
        ));
    }
    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(palette::TITLE_BG))
        .alignment(Alignment::Left);
    f.render_widget(bar, area);
}

/// 状态栏：灰色底，显示 模型 / 累计 token / 会话短 id。
fn draw_status(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let model = if app.model.is_empty() {
        "model -".to_string()
    } else {
        app.model.clone()
    };
    let usage = &app.total_usage;
    let usage_str = format!("{} in / {} out", usage.input_tokens, usage.output_tokens);
    let short_sid: String = app.session_id.to_string().chars().take(8).collect();
    let line = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(model, Style::default().fg(palette::STATUS_FG)),
        Span::styled("  ·  ", Style::default().fg(palette::DIM)),
        Span::styled(usage_str, Style::default().fg(palette::STATUS_FG)),
        Span::styled("  ·  ", Style::default().fg(palette::DIM)),
        Span::styled(
            format!("session {short_sid}"),
            Style::default().fg(palette::DIM),
        ),
        Span::raw(if app.ended { "  · ended" } else { "" }),
    ]);
    let bar = Paragraph::new(line).style(Style::default().bg(palette::STATUS_BG));
    f.render_widget(bar, area);
    if let Some(label) = activity_label(app) {
        let right = Line::from(vec![
            Span::styled(
                format!("{}, ", spinner_frame()),
                Style::default().fg(palette::AI_FG),
            ),
            Span::styled(label, Style::default().fg(palette::STATUS_FG)),
            Span::raw(" "),
        ]);
        let right_para = Paragraph::new(right)
            .style(Style::default().bg(palette::STATUS_BG))
            .alignment(Alignment::Right);
        f.render_widget(right_para, area);
    }
}

fn draw_entries(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    // 对话卡片：圆角边框 + 水平内边距，让内容不贴边。
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::DIM))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let n = app.entries.len();
    for (idx, e) in app.entries.iter().enumerate() {
        entry_lines(e, app.selected_tool.as_deref(), &mut lines);
        // 只在消息之间留空行，最后一条之后不加，避免底部多一行空白把
        // 真实内容顶出可见区。
        let is_last = idx + 1 == n;
        if !is_last {
            match e {
                ChatEntry::Error(_) | ChatEntry::Warning(_) => {}
                _ => lines.push(Line::from("")),
            }
        }
    }

    // 流式输出中的 assistant 消息：单独渲染。状态指示（Thinking/Working）
    // 在状态栏右侧固定位置，不再作为聊天行插入，避免底部锚定视图反复位移。
    if let Some(text) = app.streaming_text() {
        if !text.trim().is_empty() {
            lines.push(header_line("●", palette::AI_FG, None));
            push_markdown(text, &mut lines);
        }
    }

    let view_h = inner.height as usize;
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });

    // 用 ratatui 的 line_count 精确算折行后的真实可视行数（与渲染一致），
    // 避免手动 div_ceil 估算偏差导致末尾内容被推出可见区。
    let total_visual_lines = if inner.width > 0 {
        para.line_count(inner.width)
    } else {
        0
    };
    let max_scroll = total_visual_lines.saturating_sub(view_h);
    app.view_height = inner.height;
    app.clamp_scroll(max_scroll.min(u16::MAX as usize) as u16);
    let scroll = max_scroll.saturating_sub(app.scroll_offset as usize);

    f.render_widget(para.scroll((scroll as u16, 0)), inner);
}

/// 构造消息标题行：`> 14:32:05`（用户）/ `● 14:32:10`（助手）。
/// time 为 None 时省略时间部分（流式渲染中）。
fn header_line(role: &str, fg: Color, time: Option<&str>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        role.to_string(),
        Style::default().fg(fg).add_modifier(Modifier::BOLD),
    ));
    if let Some(t) = time {
        spans.push(Span::styled(
            format!(" · {t}"),
            Style::default().fg(palette::DIM),
        ));
    }
    Line::from(spans)
}

fn entry_lines(e: &ChatEntry, selected_tool: Option<&str>, lines: &mut Vec<Line<'_>>) {
    match e {
        ChatEntry::User { text, time } => {
            lines.push(header_line(">", palette::USER_FG, Some(time)));
            push_body(text, lines);
        }
        ChatEntry::Assistant { text, time } => {
            lines.push(header_line("●", palette::AI_FG, Some(time)));
            push_markdown(text, lines);
        }
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
                let args_json = serde_json::to_string_pretty(arguments).unwrap_or_default();
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
        ChatEntry::Error(s) => {
            lines.push(Line::from(Span::styled(
                format!("[error] {s}"),
                Style::default().fg(palette::ERROR_FG),
            )));
        }
        ChatEntry::Warning(s) => {
            lines.push(Line::from(Span::styled(
                format!("[warn] {s}"),
                Style::default().fg(palette::WARN_FG),
            )));
        }
        ChatEntry::Shell {
            command,
            output,
            exit_code,
        } => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("▸ !{command}"),
                    Style::default().fg(palette::TOOL_FG),
                ),
                match exit_code {
                    Some(0) => Span::styled("  ✓", Style::default().fg(palette::OK_FG)),
                    Some(_) => Span::styled("  ✗", Style::default().fg(palette::ERROR_FG)),
                    None => Span::styled("  …", Style::default().fg(palette::DIM)),
                },
            ]));
            if let Some(out) = output {
                let style = Style::default().fg(if *exit_code == Some(0) {
                    palette::BODY_FG
                } else {
                    palette::ERROR_FG
                });
                let all_lines: Vec<&str> = out.lines().collect();
                let truncated = all_lines.len() > 40;
                let shown: String = all_lines
                    .iter()
                    .take(40)
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                push_body_styled(&shown, lines, style);
                if truncated {
                    lines.push(Line::from(Span::styled(
                        "…（已截断）",
                        Style::default().fg(palette::DIM),
                    )));
                }
            }
        }
        ChatEntry::Info(s) => {
            lines.push(Line::from(Span::styled(
                s.clone(),
                Style::default().fg(palette::DIM),
            )));
        }
    }
}

/// 将工具参数压缩成单行展示串：单字段对象直接取值（如 `{"path":"x"}` → `x`），
/// 多字段对象退化为紧凑 JSON，由调用方再截断。
fn compact_args(args: &Value) -> String {
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

/// 将正文按行展开 + 柔和正文色。连续空行压缩为一行,行首/行尾的
/// 空行去掉,避免"空行刷屏"和与消息间空行叠加。
fn push_body(text: &str, lines: &mut Vec<Line<'_>>) {
    push_body_styled(text, lines, Style::default().fg(palette::BODY_FG));
}

/// Markdown 渲染:tui-markdown 解析为 styled Lines。
/// 标题/代码块样式在 Line 级,须 patch 到每个 span;流式中的未闭合
/// 结构按纯文本呈现,闭合后自动升级为富格式。
fn push_markdown(text: &str, lines: &mut Vec<Line<'_>>) {
    let options = tui_markdown::Options::new(ThemeSheet);
    let md = tui_markdown::from_str_with_options(text, &options);
    for line in md.lines {
        let line_style = line.style;
        let mut spans: Vec<Span<'_>> = line
            .spans
            .into_iter()
            .map(|s| {
                let st = patch_body(line_style.patch(s.style));
                Span::styled(s.content.into_owned(), st)
            })
            .collect();
        if spans.is_empty() {
            spans.push(Span::default());
        }
        lines.push(Line::from(spans));
    }
}

/// 库默认无样式的 span 视为主题正文色,有样式的保持。
fn patch_body(st: ratatui::style::Style) -> ratatui::style::Style {
    if st == ratatui::style::Style::new() {
        st.fg(palette::BODY_FG)
    } else {
        st
    }
}

/// tui-markdown 样式表,对齐 Tokyo Night 调色板。
#[derive(Clone, Copy)]
struct ThemeSheet;

impl tui_markdown::StyleSheet for ThemeSheet {
    fn heading(&self, level: u8) -> ratatui::style::Style {
        match level {
            1 => Style::default().fg(palette::INPUT_BORDER).bold(),
            2..=3 => Style::default().fg(palette::USER_FG).bold(),
            _ => Style::default().fg(palette::USER_FG),
        }
    }

    fn code(&self) -> ratatui::style::Style {
        Style::default().fg(palette::OK_FG)
    }

    fn link(&self) -> ratatui::style::Style {
        Style::default().fg(palette::INPUT_BORDER)
    }

    fn blockquote(&self) -> ratatui::style::Style {
        Style::default().fg(palette::DIM)
    }
}

fn push_body_styled(text: &str, lines: &mut Vec<Line<'_>>, style: Style) {
    if text.trim().is_empty() {
        lines.push(Line::default());
        return;
    }
    let mut pending_blank = false;
    let mut emitted = false;
    for l in text.lines() {
        if l.trim().is_empty() {
            if emitted {
                pending_blank = true;
            }
            continue;
        }
        if pending_blank {
            lines.push(Line::default());
            pending_blank = false;
        }
        lines.push(Line::from(Span::styled(l.to_string(), style)));
        emitted = true;
    }
}

fn draw_input(f: &mut ratatui::Frame<'_>, area: Rect, input: &ratatui_textarea::TextArea<'_>) {
    // 输入卡片：圆角蓝边框标示焦点；快捷键提示收进边框标题，不再占用内容行。
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::INPUT_BORDER))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " Enter 发送 · Shift+Enter 换行 · 粘贴多行自动换行 · PgUp/PgDn 翻页 · Ctrl+C 中断 · 双击 Esc 退出 ",
            Style::default().fg(palette::DIM),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // 左侧 "> " 提示符列，textarea 随其后渲染。
    let prompt_w = 2;
    let prompt_area = Rect {
        x: inner.x,
        y: inner.y,
        width: prompt_w,
        height: inner.height,
    };
    let prompt = Paragraph::new(Span::styled(
        "> ",
        Style::default()
            .fg(palette::INPUT_BORDER)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(prompt, prompt_area);

    let text_area = Rect {
        x: inner.x + prompt_w,
        y: inner.y,
        width: inner.width.saturating_sub(prompt_w),
        height: inner.height,
    };
    f.render_widget(input, text_area);
}

/// 状态栏右侧的活动指示：spinner + 状态标签，位置固定不推动聊天内容。
fn activity_label(app: &App) -> Option<String> {
    if app.compacting {
        Some("Compacting…".into())
    } else if app.is_working() {
        Some("Working…".into())
    } else if app.is_thinking() {
        Some("Thinking…".into())
    } else if app.is_turn_active() {
        Some("Streaming…".into())
    } else {
        None
    }
}

fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[(ms / 100) as usize % FRAMES.len()]
}

/// 按字符数截断（超出补 `…`）。ui 渲染与 confirm modal 共用。
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn draw_confirm_modal(f: &mut ratatui::Frame<'_>, area: Rect, text: &str) {
    let width = 60.min(area.width);
    let height = 14.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let modal_area = Rect::new(x, y, width, height);
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Confirmation (y=approve / n=reject / Esc=reject) "),
        )
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    f.render_widget(Clear, modal_area);
    f.render_widget(para, modal_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题/代码块样式在 Line 级,接线必须 patch 到 span,否则 markdown
    /// 结构与正文视觉无差别(用户报告的"没渲染")。
    #[test]
    fn push_markdown_applies_line_level_styles() {
        let raw = "## 标题:方案\n\n```\n① dir /s x.txt\n```\n";
        let mut lines = Vec::new();
        push_markdown(raw, &mut lines);
        let heading = &lines[0];
        assert!(
            heading
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "heading must be bold, got {:?}",
            heading.spans
        );
        // 代码块内容行(第 3 行)应为 code 样式而非正文。
        let code_line = &lines[3];
        assert!(
            code_line
                .spans
                .iter()
                .any(|s| s.style.fg == Some(palette::OK_FG)),
            "code line must use code style, got {:?}",
            code_line.spans
        );
    }

    use parrot_protocol::types::ToolOutput;
    use parrot_protocol::SessionId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn compact_args_single_field_object_uses_value() {
        let v = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(compact_args(&v), "src/main.rs");
    }

    #[test]
    fn compact_args_multi_field_falls_back_to_json() {
        let v = serde_json::json!({"a": 1, "b": 2});
        let s = compact_args(&v);
        assert!(s.contains("\"a\":1"));
        assert!(s.contains("\"b\":2"));
    }

    #[test]
    fn compact_args_null_is_empty() {
        assert_eq!(compact_args(&Value::Null), "");
    }

    #[test]
    fn truncate_str_appends_ellipsis() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello…");
    }

    /// 用 TestBackend 在多种终端尺寸下真实渲染一遍，保证布局在小窗口下
    /// 不 panic（边框/padding/折行/滚动计算的边界组合）。
    #[test]
    fn draw_renders_without_panic_at_various_sizes() {
        for (w, h) in [(80u16, 24u16), (40, 12), (30, 8), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut app = App::new(SessionId::new_v4());
            app.entries.push(ChatEntry::User {
                text: "你好，帮我看一下 main.rs 的结构".into(),
                time: "14:32".into(),
            });
            app.entries.push(ChatEntry::Tool {
                tool_call_id: "t1".into(),
                tool_name: "file_read".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
                result: Some(ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                }),
                expanded: false,
            });
            app.entries.push(ChatEntry::Assistant {
                text: "这个文件的结构如下：……".into(),
                time: "14:33".into(),
            });
            app.entries.push(ChatEntry::Error("boom".into()));
            let input = ratatui_textarea::TextArea::default();
            terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        }
    }

    /// 渲染出的对话卡片应带圆角边框（╭/╰），输入卡片在对话卡片下方。
    #[test]
    fn draw_renders_rounded_borders_around_chat_and_input() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        let input = ratatui_textarea::TextArea::default();
        terminal.draw(|f| draw(f, &mut app, &input)).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            text.contains('╭'),
            "expected rounded top corner, got:\n{text}"
        );
        assert!(
            text.contains('╰'),
            "expected rounded bottom corner, got:\n{text}"
        );
    }

    #[test]
    fn draw_clamps_scroll_offset_to_actual_range() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = App::new(SessionId::new_v4());
        for i in 0..50 {
            app.entries.push(ChatEntry::User {
                text: format!("line {i}"),
                time: "x".into(),
            });
        }
        app.scroll_to_top();
        terminal
            .draw(|f| draw(f, &mut app, &ratatui_textarea::TextArea::default()))
            .unwrap();
        assert!(
            app.scroll_offset < u16::MAX,
            "overshoot must be clamped, got {}",
            app.scroll_offset
        );
        assert!(app.scroll_offset > 0);
    }

    #[test]
    fn push_body_collapses_consecutive_blank_lines() {
        let mut lines = Vec::new();
        push_body("line1\n\n\n\nline2", &mut lines);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(
            rendered,
            vec!["line1".to_string(), "".to_string(), "line2".to_string()]
        );
    }

    #[test]
    fn push_body_drops_leading_and_trailing_blank_lines() {
        let mut lines = Vec::new();
        push_body("\n\nline1\n\n\n", &mut lines);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(rendered, vec!["line1".to_string()]);
    }

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
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        let joined = texts.join("\n");
        assert!(joined.contains("▾"), "展开后 marker 应为 ▾：{joined}");
        assert!(
            joined.contains("\"path\": \"src/lib.rs\""),
            "应展示完整参数：{joined}"
        );
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
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<String>();
        assert!(joined.contains("❯"), "选中未展开应为 ❯：{joined}");
        assert!(
            !joined.contains("\"path\": \"src/lib.rs\""),
            "未展开不应显示完整参数（pretty JSON）：{joined}"
        );
    }
}
