//! The message being typed: text and a cursor, edited like a text field
//! (Milestone 6.7a). Pure - no terminal - so every key is a table test.

/// One editable line (or several, with Alt+Enter) of user input.
#[derive(Debug, Default)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert(ch);
        }
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Back to the start of this word, or the previous one: separators
    /// first, then the word.
    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// On to the end of this word, or the next one.
    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
    }

    /// The start of the current line.
    pub fn line_start(&mut self) {
        self.cursor = self.line_bounds().0;
    }

    /// The end of the current line.
    pub fn line_end(&mut self) {
        self.cursor = self.line_bounds().1;
    }

    pub fn delete_word_back(&mut self) {
        let start = self.word_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn delete_word_forward(&mut self) {
        let end = self.word_end();
        self.chars.drain(self.cursor..end);
    }

    /// Delete to the end of the line (the newline stays).
    pub fn kill_line_end(&mut self) {
        let end = self.line_bounds().1;
        self.chars.drain(self.cursor..end);
    }

    /// Up a line, keeping the column where it fits. False on the first line.
    pub fn line_up(&mut self) -> bool {
        let (start, _) = self.line_bounds();
        if start == 0 {
            return false;
        }
        let column = self.cursor - start;
        self.cursor = start - 1; // the end of the line above
        let (above, end) = self.line_bounds();
        self.cursor = (above + column).min(end);
        true
    }

    /// Down a line, keeping the column where it fits. False on the last line.
    pub fn line_down(&mut self) -> bool {
        let (start, end) = self.line_bounds();
        if end == self.chars.len() {
            return false;
        }
        let column = self.cursor - start;
        self.cursor = end + 1; // the start of the line below
        let (below, below_end) = self.line_bounds();
        self.cursor = (below + column).min(below_end);
        true
    }

    /// The current line, as `[start, end)` - `end` before its newline.
    fn line_bounds(&self) -> (usize, usize) {
        let start = self.chars[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1);
        let end = self.chars[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.chars.len(), |i| self.cursor + i);
        (start, end)
    }

    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && !self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        let mut i = self.cursor;
        while i < self.chars.len() && !self.chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < self.chars.len() && self.chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Take the text out of the box.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `"ab|c"`: the text, with the cursor where the bar is.
    fn at(marked: &str) -> Input {
        let cursor = marked.chars().position(|c| c == '|').expect("a cursor");
        Input {
            chars: marked.chars().filter(|c| *c != '|').collect(),
            cursor,
        }
    }

    fn show(input: &Input) -> String {
        let mut out: String = input.chars[..input.cursor].iter().collect();
        out.push('|');
        out.extend(&input.chars[input.cursor..]);
        out
    }

    fn after(marked: &str, op: fn(&mut Input)) -> String {
        let mut input = at(marked);
        op(&mut input);
        show(&input)
    }

    #[test]
    fn word_right_jumps_words_and_skips_separators() {
        assert_eq!(
            after("|/mnt/storage/CS", Input::word_right),
            "/mnt|/storage/CS"
        );
        assert_eq!(
            after("/mnt|/storage/CS", Input::word_right),
            "/mnt/storage|/CS"
        );
        assert_eq!(
            after("/mnt/storage|/CS", Input::word_right),
            "/mnt/storage/CS|"
        );
        assert_eq!(
            after("/mnt/storage/CS|", Input::word_right),
            "/mnt/storage/CS|"
        );
    }

    #[test]
    fn word_left_mirrors() {
        assert_eq!(
            after("/mnt/storage/CS|", Input::word_left),
            "/mnt/storage/|CS"
        );
        assert_eq!(
            after("/mnt/storage/|CS", Input::word_left),
            "/mnt/|storage/CS"
        );
        assert_eq!(after("|/mnt", Input::word_left), "|/mnt");
    }

    #[test]
    fn unicode_words() {
        assert_eq!(after("|héllo, wörld", Input::word_right), "héllo|, wörld");
        assert_eq!(after("héllo|, wörld", Input::word_right), "héllo, wörld|");
    }

    #[test]
    fn delete_word_back() {
        assert_eq!(
            after("rename the files|", Input::delete_word_back),
            "rename the |"
        );
        assert_eq!(
            after("rename the |files", Input::delete_word_back),
            "rename |files"
        );
        assert_eq!(after("|files", Input::delete_word_back), "|files");
    }

    #[test]
    fn delete_word_forward() {
        assert_eq!(
            after("rename |the files", Input::delete_word_forward),
            "rename | files"
        );
        assert_eq!(after("files|", Input::delete_word_forward), "files|");
    }

    #[test]
    fn kill_line_end_stops_at_the_newline() {
        assert_eq!(after("ab|cd\nef", Input::kill_line_end), "ab|\nef");
        assert_eq!(after("abcd|", Input::kill_line_end), "abcd|");
    }

    #[test]
    fn line_start_and_end_stay_on_the_current_line() {
        assert_eq!(after("ab\ncd|ef", Input::line_start), "ab\n|cdef");
        assert_eq!(after("ab\ncd|ef", Input::line_end), "ab\ncdef|");
        assert_eq!(after("ab|\ncd", Input::line_end), "ab|\ncd");
        assert_eq!(after("ab\ncd|ef", Input::home), "|ab\ncdef");
        assert_eq!(after("ab|\ncd", Input::end), "ab\ncd|");
    }

    #[test]
    fn line_up_keeps_the_column_clamped() {
        let mut input = at("abcdef\nxy|");
        assert!(input.line_up());
        assert_eq!(show(&input), "ab|cdef\nxy");
        let mut first = at("ab|cdef\nxy");
        assert!(!first.line_up(), "the first line: the caller walks history");
        assert_eq!(show(&first), "ab|cdef\nxy");
        let mut long = at("ab\nwxyz|");
        assert!(long.line_up());
        assert_eq!(show(&long), "ab|\nwxyz", "clamped to the shorter line");
    }

    #[test]
    fn line_down_mirrors() {
        let mut input = at("abcd|ef\nxy");
        assert!(input.line_down());
        assert_eq!(show(&input), "abcdef\nxy|");
        let mut last = at("ab\nx|y");
        assert!(!last.line_down());
    }
}
