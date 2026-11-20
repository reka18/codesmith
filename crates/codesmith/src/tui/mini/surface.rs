//! Mini mode's terminal writer: the main screen (never the alternate
//! one), with finished transcript rows written into the terminal's own
//! scrollback and a live footer below them.
//!
//! The footer starts on the row the cursor was on (what the shell printed
//! before stays), and sits right under the last row written. A commit
//! clears the footer, prints its rows (the terminal scrolls them up into
//! its history once the screen is full) and leaves the footer to be drawn
//! again under them. A taller footer scrolls the screen up to make room;
//! a shorter one clears what it leaves below. Each frame is one write,
//! wrapped in synchronized output (`CSI ? 2026`) so terminals that support
//! it show it at once; others ignore the markers.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::io::Write;
use unicode_width::UnicodeWidthStr;

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

pub struct Surface<W: Write> {
    out: W,
    pub cols: u16,
    pub rows: u16,
    /// The footer's first row.
    top: u16,
    /// The footer as last drawn: unchanged rows are not drawn again.
    shown: Option<Buffer>,
    /// Monochrome ASCII: no colors, and common symbols as ASCII.
    pub mono: bool,
    /// This frame's bytes.
    frame: Vec<u8>,
}

impl<W: Write> Surface<W> {
    /// A surface on `out`, `cols` by `rows`, with the footer starting on
    /// row `row` (the cursor's when the UI opened).
    pub fn new(out: W, cols: u16, rows: u16, row: u16) -> Self {
        Surface {
            out,
            cols: cols.max(1),
            rows: rows.max(1),
            top: row.min(rows.saturating_sub(1)),
            shown: None,
            mono: false,
            frame: Vec::new(),
        }
    }

    pub fn top(&self) -> u16 {
        self.top
    }

    #[cfg(test)]
    pub fn out(&self) -> &W {
        &self.out
    }

    #[cfg(test)]
    pub fn out_mut(&mut self) -> &mut W {
        &mut self.out
    }

    fn goto(&mut self, x: u16, y: u16) {
        let _ = write!(self.frame, "\x1b[{};{}H", y + 1, x + 1);
    }

