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
    draw_menu(frame, app, chunks[2]);

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
        Span::styled(
            format!("  {}", app.mode_label),
            Style::new().fg(Color::Magenta),
        ),
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
        let doing = app.busy_label(app.busy_for());
        if !doing.is_empty() {
            spans.push(Span::styled(format!(" {doing}"), style));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(" Conversation ");
    let inner = block.inner(area);
    let width = inner.width.max(1) as usize;
    let (mut lines, work_lines) = transcript_lines(&app.entries, width, app.show_steps);
    // What waits for the running turn: dim, at the end, as in T3 Code.
    for queued in app.queue().into_iter().flatten() {
        let what = if queued.steer {
            "returned steer"
        } else {
            "queued"
        };
        lines.push(Line::raw(""));
        for wrapped in wrap::wrap(
            &format!("⋯ {what}: {}", queued.text),
            width.saturating_sub(2).max(1),
        ) {
            lines.push(Line::styled(
                format!("  {wrapped}"),
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
    }

    app.max_scroll = lines.len().saturating_sub(inner.height as usize);
    if app.scroll > app.max_scroll {
        app.scroll = app.max_scroll;
    }
    let offset = (app.max_scroll - app.scroll).min(u16::MAX as usize) as u16;
    // Where each work row landed on screen, for a click to find it.
    app.work_rows = work_lines
        .into_iter()
        .filter(|(line, _)| {
            *line >= offset as usize && *line < offset as usize + inner.height as usize
        })
        .map(|(line, index)| ((line - offset as usize) as u16 + inner.y, index))
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}

/// The transcript as lines, and where each work row is: (line, entry).
fn transcript_lines(
    entries: &[Entry],
    width: usize,
    show_steps: bool,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let body_width = width.saturating_sub(2).max(1);
    let dim = Style::new().fg(Color::DarkGray);

    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        if let Some(log) = &entry.work {
            let open = log.open || show_steps;
            if log.done {
                rows.push((lines.len(), index));
                let style = if log.failed_turn {
                    Style::new().fg(Color::Red)
                } else {
                    dim
                };
                lines.push(Line::styled(
                    format!("  {}", crate::app::work_row(log, open)),
                    style,
                ));
            }
            if !log.done || open {
                for step in &log.steps {
                    lines.push(step_line(step, body_width));
                }
            }
            continue;
        }
        let (label, colour) = match entry.role {
            Role::User => ("You", Color::Cyan),
            Role::Assistant => ("Jarvis", Color::Green),
            Role::System => ("system", Color::Yellow),
            Role::Error | Role::Work => ("error", Color::Red),
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

    (lines, rows)
}

/// One step: its mark, what it is, and how it went.
fn step_line(step: &jarvis_common::ipc::Step, width: usize) -> Line<'static> {
    let (mark, colour) = match step.state.as_str() {
        "running" => ("◐", Color::Cyan),
        "done" => ("●", Color::Green),
        "failed" | "denied" | "untrusted" => ("✗", Color::Red),
        "declined" | "withheld" => ("⊘", Color::Yellow),
        "deferred" => ("⋯", Color::Magenta),
        _ => ("·", Color::DarkGray),
    };
    let summary: String = step.summary.chars().take(width.saturating_sub(6)).collect();
    let mut spans = vec![
        Span::styled(format!("  {mark} "), Style::new().fg(colour)),
        Span::raw(summary),
    ];
    if let Some(detail) = &step.detail {
        spans.push(Span::styled(
            format!("  {detail}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// The keys that apply in the input box now.
fn input_hint(app: &App) -> String {
    let newline = if app.keyboard_enhanced {
        "Shift+Enter"
    } else {
        "Alt+Enter"
    };
    let queued = app.queue().map_or(0, |q| q.len());
    let queued = if queued > 0 {
        format!("queued: {queued} · ")
    } else {
        String::new()
    };
    if app.is_busy() {
        let (enter, other) = match app.follow_up {
            crate::app::FollowUp::Queue => ("queue", "steer"),
            crate::app::FollowUp::Steer => ("steer", "queue"),
        };
        let flip = if app.keyboard_enhanced {
            format!("Ctrl+Enter {other}")
        } else {
            "/steer <msg>".to_string()
        };
        format!(" {queued}Enter {enter} · {flip} · Ctrl+C cancel turn ")
    } else {
        format!(" {queued}Enter send · {newline} newline · Ctrl+←/→ word ")
    }
}

/// The `/` menu, just above the input box, over the transcript.
fn draw_menu(frame: &mut Frame, app: &App, input: Rect) {
    let Some(rows) = app.menu_rows() else {
        return;
    };
    const SHOWN: usize = 8;
    let selected = app.menu_selected.min(rows.len() - 1);
    let first = selected.saturating_sub(SHOWN - 1);
    let visible = &rows[first..rows.len().min(first + SHOWN)];
    let height = visible.len() as u16 + 2;
    let area = Rect {
        x: input.x,
        y: input.y.saturating_sub(height),
        width: input.width,
        height: height.min(input.y),
    };
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let style = if first + i == selected {
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<10}", c.name), style.fg(Color::Cyan)),
                Span::styled(format!("{:<14}", c.args), style.fg(Color::DarkGray)),
                Span::styled(c.about.to_string(), style),
            ]))
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(Block::bordered().title(" Tab complete · Enter run · Esc close ")),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) -> Option<Position> {
    let block = Block::bordered()
        .title(" Message ")
        .title_bottom(Line::from(input_hint(app)).right_aligned());
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
        Overlay::Models { rows, selected } => draw_models(frame, rows, *selected, area),
        Overlay::Confirm(request) => draw_confirm(frame, &request.prompt, area),
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rows = [
        ("Enter", "send the message"),
        ("Shift+Enter, Alt+Enter", "insert a newline"),
        ("Up / Down", "prompt history, or scroll"),
        ("PgUp / PgDn, wheel", "scroll the conversation"),
        ("Shift+drag", "select text to copy"),
        ("Ctrl+N", "start a new session"),
        ("Ctrl+C", "cancel the running turn, or quit when idle"),
        ("Ctrl+S", "list and switch sessions"),
        ("Ctrl+P", "pick the model"),
        ("Shift+Tab", "cycle plan, interactive, autopilot"),
        (
            "Ctrl+←/→, Ctrl+A/E",
            "word back / forward, start / end of line",
        ),
        ("Ctrl+Backspace / W / K", "delete word back, to end of line"),
        ("/", "the commands menu"),
        ("Ctrl+L", "clear the screen"),
        ("Ctrl+U", "clear the message box"),
        ("Ctrl+R", "reconnect to the daemon"),
        ("Ctrl+Q", "quit"),
        ("F1", "this help"),
    ];

    let commands: Vec<(String, &str)> = crate::commands::COMMANDS
        .iter()
        .map(|c| {
            (
                format!("{} {}", c.name, c.args).trim_end().to_string(),
                c.about,
            )
        })
        .collect();
    let mut lines = Vec::new();
    let all = rows
        .iter()
        .map(|(k, w)| (k.to_string(), *w))
        .chain(commands);
    for (key, what) in all {
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

fn draw_models(frame: &mut Frame, rows: &[crate::app::ModelRow], selected: usize, area: Rect) {
    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(Line::styled(
            "No models configured. Press Esc to close.",
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        rows.iter()
            .map(|row| {
                let marker = if row.current { "● " } else { "  " };
                let tier = if row.remote {
                    "  remote · leaves this machine"
                } else {
                    "  local"
                };
                ListItem::new(Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::Green)),
                    Span::styled(row.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(tier, Style::new().fg(Color::DarkGray)),
                ]))
            })
            .collect()
    };
    let height = (items.len() as u16 + 2).min((area.height * 7 / 10).max(5));
    let popup = centered(area, 70, height);
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(selected.min(rows.len() - 1)));
    }
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered().title(" Model ").title_bottom(
                    Line::from(" ↑↓ choose · Enter switch · Esc close ").right_aligned(),
                ),
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

    fn screen(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_folded_row_draws() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        let id = uuid::Uuid::new_v4();
        app.begin_request(id, crate::app::Pending::Turn);
        app.on_response(jarvis_common::ipc::Response::ok(
            id,
            serde_json::json!({ "response": "done", "timing": { "total_ms": 2000 } }),
        ));
        let shown = screen(&mut app);
        assert!(shown.contains("▸ 0 steps · 2.0 s"), "{shown}");
    }

    #[test]
    fn the_queue_shows_at_the_end_and_in_the_hint() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        app.queues
            .entry("abc".into())
            .or_default()
            .push_back(crate::app::Queued {
                id: uuid::Uuid::new_v4(),
                text: "rename them too".into(),
                steer: false,
            });
        let shown = screen(&mut app);
        assert!(shown.contains("⋯ queued: rename them too"), "{shown}");
        assert!(shown.contains("queued: 1"), "{shown}");
    }

    #[test]
    fn the_menu_draws_above_the_input() {
        let mut app = App::new("llama3.2:1b".into());
        app.input.set("/mo");
        let shown = screen(&mut app);
        assert!(shown.contains("/model"), "{shown}");
        assert!(shown.contains("/mode"), "{shown}");
        app.input.set("/model qwen");
        assert!(
            !screen(&mut app).contains("Tab complete"),
            "closed once an argument is typed"
        );
    }

    #[test]
    fn transcript_has_labels_and_blank_separators() {
        let entries = vec![Entry::user("hello"), Entry::assistant("hi")];
        let (lines, _) = transcript_lines(&entries, 40, false);
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
