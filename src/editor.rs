//! A small modal text buffer for the query pane.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
}

#[derive(Debug, Clone)]
pub struct Editor {
    pub lines: Vec<String>,
    pub line: usize,
    pub col: usize,
    pub mode: Mode,
    pub row_off: usize,
    undo: Vec<(Vec<String>, usize, usize)>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            line: 0,
            col: 0,
            mode: Mode::Normal,
            row_off: 0,
            undo: Vec::new(),
        }
    }
}

const UNDO_DEPTH: usize = 200;

impl Editor {
    #[cfg(test)]
    pub fn from_text(text: &str) -> Self {
        let mut e = Self::default();
        e.set_text(text);
        e
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.line = self.lines.len() - 1;
        self.col = self.cur_len();
        self.clamp();
    }

    pub fn is_blank(&self) -> bool {
        self.text().trim().is_empty()
    }

    fn cur_len(&self) -> usize {
        self.lines[self.line].chars().count()
    }

    fn snapshot(&mut self) {
        self.undo.push((self.lines.clone(), self.line, self.col));
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some((lines, line, col)) => {
                self.lines = lines;
                self.line = line;
                self.col = col;
                self.clamp();
                true
            }
            None => false,
        }
    }

    fn clamp(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.line = self.line.min(self.lines.len() - 1);
        let len = self.cur_len();
        // Normal mode rests on a character; insert mode may sit one past the end.
        let max = if self.mode == Mode::Insert {
            len
        } else {
            len.saturating_sub(1)
        };
        self.col = self.col.min(max);
    }

    /// Byte offset of the cursor within `text()`, for statement lookup.
    pub fn byte_cursor(&self) -> usize {
        let mut off = 0;
        for l in &self.lines[..self.line] {
            off += l.len() + 1;
        }
        off + self.lines[self.line]
            .chars()
            .take(self.col)
            .map(char::len_utf8)
            .sum::<usize>()
    }

    // --- movement ---

    pub fn move_line(&mut self, delta: isize) {
        let max = self.lines.len() as isize - 1;
        self.line = (self.line as isize + delta).clamp(0, max) as usize;
        self.clamp();
    }

    pub fn move_col(&mut self, delta: isize) {
        let next = self.col as isize + delta;
        self.col = next.max(0) as usize;
        self.clamp();
    }

    pub fn line_start(&mut self) {
        self.col = 0;
    }

    pub fn line_end(&mut self) {
        self.col = self.cur_len();
        self.clamp();
    }

    pub fn top(&mut self) {
        self.line = 0;
        self.col = 0;
    }

    pub fn bottom(&mut self) {
        self.line = self.lines.len() - 1;
        self.col = 0;
        self.clamp();
    }

    pub fn word_forward(&mut self) {
        let chars: Vec<char> = self.lines[self.line].chars().collect();
        let mut i = self.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() && self.line + 1 < self.lines.len() {
            self.line += 1;
            self.col = 0;
        } else {
            self.col = i;
        }
        self.clamp();
    }

    pub fn word_back(&mut self) {
        let chars: Vec<char> = self.lines[self.line].chars().collect();
        let mut i = self.col;
        while i > 0 && chars.get(i - 1).is_some_and(|c| c.is_whitespace()) {
            i -= 1;
        }
        while i > 0 && chars.get(i - 1).is_some_and(|c| !c.is_whitespace()) {
            i -= 1;
        }
        if i == 0 && self.col == 0 && self.line > 0 {
            self.line -= 1;
            self.col = self.cur_len();
        } else {
            self.col = i;
        }
        self.clamp();
    }

    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.line < self.row_off {
            self.row_off = self.line;
        } else if self.line >= self.row_off + height {
            self.row_off = self.line + 1 - height;
        }
    }

    // --- editing ---

    pub fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
    }

    pub fn enter_insert_after(&mut self) {
        self.mode = Mode::Insert;
        if self.cur_len() > 0 {
            self.col = (self.col + 1).min(self.cur_len());
        }
    }

    pub fn enter_normal(&mut self) {
        self.mode = Mode::Normal;
        self.clamp();
    }

    pub fn insert_char(&mut self, c: char) {
        self.snapshot();
        let byte = self.char_to_byte(self.col);
        self.lines[self.line].insert(byte, c);
        self.col += 1;
    }

    fn char_to_byte(&self, col: usize) -> usize {
        self.lines[self.line]
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(self.lines[self.line].len())
    }

    pub fn newline(&mut self) {
        self.snapshot();
        let byte = self.char_to_byte(self.col);
        let rest = self.lines[self.line].split_off(byte);
        self.lines.insert(self.line + 1, rest);
        self.line += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        self.snapshot();
        if self.col > 0 {
            let byte = self.char_to_byte(self.col - 1);
            self.lines[self.line].remove(byte);
            self.col -= 1;
        } else if self.line > 0 {
            let cur = self.lines.remove(self.line);
            self.line -= 1;
            self.col = self.cur_len();
            self.lines[self.line].push_str(&cur);
        } else {
            self.undo.pop();
        }
    }

    pub fn delete_char(&mut self) {
        if self.cur_len() == 0 {
            return;
        }
        self.snapshot();
        let byte = self.char_to_byte(self.col);
        if byte < self.lines[self.line].len() {
            self.lines[self.line].remove(byte);
        }
        self.clamp();
    }

    pub fn delete_line(&mut self) {
        self.snapshot();
        self.lines.remove(self.line);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.line = self.line.min(self.lines.len() - 1);
        self.col = 0;
        self.clamp();
    }

    pub fn open_below(&mut self) {
        self.snapshot();
        self.lines.insert(self.line + 1, String::new());
        self.line += 1;
        self.col = 0;
        self.mode = Mode::Insert;
    }

    pub fn open_above(&mut self) {
        self.snapshot();
        self.lines.insert(self.line, String::new());
        self.col = 0;
        self.mode = Mode::Insert;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_text_round_trips() {
        let e = Editor::from_text("select 1\nfrom t");
        assert_eq!(e.text(), "select 1\nfrom t");
        assert_eq!(e.lines.len(), 2);
    }

    #[test]
    fn byte_cursor_tracks_multiline_and_unicode() {
        let mut e = Editor::from_text("héllo\nworld");
        e.line = 1;
        e.col = 3;
        assert_eq!(e.byte_cursor(), "héllo\n".len() + 3);
        e.line = 0;
        e.col = 2;
        assert_eq!(e.byte_cursor(), 3); // h + é(2 bytes)
    }

    #[test]
    fn insert_and_backspace_handle_unicode() {
        let mut e = Editor::from_text("");
        e.enter_insert();
        for c in "héllo".chars() {
            e.insert_char(c);
        }
        assert_eq!(e.text(), "héllo");
        e.backspace();
        assert_eq!(e.text(), "héll");
        e.col = 2;
        e.backspace();
        assert_eq!(e.text(), "hll");
    }

    #[test]
    fn newline_splits_at_cursor() {
        let mut e = Editor::from_text("select 1 from t");
        e.mode = Mode::Insert;
        e.col = 8;
        e.newline();
        assert_eq!(e.text(), "select 1\n from t");
        assert_eq!((e.line, e.col), (1, 0));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut e = Editor::from_text("ab\ncd");
        e.mode = Mode::Insert;
        e.line = 1;
        e.col = 0;
        e.backspace();
        assert_eq!(e.text(), "abcd");
        assert_eq!((e.line, e.col), (0, 2));
    }

    #[test]
    fn backspace_at_origin_is_a_noop_and_leaves_undo_clean() {
        let mut e = Editor::from_text("ab");
        e.mode = Mode::Insert;
        e.line = 0;
        e.col = 0;
        e.backspace();
        assert_eq!(e.text(), "ab");
        assert!(!e.undo());
    }

    #[test]
    fn delete_line_never_empties_the_buffer() {
        let mut e = Editor::from_text("only");
        e.delete_line();
        assert_eq!(e.lines, vec![String::new()]);
        assert_eq!(e.line, 0);
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut e = Editor::from_text("select 1");
        e.delete_line();
        assert_eq!(e.text(), "");
        assert!(e.undo());
        assert_eq!(e.text(), "select 1");
        assert!(!e.undo());
    }

    #[test]
    fn normal_mode_cursor_rests_on_a_character() {
        let mut e = Editor::from_text("abc");
        e.enter_normal();
        e.line_end();
        assert_eq!(e.col, 2);
        e.enter_insert();
        e.line_end();
        assert_eq!(e.col, 3);
    }

    #[test]
    fn word_motions_walk_tokens() {
        let mut e = Editor::from_text("select id from users");
        e.top();
        e.word_forward();
        assert_eq!(e.col, 7);
        e.word_forward();
        assert_eq!(e.col, 10);
        e.word_back();
        assert_eq!(e.col, 7);
        e.word_back();
        assert_eq!(e.col, 0);
    }

    #[test]
    fn open_line_enters_insert_mode() {
        let mut e = Editor::from_text("a");
        e.open_below();
        assert_eq!(e.mode, Mode::Insert);
        assert_eq!(e.text(), "a\n");
        assert_eq!(e.line, 1);
    }
}
