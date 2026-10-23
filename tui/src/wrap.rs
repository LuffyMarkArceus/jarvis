//! Wrapping that keeps the terminal cursor aligned with the text.
//!
//! Ratatui can wrap a paragraph for us, but it does not say where a
//! particular character ended up. The input box needs exactly that to put
//! the cursor back after a line wraps, so wrapping is done here once and
//! both the rendered lines and the cursor position come from the same pass.

use unicode_width::UnicodeWidthChar;

/// The result of laying text out in a fixed-width box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Visual lines, without a trailing newline in any of them.
    pub lines: Vec<String>,
    /// `(row, column)` of the cursor, when one was requested.
    pub cursor: Option<(usize, usize)>,
}

/// Width of a character in terminal cells. Tabs are rarely present in model
/// output, but counting one as a single cell is less wrong than zero.
fn char_width(ch: char) -> usize {
    if ch == '\t' {
        return 1;
    }
    ch.width().unwrap_or(0)
}

/// Lay out `text` at `width` cells per line.
///
/// Breaks at the last space on a line when there is one, otherwise between
/// characters, which is what a terminal does. `cursor` is a character index
/// into `text`; an index at the end of the text is placed after the final
/// character.
pub fn layout(text: &str, width: usize, cursor: Option<usize>) -> Layout {
    let width = width.max(1);
    // Each output character keeps its index in the input, so a word moved to
    // the next line can still be matched with the cursor later.
    let mut lines: Vec<Vec<(usize, char)>> = vec![Vec::new()];
    let mut column = 0usize;

    for (index, ch) in text.chars().enumerate() {
        if ch == '\n' {
            lines.push(Vec::new());
            column = 0;
            continue;
        }

        let w = char_width(ch);
        if column + w > width && column > 0 {
            let line = lines.last_mut().expect("always at least one line");
            match line.iter().rposition(|(_, c)| *c == ' ') {
                Some(space) => {
                    // Move the unfinished word down. The space itself stays
                    // at the end of this line: removing it would move the
                    // cursor if the cursor happened to be on it.
                    let tail: Vec<(usize, char)> = line.drain(space + 1..).collect();
                    column = tail.iter().map(|(_, c)| char_width(*c)).sum();
                    lines.push(tail);
                }
                None => {
                    lines.push(Vec::new());
                    column = 0;
                }
            }
        }

        lines
            .last_mut()
            .expect("always at least one line")
            .push((index, ch));
        column += w;
    }

    let total_chars = text.chars().count();
    let cursor = cursor.map(|cursor| {
        for (row, line) in lines.iter().enumerate() {
            if let Some(column) = line.iter().position(|(index, _)| *index == cursor) {
                return (row, column);
            }
        }
        // The cursor sits after the last character.
        debug_assert!(cursor >= total_chars);
        (
            lines.len() - 1,
            lines.last().map(|line| line.len()).unwrap_or(0),
        )
    });

    Layout {
        lines: lines
            .into_iter()
            .map(|line| line.into_iter().map(|(_, c)| c).collect())
            .collect(),
        cursor,
    }
}

/// Wrap text without tracking a cursor.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    layout(text, width, None).lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_line() {
        assert_eq!(wrap("hello", 20), vec!["hello"]);
    }

    #[test]
    fn wraps_at_a_space() {
        // The space stays at the end of the line it broke; it is invisible,
        // and keeping it means a cursor sitting on it still has a position.
        assert_eq!(wrap("hello world", 6), vec!["hello ", "world"]);
    }

    #[test]
    fn cursor_can_sit_on_the_space_that_wraps() {
        let out = layout("hello world", 6, Some(5));
        assert_eq!(out.cursor, Some((0, 5)));
    }

    #[test]
    fn wraps_long_words_between_characters() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
    }

    #[test]
    fn keeps_explicit_newlines_as_empty_lines() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn empty_text_is_one_line() {
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn cursor_tracks_a_wrapped_line() {
        let out = layout("hello world", 6, Some(7));
        assert_eq!(out.lines, vec!["hello ", "world"]);
        assert_eq!(out.cursor, Some((1, 1)));
    }

    #[test]
    fn cursor_at_the_end_sits_after_the_text() {
        let out = layout("abcdef", 3, Some(6));
        assert_eq!(out.lines, vec!["abc", "def"]);
        assert_eq!(out.cursor, Some((1, 3)));
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let out = layout("你好", 2, Some(2));
        assert_eq!(out.lines, vec!["你", "好"]);
        assert_eq!(out.cursor, Some((1, 1)));
    }

    #[test]
    fn zero_width_does_not_panic_on_an_empty_line() {
        assert_eq!(layout("", 0, Some(0)).cursor, Some((0, 0)));
    }
}
