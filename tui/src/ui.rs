//! Drawing. No state changes belong here beyond recording scroll limits that
//! the key handler needs.

use crate::app::{App, DaemonState, Entry, Overlay, Role};
use crate::wrap;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, chunks[0]);
    draw_transcript(frame, app, chunks[1]);
    let cursor = draw_input(frame, app, chunks[2]);
    draw_footer(frame, app, chunks[3]);

    if app.overlay.is_some() {
        draw_overlay(frame, app, area);
    } else if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let state_style = match app.state {
        DaemonState::Connecting => Style::new().fg(Color::Yellow),
        DaemonState::Ready => Style::new().fg(Color::Green),
        DaemonState::Offline(_) => Style::new().fg(Color::Red),
    };
    let dim = Style::new().fg(Color::DarkGray);

    let mut spans = vec![
        Span::styled(
            " JARVIS ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", app.model), dim),
        Span::raw("   "),
        Span::styled(app.daemon_label(), state_style),
    ];

    if let Some(session) = &app.session_id {
        spans.push(Span::styled(
            format!("   session {}", short_id(session)),
            dim,
        ));
    }

    if app.is_busy() {
        let (label, style) = if app.cancelling {
            ("   cancelling", Style::new().fg(Color::Yellow))
        } else {
            ("   ", Style::new().fg(Color::Cyan))
        };
        spans.push(Span::styled(
            format!("{label}{}", SPINNER[app.spinner % SPINNER.len()]),
            style,
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(" Conversation ");
    let inner = block.inner(area);
    let width = inner.width.max(1) as usize;
    let lines = transcript_lines(&app.entries, width);

    app.max_scroll = lines.len().saturating_sub(inner.height as usize);
    if app.scroll > app.max_scroll {
        app.scroll = app.max_scroll;
    }
    let offset = (app.max_scroll - app.scroll).min(u16::MAX as usize) as u16;

    frame.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}

fn transcript_lines(entries: &[Entry], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let body_width = width.saturating_sub(2).max(1);
    let dim = Style::new().fg(Color::DarkGray);

    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        let (label, colour) = match entry.role {
            Role::User => ("You", Color::Cyan),
            Role::Assistant => ("Jarvis", Color::Green),
            Role::System => ("system", Color::Yellow),
            Role::Error => ("error", Color::Red),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        )));

        let body_style = match entry.role {
            Role::System | Role::Error => dim,
            _ => Style::new(),
        };
        for wrapped in wrap::wrap(&entry.text, body_width) {
            lines.push(Line::styled(format!("  {wrapped}"), body_style));
        }
        if let Some(footer) = &entry.footer {
            lines.push(Line::styled(format!("  {footer}"), dim));
        }
    }

    lines
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) -> Option<Position> {
    let block = Block::bordered()
        .title(" Message ")
        .title_bottom(Line::from(" Enter send · Alt+Enter newline ").right_aligned());
    let inner = block.inner(area);
    if inner.height == 0 {
        frame.render_widget(block, area);
        return None;
    }

    let width = inner.width.max(1) as usize;
    let text = app.input.text();
    let layout = wrap::layout(&text, width, Some(app.input.cursor()));
    let (cursor_row, cursor_column) = layout.cursor.unwrap_or((0, 0));

    let visible = inner.height as usize;
    let offset = cursor_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = layout
        .lines
        .iter()
        .skip(offset)
        .map(|line| Line::raw(line.clone()))
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), area);

    let cursor = Position::new(
        inner.x + cursor_column.min(width.saturating_sub(1)) as u16,
        inner.y + (cursor_row - offset) as u16,
    );
    Some(cursor)
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![Span::styled(
        " PgUp/PgDn scroll · Ctrl+C cancel · Ctrl+S sessions · Ctrl+N new · F1 help · Ctrl+R reconnect · Ctrl+Q quit",
        dim,
    )];
    if app.cancelling {
        spans.push(Span::styled(
            "   cancelling… waiting for the turn to stop",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    if let DaemonState::Offline(_) = app.state {
        spans.push(Span::styled(
            "   [offline]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let Some(overlay) = app.overlay.as_ref() else {
        return;
    };
    match overlay {
        Overlay::Help => draw_help(frame, area),
        Overlay::Sessions { rows, selected } => {
            draw_sessions(frame, rows, *selected, app.session_id.as_deref(), area)
        }
        Overlay::Recall(rows) => draw_recall(frame, rows, area),
        Overlay::Confirm(request) => draw_confirm(frame, &request.prompt, area),
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rows = [
        ("Enter", "send the message"),
        ("Alt+Enter", "insert a newline"),
        ("Up / Down", "prompt history, or scroll"),
        ("PgUp / PgDn", "scroll the conversation"),
        ("Ctrl+N", "start a new session"),
        ("Ctrl+C", "cancel the running turn, or quit when idle"),
        ("Ctrl+S", "list and switch sessions"),
        ("/recall <q>", "search memories across sessions"),
        ("Ctrl+L", "clear the screen"),
        ("Ctrl+U", "clear the message box"),
        ("Ctrl+R", "reconnect to the daemon"),
        ("Ctrl+Q", "quit"),
        ("/new", "start a new session"),
        ("/sessions", "list sessions"),
        ("/switch <id>", "switch to a session"),
        ("/delete <id>", "delete a session"),
        ("/prune", "remove empty sessions"),
        ("/session", "show the current session id"),
        ("/recall <query>", "search memories across sessions"),
        ("/status, /tools", "daemon information"),
        ("/clear, /quit, /help", "clear, leave, this list"),
    ];

    let mut lines = Vec::new();
    for (key, what) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<26}"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Press any key to close.",
        Style::new().fg(Color::DarkGray),
    ));

    let popup = centered(area, 80, (lines.len() as u16 + 2).min(area.height));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" Help "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_confirm(frame: &mut Frame, prompt: &str, area: Rect) {
    let lines = vec![
        Line::raw(""),
        Line::raw(prompt.to_string()),
        Line::raw(""),
        Line::styled(
            "y approve · n deny · Ctrl+C cancel the turn",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ];
    let popup = centered(area, 70, 9);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" Approval required "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_sessions(
    frame: &mut Frame,
    rows: &[crate::app::SessionRow],
    selected: usize,
    current: Option<&str>,
    area: Rect,
) {
    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::styled(
            "No sessions yet. Press Esc to close.",
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        rows.iter()
            .map(|row| {
                let marker = if Some(row.id.as_str()) == current {
                    "▸ "
                } else {
                    "  "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::Green)),
                    Span::styled(row.title.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("  {} msgs", row.message_count),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("  {}", short_id(&row.id)),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect()
    };

    // Keep the popup to the useful part of the screen: a long history
    // should scroll inside the list, not cover the conversation entirely.
    let max_height = (area.height * 7 / 10).max(5);
    let height = (items.len() as u16 + 2).min(max_height);
    let popup = centered(area, 70, height);
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(selected.min(rows.len() - 1)));
    }

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .title(" Sessions ")
                    .title_bottom(Line::from(" Enter switch · Esc close ").right_aligned()),
            )
            .highlight_style(Style::new().bg(Color::DarkGray)),
        popup,
        &mut state,
    );
}

fn centered(area: Rect, width_percent: u16, height: u16) -> Rect {
    let width = area.width * width_percent / 100;
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn draw_recall(frame: &mut Frame, rows: &[crate::app::RecallRow], area: Rect) {
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let sid = if row.session_id.len() > 8 {
                &row.session_id[..8]
            } else {
                &row.session_id
            };
            let text = format!("[{}] {}: {}", sid, row.role, row.snippet);
            ListItem::new(Line::from(text))
        })
        .collect();

    let list = List::new(items).block(Block::bordered().title(" Recall (Esc to close) "));
    let popup = centered(area, 90, (rows.len() as u16 + 4).min(area.height * 2 / 3));
    frame.render_widget(Clear, popup);
    frame.render_widget(list, popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_has_labels_and_blank_separators() {
        let entries = vec![Entry::user("hello"), Entry::assistant("hi")];
        let lines = transcript_lines(&entries, 40);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(text, vec!["You", "  hello", "", "Jarvis", "  hi"]);
    }

    #[test]
    fn a_small_popup_stays_inside_the_area() {
        let area = Rect::new(0, 0, 10, 6);
        let popup = centered(area, 70, 20);
        assert_eq!(popup.height, 6);
        assert!(popup.width <= area.width);
    }
}