    /// Write `lines` (wrapped at the terminal's width) above the footer,
    /// into the scrollback. The footer must be drawn again after.
    pub fn commit(&mut self, lines: &[Line<'_>]) {
        if lines.is_empty() {
            return;
        }
        let w = self.cols;
        self.goto(0, self.top);
        self.frame.extend_from_slice(b"\x1b[J");
        let mut row = self.top;
        let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
        for line in lines {
            for part in fit(line, w as usize) {
                buf.reset();
                buf.set_line(0, 0, &part, w);
                row_bytes(&mut self.frame, &buf, 0, self.mono, true);
                self.frame.extend_from_slice(b"\r\n");
                row = (row + 1).min(self.rows - 1);
            }
        }
        self.top = row;
        self.shown = None;
    }

    /// Draw the footer (`buf`, as wide as the terminal) under the rows
    /// written, with the cursor at `cursor` inside it (hidden for `None`).
    pub fn footer(&mut self, buf: &Buffer, cursor: Option<(u16, u16)>) {
        let h = buf.area.height.min(self.rows);
        if self.top + h > self.rows {
            let up = self.top + h - self.rows;
            self.goto(0, self.rows - 1);
            for _ in 0..up {
                self.frame.push(b'\n');
            }
            self.top -= up;
            self.shown = None;
        }
        let same = self.shown.as_ref().is_some_and(|s| s.area == buf.area);
        self.frame.extend_from_slice(b"\x1b[?25l");
        for y in 0..h {
            if same {
                let prev = self.shown.as_ref().unwrap();
                let w = buf.area.width as usize;
                let start = y as usize * w;
                if prev.content[start..start + w] == buf.content[start..start + w] {
                    continue;
                }
            }
            self.goto(0, self.top + y);
            row_bytes(&mut self.frame, buf, y, self.mono, true);
            self.frame.extend_from_slice(b"\x1b[0m\x1b[K");
        }
        let grew_or_shrank = self
            .shown
            .as_ref()
            .is_none_or(|s| s.area.height != buf.area.height);
        if grew_or_shrank && self.top + h < self.rows {
            self.goto(0, self.top + h);
            self.frame.extend_from_slice(b"\x1b[J");
        }
        if let Some((x, y)) = cursor.filter(|(_, y)| *y < h) {
            self.goto(x.min(self.cols - 1), self.top + y);
            self.frame.extend_from_slice(b"\x1b[?25h");
        }
        self.shown = Some(buf.clone());
    }

    /// The terminal changed size: the footer is drawn whole next time. With
    /// `reset`, the screen and its scrollback are cleared to be written
    /// again at the new width; otherwise the footer moves to the bottom
    /// rows (where a terminal that reflowed its lines leaves the end of
    /// them), and what is under it is cleared.
    pub fn resize(&mut self, cols: u16, rows: u16, reset: bool) {
        let h = self.shown.as_ref().map_or(1, |s| s.area.height);
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.shown = None;
        if reset {
            self.frame.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3J");
            self.top = 0;
        } else {
            self.top = self.rows.saturating_sub(h);
            self.goto(0, self.top);
            self.frame.extend_from_slice(b"\x1b[J");
        }
    }

    /// Clear the screen, keeping what was written: the rows above the
    /// footer scroll up into the scrollback and the footer moves to the
    /// top.
    pub fn clear(&mut self) {
        if self.top == 0 {
            return;
        }
        self.goto(0, self.rows - 1);
        for _ in 0..self.top {
            self.frame.push(b'\n');
        }
        self.top = 0;
        self.shown = None;
    }

    /// The footer as it is cleared for good: the cursor waits where it
    /// started, under the last row written.
    pub fn close(&mut self) {
        self.goto(0, self.top);
        self.frame.extend_from_slice(b"\x1b[J\x1b[0m\x1b[?25h");
        self.shown = None;
    }

    /// The footer starts again on row `row` (after a suspend, say, when
    /// the shell has printed in the meantime).
    pub fn restart(&mut self, row: u16) {
        self.top = row.min(self.rows - 1);
        self.shown = None;
    }

    /// Send the frame, if anything changed.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        let mut all = Vec::with_capacity(self.frame.len() + 16);
        all.extend_from_slice(SYNC_BEGIN);
        all.append(&mut self.frame);
        all.extend_from_slice(SYNC_END);
        self.out.write_all(&all)?;
        self.out.flush()
    }
}

/// `line` hard-wrapped into rows of at most `width` columns.
pub fn fit(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    let owned = || Line {
        spans: line
            .spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect(),
        style: line.style,
        alignment: None,
    };
    if total <= width {
        return vec![owned()];
    }
    let mut out = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for s in &line.spans {
        let mut piece = String::new();
        for c in s.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + cw > width && used > 0 {
                if !piece.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut piece), s.style));
                }
                out.push(Line::from(std::mem::take(&mut cur)).style(line.style));
                used = 0;
            }
            piece.push(c);
            used += cw;
        }
        if !piece.is_empty() {
            cur.push(Span::styled(piece, s.style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur).style(line.style));
    }
    out
}

/// The ASCII stand-in for a symbol in monochrome mode.
pub fn mono_symbol(s: &str) -> &str {
    match s {
        "›" | "❯" | "▶" | "→" | "⟩" => ">",
        "‹" | "←" => "<",
        "·" | "•" | "◦" | "●" | "▪" | "■" | "✱" | "⚙" | "◆" | "○" | "◇" | "□" | "▫" => {
            "*"
        }
        "☐" => "[",
        "☑" | "☒" => "x",
        "…" => ".",
        "─" | "━" | "−" | "–" | "—" | "╌" => "-",
        "│" | "┃" | "▏" | "▎" | "▍" | "▌" | "▐" => "|",
        "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼" | "╭" | "╮" | "╰" | "╯" => {
            "+"
        }
        "✓" | "✔" => "v",
        "✗" | "✘" | "×" => "x",
        "↑" => "^",
        "↓" => "v",
        "“" | "”" => "\"",
        "‘" | "’" => "'",
        _ => s,
    }
}

