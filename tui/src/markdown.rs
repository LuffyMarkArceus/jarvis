//! Markdown in answers (Milestone 6.7b), parsed with `pulldown-cmark` and
//! drawn as styled lines. Anything it cannot style is kept as text: a
//! renderer must never lose part of an answer.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

/// `text` as lines no wider than `width`.
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = Renderer {
        width: width.max(4),
        ..Renderer::default()
    };
    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
    for event in parser {
        r.event(event);
    }
    r.flush();
    if let Some(code) = r.code.take() {
        r.code_block(&code.1, &code.0);
    }
    r.lines
}

#[derive(Default)]
struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    /// The paragraph being built.
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Open lists: the next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    /// The marker the next line of an item starts with.
    marker: Option<String>,
    quotes: usize,
    /// An open code block: its language and text so far.
    code: Option<(String, String)>,
    links: Vec<String>,
    table: Option<Table>,
    /// A block has been written: the next top-level one is set apart.
    wrote: bool,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let next = f(self.style());
        self.styles.push(next);
    }

    fn text(&mut self, text: &str, style: Style) {
        if let Some(table) = &mut self.table {
            table.cell.push_str(text);
            return;
        }
        self.spans.push(Span::styled(text.to_string(), style));
    }

    /// Set a top-level block apart from the one before.
    fn start_block(&mut self) {
        self.flush();
        if self.wrote && self.lists.is_empty() && self.quotes == 0 {
            self.lines.push(Line::raw(""));
        }
    }

    fn indent(&self) -> String {
        "  ".repeat(self.lists.len().saturating_sub(1))
    }

    fn quote_bar(&self) -> String {
        "│ ".repeat(self.quotes)
    }

    /// Write the paragraph built so far, wrapped.
    fn flush(&mut self) {
        if self.spans.is_empty() && self.marker.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let marker = self.marker.take().unwrap_or_default();
        let first = format!("{}{}{}", self.quote_bar(), self.indent(), marker);
        let rest = format!(
            "{}{}{}",
            self.quote_bar(),
            self.indent(),
            " ".repeat(marker.width())
        );
        let quote = self.quotes > 0;
        for line in wrap(spans, &first, &rest, self.width) {
            let line = if quote {
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|s| {
                            let style = s.style.patch(Style::new().fg(Color::Gray));
                            Span::styled(s.content, style)
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                line
            };
            self.lines.push(line);
        }
        self.wrote = true;
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        let dim = Style::new().fg(Color::DarkGray);
        if !lang.is_empty() {
            self.lines
                .push(Line::from(Span::styled(format!("  {lang}"), dim)));
        }
        let green = Style::new().fg(Color::Green);
        for line in code.lines() {
            let mut shown = format!("  {line}");
            if shown.width() > self.width {
                let mut cut = String::new();
                for ch in shown.chars() {
                    if cut.width() + 2 > self.width {
                        break;
                    }
                    cut.push(ch);
                }
                cut.push('…');
                shown = cut;
            }
            self.lines.push(Line::from(Span::styled(shown, green)));
        }
        self.wrote = true;
    }

    fn event(&mut self, event: Event) {
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(t) => {
                    code.push_str(&t);
                    return;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, code) = self.code.take().unwrap();
                    self.code_block(&lang, &code);
                    return;
                }
                _ => return,
            }
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if self.marker.is_none() {
                        self.start_block();
                    }
                }
                Tag::Heading { .. } => {
                    self.start_block();
                    self.push_style(|s| s.fg(Color::Cyan).add_modifier(Modifier::BOLD));
                }
                Tag::BlockQuote(_) => {
                    self.start_block();
                    self.quotes += 1;
                }
                Tag::CodeBlock(kind) => {
                    self.start_block();
                    let lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    if self.lists.is_empty() {
                        self.start_block();
                    } else {
                        self.flush();
                    }
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.flush();
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => "• ".to_string(),
                    };
                    self.marker = Some(marker);
                }
                Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    self.links.push(dest_url.to_string());
                    self.push_style(|s| s.add_modifier(Modifier::UNDERLINED));
                }
                Tag::Table(_) => {
                    self.start_block();
                    self.table = Some(Table::default());
                }
                Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Item => self.flush(),
                TagEnd::Heading(_) => {
                    self.flush();
                    self.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.quotes = self.quotes.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    self.styles.pop();
                }
                TagEnd::Link => {
                    self.styles.pop();
                    if let Some(url) = self.links.pop() {
                        self.text(&format!(" ({url})"), Style::new().fg(Color::DarkGray));
                    }
                }
                TagEnd::TableCell => {
                    if let Some(t) = &mut self.table {
                        let cell = std::mem::take(&mut t.cell);
                        t.row.push(cell.trim().to_string());
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(t) = &mut self.table {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = self.table.take() {
                        self.table_lines(t);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                let style = self.style();
                self.text(&t, style);
            }
            Event::Code(t) => {
                let style = self.style().fg(Color::Yellow).bg(Color::Rgb(40, 40, 40));
                self.text(&t, style);
            }
            // Raw HTML is kept as it was written.
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t, Style::new()),
            Event::SoftBreak => {
                let style = self.style();
                self.text(" ", style);
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.start_block();
                self.lines.push(Line::styled(
                    "─".repeat(self.width),
                    Style::new().fg(Color::DarkGray),
                ));
                self.wrote = true;
            }
            Event::TaskListMarker(done) => {
                let style = self.style();
                self.text(if done { "[x] " } else { "[ ] " }, style);
            }
            Event::FootnoteReference(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                let style = self.style();
                self.text(&t, style);
            }
        }
    }

    fn table_lines(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                table
                    .rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| cell.width())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for row in &table.rows {
            let cells: Vec<String> = (0..columns)
                .map(|c| {
                    let cell = row.get(c).cloned().unwrap_or_default();
                    let pad = widths[c].saturating_sub(cell.width());
                    format!("{cell}{}", " ".repeat(pad))
                })
                .collect();
            self.lines.push(Line::raw(cells.join(" │ ")));
        }
        self.wrote = true;
    }
}

