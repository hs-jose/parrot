use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

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

    let mut lines: Vec<Line<'_>> = Vec::new();
    for e in &app.entries {
        entry_lines(e, &mut lines);
    }
    if let Some(text) = app.streaming_text() {
        if !text.is_empty() {
            let text_lines: Vec<&str> = text.lines().collect();
            for (i, l) in text_lines.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("assistant> ", Style::default().fg(Color::Cyan)),
                        Span::raw(*l),
                        Span::raw("▌"),
                    ]));
                } else {
                    lines.push(Line::from(format!("          {}", l)));
                }
            }
        }
    }

    let view_h = inner.height as usize;
    let col_width = inner.width as usize;
    let total_visual_lines: usize = lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 || col_width == 0 {
                1
            } else {
                w.div_ceil(col_width)
            }
        })
        .sum();

    let max_scroll = total_visual_lines.saturating_sub(view_h);
    let scroll = max_scroll.saturating_sub(app.scroll_offset as usize);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    f.render_widget(para, inner);
}

fn entry_lines(e: &ChatEntry, lines: &mut Vec<Line<'_>>) {
    let user_style = Style::default().fg(Color::Green);
    let assistant_style = Style::default().fg(Color::Cyan);
    let tool_style = Style::default().fg(Color::Yellow);
    let error_style = Style::default().fg(Color::Red);
    let warn_style = Style::default().fg(Color::Magenta);

    match e {
        ChatEntry::User(s) => {
            for (i, l) in s.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("user> ", user_style),
                        Span::raw(l.to_string()),
                    ]));
                } else {
                    lines.push(Line::from(format!("      {}", l)));
                }
            }
        }
        ChatEntry::Assistant { text, .. } => {
            if text.is_empty() {
                lines.push(Line::from(Span::styled("assistant> ", assistant_style)));
            } else {
                for (i, l) in text.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled("assistant> ", assistant_style),
                            Span::raw(l.to_string()),
                        ]));
                    } else {
                        lines.push(Line::from(format!("          {}", l)));
                    }
                }
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
            lines.push(Line::from(Span::styled(
                format!("  tool: {} {}{}", tool_name, arguments, rsummary),
                tool_style,
            )));
        }
        ChatEntry::Error(s) => {
            lines.push(Line::from(Span::styled(
                format!("[error] {}", s),
                error_style,
            )));
        }
        ChatEntry::Warning(s) => {
            lines.push(Line::from(Span::styled(
                format!("[warn] {}", s),
                warn_style,
            )));
        }
    }
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