fn sgr(out: &mut Vec<u8>, style: Style, mono: bool) {
    out.extend_from_slice(b"\x1b[0");
    let m = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, ";1"),
        (Modifier::DIM, ";2"),
        (Modifier::ITALIC, ";3"),
        (Modifier::UNDERLINED, ";4"),
        (Modifier::REVERSED, ";7"),
        (Modifier::CROSSED_OUT, ";9"),
    ] {
        if m.contains(flag) && !(mono && matches!(code, ";2" | ";3" | ";9")) {
            out.extend_from_slice(code.as_bytes());
        }
    }
    if !mono {
        if let Some(c) = style.fg {
            color(out, c, false);
        }
        if let Some(c) = style.bg {
            color(out, c, true);
        }
    }
    out.push(b'm');
}

fn color(out: &mut Vec<u8>, c: Color, bg: bool) {
    let base = if bg { 40 } else { 30 };
    let named = |n: u8| format!(";{}", base + n);
    let bright = |n: u8| format!(";{}", base + 60 + n);
    let s = match c {
        Color::Reset => format!(";{}", base + 9),
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => bright(0),
        Color::LightRed => bright(1),
        Color::LightGreen => bright(2),
        Color::LightYellow => bright(3),
        Color::LightBlue => bright(4),
        Color::LightMagenta => bright(5),
        Color::LightCyan => bright(6),
        Color::White => bright(7),
        Color::Indexed(i) => format!(";{};5;{i}", base + 8),
        Color::Rgb(r, g, b) => format!(";{};2;{r};{g};{b}", base + 8),
    };
    out.extend_from_slice(s.as_bytes());
}

fn style_of(c: &Cell) -> Style {
    Style::default().fg(c.fg).bg(c.bg).add_modifier(c.modifier)
}

