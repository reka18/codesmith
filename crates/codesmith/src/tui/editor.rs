//! The prompt editor: a multi-line text buffer with a cursor. Pure, so the
//! key handling in `app.rs` and the cursor placement in `view.rs` can be
//! tested without a terminal.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Editor {
    text: String,
    /// Byte offset into `text`, always on a char boundary.
    cursor: usize,
    /// Display column kept while moving up and down through shorter lines.
    goal: Option<usize>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replace the whole text, cursor at the end.
    pub fn set(&mut self, s: impl Into<String>) {
        self.text = s.into();
        self.cursor = self.text.len();
        self.goal = None;
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.goal = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.goal = None;
    }

    /// Replace the byte range `start..self.cursor` with `s` (completions).
    pub fn replace_before_cursor(&mut self, start: usize, s: &str) {
        let start = start.min(self.cursor);
        self.text.replace_range(start..self.cursor, s);
        self.cursor = start + s.len();
        self.goal = None;
    }

    fn prev_boundary(&self, i: usize) -> usize {
        self.text[..i]
            .char_indices()
            .next_back()
            .map(|(j, _)| j)
            .unwrap_or(0)
    }

    fn next_boundary(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map(|c| i + c.len_utf8())
            .unwrap_or(i)
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev_boundary(self.cursor);
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
        self.goal = None;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next_boundary(self.cursor);
            self.text.replace_range(self.cursor..n, "");
        }
        self.goal = None;
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
        self.goal = None;
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
        self.goal = None;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
        self.goal = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
        self.goal = None;
    }

    /// Start of the previous word (skipping separators first).
    fn word_left_pos(&self) -> usize {
        let before = &self.text[..self.cursor];
        let mut it = before.char_indices().rev().peekable();
        while it.peek().is_some_and(|(_, c)| !is_word(*c)) {
            it.next();
        }
        let mut pos = it.peek().map(|(i, _)| *i).unwrap_or(0);
        for (i, c) in it {
            if !is_word(c) {
                break;
            }
            pos = i;
        }
        pos
    }

    /// End of the next word (skipping separators first).
    fn word_right_pos(&self) -> usize {
        let mut pos = self.cursor;
        let mut it = self.text[self.cursor..].chars().peekable();
        while let Some(c) = it.next_if(|c| !is_word(*c)) {
            pos += c.len_utf8();
        }
        while let Some(c) = it.next_if(|c| is_word(*c)) {
            pos += c.len_utf8();
        }
        pos
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
        self.goal = None;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_pos();
        self.goal = None;
    }

    /// Delete back to the start of the previous word (Ctrl+W, Alt+Backspace).
    pub fn delete_word_back(&mut self) {
        let p = self.word_left_pos();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
        self.goal = None;
    }

    /// Delete to the end of the next word (Alt+D).
    pub fn delete_word_forward(&mut self) {
        let p = self.word_right_pos();
        self.text.replace_range(self.cursor..p, "");
        self.goal = None;
    }

    /// Delete to the end of the line, or the newline itself when already
    /// there (Ctrl+K).
    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        if e == self.cursor {
            self.delete();
        } else {
            self.text.replace_range(self.cursor..e, "");
        }
        self.goal = None;
    }

    /// Delete back to the start of the line, or the newline before it when
    /// already there (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        if s == self.cursor {
            self.backspace();
        } else {
            self.text.replace_range(s..self.cursor, "");
            self.cursor = s;
        }
        self.goal = None;
    }

    /// Delete the whole line the cursor is on, with its newline.
    pub fn delete_line(&mut self) {
        let (mut s, mut e) = (self.line_start(), self.line_end());
        if e < self.text.len() {
            e += 1;
        } else {
            s = s.saturating_sub(1);
        }
        self.text.replace_range(s..e, "");
        self.cursor = s.min(self.text.len());
        self.goal = None;
    }

    /// Start of the whole text.
    pub fn buffer_home(&mut self) {
        self.cursor = 0;
        self.goal = None;
    }

    /// End of the whole text.
    pub fn buffer_end(&mut self) {
        self.cursor = self.text.len();
        self.goal = None;
    }

    fn col_of(&self, start: usize, end: usize) -> usize {
        self.text[start..end]
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum()
    }

    /// Byte offset in the line starting at `start` closest to display column `col`.
    fn at_col(&self, start: usize, col: usize) -> usize {
        let mut w = 0;
        for (i, c) in self.text[start..].char_indices() {
            if c == '\n' || w + c.width().unwrap_or(0) > col {
                return start + i;
            }
            w += c.width().unwrap_or(0);
        }
        self.text.len()
    }

    /// Move up a line. Returns false when already on the first line.
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.goal.unwrap_or_else(|| self.col_of(start, self.cursor));
        let prev_start = self.text[..start - 1]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = self.at_col(prev_start, col);
        self.goal = Some(col);
        true
    }

    /// Move down a line. Returns false when already on the last line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let col = self
            .goal
            .unwrap_or_else(|| self.col_of(self.line_start(), self.cursor));
        self.cursor = self.at_col(end + 1, col);
        self.goal = Some(col);
        true
    }
}