/// `spans` wrapped to `width` at spaces, styles kept; the first line starts
/// with `first`, the others with `rest`.
fn wrap(spans: Vec<Span<'static>>, first: &str, rest: &str, width: usize) -> Vec<Line<'static>> {
    // Words with their style; a space between words takes the style of the
    // word before it.
    let mut words: Vec<(String, Style)> = Vec::new();
    let mut glue = false;
    for span in spans {
        let mut pieces = span.content.split(' ').peekable();
        let starts_with_space = span.content.starts_with(' ');
        if starts_with_space {
            glue = false;
        }
        while let Some(piece) = pieces.next() {
            if !piece.is_empty() {
                if glue {
                    if let Some(last) = words.last_mut() {
                        if last.1 == span.style {
                            last.0.push_str(piece);
                        } else {
                            words.push((piece.to_string(), span.style));
                            // Joined to the previous word without a space.
                            let n = words.len();
                            words[n - 1].0.insert(0, '\u{0}');
                        }
                    } else {
                        words.push((piece.to_string(), span.style));
                    }
                } else {
                    words.push((piece.to_string(), span.style));
                }
            }
            glue = pieces.peek().is_none() && !piece.is_empty();
            if pieces.peek().is_some() {
                glue = false;
            }
        }
    }
    let mut lines = Vec::new();
    let mut line: Vec<Span<'static>> = vec![Span::raw(first.to_string())];
    let mut used = first.width();
    let mut empty = true;
    for (word, style) in words {
        let joined = word.starts_with('\u{0}');
        let word = word.trim_start_matches('\u{0}').to_string();
        let need = word.width() + usize::from(!empty && !joined);
        if !empty && used + need > width {
            lines.push(Line::from(std::mem::take(&mut line)));
            line.push(Span::raw(rest.to_string()));
            used = rest.width();
            empty = true;
        }
        if !empty && !joined {
            line.push(Span::styled(" ".to_string(), style));
            used += 1;
        }
        used += word.width();
        line.push(Span::styled(word, style));
        empty = false;
    }
    lines.push(Line::from(line));
    // Drop empty prefix spans so texts compare cleanly.
    lines
        .into_iter()
        .map(|l| {
            Line::from(
                l.spans
                    .into_iter()
                    .filter(|s| !s.content.is_empty())
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// The style of the first span holding `needle`.
    fn style_of(lines: &[Line], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span with {needle:?}"))
            .style
    }

    #[test]
    fn headings_are_bold() {
        let lines = render("# Plan\n\nbody", 40);
        assert_eq!(texts(&lines)[0], "Plan");
        assert!(style_of(&lines, "Plan")
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(texts(&lines).contains(&"body".to_string()));
    }

    #[test]
    fn bold_italic_code_inline() {
        let lines = render("a **strong** and *soft* `code` b", 60);
        assert_eq!(texts(&lines), vec!["a strong and soft code b"]);
        assert!(style_of(&lines, "strong")
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(style_of(&lines, "soft")
            .add_modifier
            .contains(Modifier::ITALIC));
        assert_eq!(style_of(&lines, "code").fg, Some(Color::Yellow));
    }

    #[test]
    fn a_fence_keeps_its_lines_and_is_not_rewrapped() {
        let long = "x".repeat(50);
        let md = format!("```rust\nfn main() {{\n    {long}\n}}\n```");
        let lines = render(&md, 30);
        let t = texts(&lines);
        assert!(t[0].contains("rust"), "{t:?}");
        assert_eq!(t[1], "  fn main() {");
        assert!(t[2].ends_with('…') && t[2].chars().count() <= 30, "{t:?}");
        assert_eq!(t[3], "  }");
        assert_eq!(style_of(&lines, "fn main").fg, Some(Color::Green));
    }

    #[test]
    fn nested_lists() {
        let lines = render("- one\n  - inner\n- two\n\n1. first\n2. second", 40);
        assert_eq!(
            texts(&lines),
            vec!["• one", "  • inner", "• two", "", "1. first", "2. second"]
        );
    }

    #[test]
    fn quotes_have_a_bar() {
        let lines = render("> careful here", 40);
        assert_eq!(texts(&lines), vec!["│ careful here"]);
    }

    #[test]
    fn links_show_text_then_url() {
        let lines = render("see [the docs](https://example.org/x)", 60);
        assert_eq!(texts(&lines), vec!["see the docs (https://example.org/x)"]);
        assert_eq!(style_of(&lines, "https://").fg, Some(Color::DarkGray));
    }

    #[test]
    fn a_table_aligns() {
        let lines = render("| a | bb |\n|---|---|\n| ccc | d |", 40);
        let t = texts(&lines);
        assert_eq!(t[0], "a   │ bb");
        assert_eq!(t[1], "ccc │ d ");
    }

    #[test]
    fn an_unclosed_fence_keeps_every_line() {
        // Review Focus 3.
        let lines = render("text\n\n```\nline one\nline two", 40);
        let t = texts(&lines).join("\n");
        assert!(
            t.contains("text") && t.contains("line one") && t.contains("line two"),
            "{t}"
        );
    }

    #[test]
    fn raw_html_stays_as_text() {
        let lines = render("before <b>bold?</b> after", 60);
        let t = texts(&lines).join("\n");
        assert!(
            t.contains("<b>") && t.contains("bold?") && t.contains("after"),
            "{t}"
        );
    }

    #[test]
    fn wrapping_keeps_bold_across_lines() {
        let lines = render("**one two three four five six**", 12);
        assert!(lines.len() >= 2, "{:?}", texts(&lines));
        for line in &lines {
            for span in line.spans.iter().filter(|s| !s.content.trim().is_empty()) {
                assert!(
                    span.style.add_modifier.contains(Modifier::BOLD),
                    "{:?}",
                    texts(&lines)
                );
            }
        }
        assert!(
            texts(&lines).iter().all(|l| l.chars().count() <= 12),
            "{:?}",
            texts(&lines)
        );
    }

    #[test]
    fn punctuation_after_a_style_stays_attached() {
        assert_eq!(
            texts(&render("it is **done**. Next `x`, then *y*!", 60)),
            vec!["it is done. Next x, then y!"]
        );
    }

    #[test]
    fn a_rule_spans_the_width() {
        let lines = render("a\n\n---\n\nb", 10);
        assert!(
            texts(&lines).contains(&"─".repeat(10)),
            "{:?}",
            texts(&lines)
        );
    }
}