/// Row `y` of `buf` as terminal bytes, from the cursor's column 0. With
/// `trim`, blank cells at its end are left out (so a copied row has no
/// trailing spaces).
fn row_bytes(out: &mut Vec<u8>, buf: &Buffer, y: u16, mono: bool, trim: bool) {
    let w = buf.area.width as usize;
    let cells = &buf.content[y as usize * w..(y as usize + 1) * w];
    let blank = |c: &Cell| {
        c.symbol() == " "
            && c.bg == Color::Reset
            && !c
                .modifier
                .intersects(Modifier::REVERSED | Modifier::UNDERLINED)
    };
    let end = if trim {
        cells.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1)
    } else {
        w
    };
    let mut current = Style::default().fg(Color::Reset).bg(Color::Reset);
    let mut styled = false;
    let mut skip = 0;
    for c in &cells[..end] {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let st = style_of(c);
        if st != current {
            if st == Style::default().fg(Color::Reset).bg(Color::Reset) {
                out.extend_from_slice(b"\x1b[0m");
            } else {
                sgr(out, st, mono);
            }
            current = st;
            styled = true;
        }
        let sym = c.symbol();
        let sym = if mono { mono_symbol(sym) } else { sym };
        out.extend_from_slice(sym.as_bytes());
        skip = sym.width().saturating_sub(1);
    }
    if styled {
        out.extend_from_slice(b"\x1b[0m");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, 500);
        p.process(bytes);
        p
    }

    fn footer(text: &str, w: u16, h: u16) -> Buffer {
        let mut b = Buffer::empty(Rect::new(0, 0, w, h));
        for (i, l) in text.lines().enumerate() {
            b.set_string(0, i as u16, l, Style::default());
        }
        b
    }

    fn rows_of(p: &vt100::Parser) -> Vec<String> {
        p.screen().rows(0, p.screen().size().1).collect()
    }

    #[test]
    fn commits_go_above_the_footer_and_into_the_scrollback() {
        let mut s = Surface::new(Vec::new(), 20, 6, 0);
        s.footer(&footer("> prompt", 20, 2), Some((2, 0)));
        s.flush().unwrap();
        let lines: Vec<Line> = (0..10).map(|i| Line::from(format!("row {i}"))).collect();
        s.commit(&lines);
        s.footer(&footer("> prompt", 20, 2), Some((2, 0)));
        s.flush().unwrap();
        let mut p = screen(s.out(), 6, 20);
        let rows = rows_of(&p);
        assert_eq!(rows[..4], ["row 6", "row 7", "row 8", "row 9"]);
        assert_eq!(rows[4], "> prompt");
        assert_eq!(p.screen().cursor_position(), (4, 2));
        // The earlier rows are in the terminal's own scrollback.
        p.set_scrollback(6);
        assert_eq!(rows_of(&p)[0], "row 0");
    }

    #[test]
    fn the_footer_starts_where_the_cursor_was_and_grows_upward_only_at_the_bottom() {
        // What the shell printed before stays.
        let mut s = Surface::new(b"$ codesmith mini\r\n".to_vec(), 20, 6, 1);
        s.footer(&footer("a\nb", 20, 2), None);
        s.flush().unwrap();
        let p = screen(s.out(), 6, 20);
        assert_eq!(rows_of(&p)[..3], ["$ codesmith mini", "a", "b"]);
        // Taller than the room below: the screen scrolls, the shell's line too.
        s.footer(&footer("1\n2\n3\n4\n5\n6", 20, 6), None);
        s.flush().unwrap();
        let p = screen(s.out(), 6, 20);
        assert_eq!(rows_of(&p), ["1", "2", "3", "4", "5", "6"]);
        assert_eq!(s.top(), 0);
        // Shorter again: what it leaves below is cleared.
        s.footer(&footer("x", 20, 1), None);
        s.flush().unwrap();
        let p = screen(s.out(), 6, 20);
        assert_eq!(rows_of(&p)[..2], ["x", ""]);
    }

    #[test]
    fn rows_wider_than_the_terminal_wrap_and_styles_survive() {
        let mut s = Surface::new(Vec::new(), 10, 5, 0);
        s.commit(&[Line::from(vec![
            Span::styled("abcdef", Style::default().fg(Color::Rgb(1, 2, 3))),
            Span::raw("ghijklmn"),
        ])]);
        s.footer(&footer("", 10, 1), None);
        s.flush().unwrap();
        let p = screen(s.out(), 5, 10);
        assert_eq!(rows_of(&p)[..2], ["abcdefghij", "klmn"]);
        assert_eq!(
            p.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            p.screen().cell(1, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
    }

    #[test]
    fn frames_are_synchronized_and_unchanged_footer_rows_are_not_redrawn() {
        let mut s = Surface::new(Vec::new(), 20, 5, 0);
        s.footer(&footer("one\ntwo", 20, 2), None);
        s.flush().unwrap();
        let first = s.out().len();
        assert!(s.out().starts_with(SYNC_BEGIN) && s.out().ends_with(SYNC_END));
        s.footer(&footer("one\nTWO", 20, 2), None);
        s.flush().unwrap();
        let second = String::from_utf8_lossy(&s.out()[first..]).to_string();
        assert!(
            second.contains("TWO") && !second.contains("one"),
            "{second:?}"
        );
        // Nothing changed: nothing is sent but the cursor.
        let before = s.out().len();
        s.footer(&footer("one\nTWO", 20, 2), None);
        s.flush().unwrap();
        assert!(!String::from_utf8_lossy(&s.out()[before..]).contains("one"));
    }

    #[test]
    fn mono_drops_colors_and_symbols() {
        let mut s = Surface::new(Vec::new(), 20, 5, 0);
        s.mono = true;
        s.commit(&[Line::from(Span::styled(
            "› done · ✓",
            Style::default().fg(Color::Red),
        ))]);
        s.flush().unwrap();
        let p = screen(s.out(), 5, 20);
        assert_eq!(rows_of(&p)[0], "> done * v");
        assert_eq!(
            p.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
    }

    #[test]
    fn clearing_keeps_the_rows_in_the_scrollback() {
        let mut s = Surface::new(Vec::new(), 20, 6, 0);
        s.commit(&[Line::from("kept"), Line::from("also kept")]);
        s.footer(&footer("> ", 20, 1), None);
        s.clear();
        s.footer(&footer("> ", 20, 1), None);
        s.flush().unwrap();
        let mut p = screen(s.out(), 6, 20);
        assert_eq!(rows_of(&p)[0], ">");
        p.set_scrollback(2);
        assert_eq!(rows_of(&p)[..2], ["kept", "also kept"]);
    }

    #[test]
    fn wide_characters_take_two_columns() {
        let mut s = Surface::new(Vec::new(), 10, 3, 0);
        s.commit(&[Line::from("界e🙂x")]);
        s.flush().unwrap();
        let p = screen(s.out(), 3, 10);
        assert_eq!(rows_of(&p)[0], "界e🙂x");
    }
}