/// Hard-wrap text into rows of at most `width` display columns (the input
/// box wraps by character so the cursor maps exactly). Returns each row's
/// byte range.
pub fn rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    let mut w = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            out.push((start, i));
            start = i + 1;
            w = 0;
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw > width && i > start {
            out.push((start, i));
            start = i;
            w = 0;
        }
        w += cw;
    }
    out.push((start, text.len()));
    out
}

/// The (row, column) of byte offset `cursor` within `rows(text, width)`.
pub fn cursor_pos(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let rs = rows(text, width);
    let mut row = 0;
    for (i, (s, e)) in rs.iter().enumerate() {
        // The cursor belongs to the last row it falls in (a wrapped row's
        // end equals the next row's start).
        if cursor >= *s && cursor <= *e {
            row = i;
            if cursor < *e {
                break;
            }
        }
    }
    let (s, _) = rs[row];
    let col = text[s..cursor]
        .chars()
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>();
    // At the very end of a full row, show the cursor at the start of the next.
    if col >= width.max(1) {
        return (row + 1, 0);
    }
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(s: &str) -> Editor {
        let mut e = Editor::default();
        e.set(s);
        e
    }

    #[test]
    fn insert_and_delete_at_the_cursor() {
        let mut e = ed("hello");
        e.left();
        e.left();
        e.insert('X');
        assert_eq!(e.text(), "helXlo");
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "helo");
        e.delete();
        assert_eq!(e.text(), "heo");
        e.home();
        e.insert_str(">> ");
        assert_eq!(e.text(), ">> heo");
        e.end();
        e.insert('!');
        assert_eq!(e.text(), ">> heo!");
    }

    #[test]
    fn unicode_is_safe() {
        let mut e = ed("héllo→");
        e.left();
        e.backspace();
        assert_eq!(e.text(), "héll→");
        e.home();
        e.right();
        e.right();
        e.delete();
        assert_eq!(e.text(), "hél→");
    }

    #[test]
    fn word_movement_and_deletion() {
        let mut e = ed("foo bar_baz  qux");
        e.word_left();
        assert_eq!(e.cursor(), 13);
        e.word_left();
        assert_eq!(e.cursor(), 4);
        e.word_left();
        assert_eq!(e.cursor(), 0);
        e.word_right();
        assert_eq!(e.cursor(), 3);
        e.word_right();
        assert_eq!(e.cursor(), 11);
        e.end();
        e.delete_word_back();
        assert_eq!(e.text(), "foo bar_baz  ");
        e.delete_word_back();
        assert_eq!(e.text(), "foo ");
        e.home();
        e.delete_word_forward();
        assert_eq!(e.text(), " ");
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut e = ed("abcdef\nxy\nlonger line");
        assert!(!e.down(), "already on the last line");
        assert!(e.up());
        assert_eq!(e.cursor(), 9, "clamped to the end of 'xy'");
        assert!(e.up());
        assert_eq!(
            e.cursor(),
            6,
            "goal column 11 clamps to the end of 'abcdef'"
        );
        assert!(!e.up());
        e.home();
        e.right();
        assert!(e.down());
        assert_eq!(e.cursor(), 8);
        assert!(e.down());
        assert_eq!(e.cursor(), 11);
        assert!(!e.down());
    }

    #[test]
    fn kill_line() {
        let mut e = ed("one\ntwo");
        e.up();
        e.home();
        e.kill_to_end();
        assert_eq!(e.text(), "\ntwo");
        e.kill_to_end();
        assert_eq!(e.text(), "two");
    }

    #[test]
    fn completion_replacement() {
        let mut e = ed("look at @sr please");
        e.home();
        for _ in 0..11 {
            e.right();
        }
        e.replace_before_cursor(8, "@src/main.rs");
        assert_eq!(e.text(), "look at @src/main.rs please");
        assert_eq!(e.cursor(), 20);
    }

    #[test]
    fn rows_and_cursor_positions() {
        assert_eq!(rows("", 5), vec![(0, 0)]);
        assert_eq!(rows("abcdefg", 3), vec![(0, 3), (3, 6), (6, 7)]);
        assert_eq!(rows("ab\ncd", 10), vec![(0, 2), (3, 5)]);
        assert_eq!(cursor_pos("abcdefg", 7, 3), (2, 1));
        assert_eq!(cursor_pos("abcdef", 6, 3), (2, 0), "full row: next row");
        assert_eq!(cursor_pos("abcdef", 3, 3), (1, 0));
        assert_eq!(cursor_pos("ab\ncd", 2, 10), (0, 2));
        assert_eq!(cursor_pos("ab\ncd", 3, 10), (1, 0));
        assert_eq!(cursor_pos("ab\n", 3, 10), (1, 0));
    }
}
