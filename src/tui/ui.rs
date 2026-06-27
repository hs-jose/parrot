use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::tui::app::{App, ChatEntry, Mode};
use crate::tui::confirm::format_confirmation;

pub(crate) fn draw(f: &mut ratatui::Frame<'_>, app: &App, input: &tui_textarea::TextArea<'_>) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Min(5),    // entries
            Constraint::Length(3), // input
        ])
        .split(area);
    draw_status(f, chunks[0], app);
    draw_entries(f, chunks[1], app);
    draw_input(f, chunks[2], input);
    if app.mode == Mode::ConfirmPending {
        if let Some(p) = &app.pending_confirmation {
            draw_confirm_modal(f, area, &format_confirmation(p));
        }
    }
}

fn draw_status(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(
            format!(" Parrot  |  session: {}  ", app.session_id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(if app.ended { "| ended" } else { "" }),
    ]);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    f.render_widget(bar, area);
}

fn draw_entries(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::LEFT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut items: Vec<ListItem<'_>> = Vec::new();
    for e in &app.entries {
        items.push(ListItem::new(entry_text(e)));
    }
    let mut state = ListState::default();
    // 滚动偏移：把 cursor 推到底，再减用户向上 offset
    let total = items.len();
    let view_h = inner.height as usize;
    let last = total.saturating_sub(1);
    let selected = last.saturating_sub(app.scroll_offset as usize);
    state.select(Some(selected));
    // 强制 ratatui 不滚过头：用一个新的 List with start_corner 不便，干脆限制 selected 不越过 last - view_h + 1
    let selected_clamped = selected
        .min(last)
        .max(last.saturating_sub(view_h.saturating_sub(1)));
    state.select(Some(selected_clamped));
    let list = List::new(items)
        .style(Style::default())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn entry_text(e: &ChatEntry) -> ratatui::text::Text<'static> {
    let text = match e {
        ChatEntry::User(s) => format!("user> {}", s),
        ChatEntry::Assistant {
            text, completed, ..
        } => {
            if *completed {
                format!("assistant> {}", text)
            } else {
                format!("assistant> {}▌", text)
            }
        }
        ChatEntry::Tool {
            tool_name,
            arguments,
            result,
            ..
        } => {
            let rsummary = match result {
                Some(r) if r.is_error => format!(" [error: {}]", truncate_str(&r.content, 60)),
                Some(r) => format!(" [ok: {}]", truncate_str(&r.content, 60)),
                None => " [...]".to_string(),
            };
            format!("  tool: {} {}{}", tool_name, arguments, rsummary)
        }
        ChatEntry::Error(s) => format!("[error] {}", s),
        ChatEntry::Warning(s) => format!("[warn] {}", s),
    };
    ratatui::text::Text::from(text)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn draw_input(f: &mut ratatui::Frame<'_>, area: Rect, input: &tui_textarea::TextArea<'_>) {
    // tui-textarea 0.7's `block()` is a getter, not a builder setter; render the
    // block ourselves and the textarea into its inner area (mirrors draw_entries).
    let block = Block::default()
        .borders(Borders::TOP)
        .title("Input (Enter to send, Ctrl+C to quit)");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(input, inner);
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
                .title("Confirmation required (y=approve / n=reject / Esc=reject)"),
        )
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    f.render_widget(Clear, modal_area);
    f.render_widget(para, modal_area);
}
