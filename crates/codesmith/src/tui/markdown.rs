//! Markdown to styled terminal lines: headings, emphasis, inline code,
//! links, lists (with task boxes), quotes, rules, tables and fenced code
//! blocks highlighted with syntect.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        set.themes.remove("base16-ocean.dark").unwrap_or_default()
    })
}

/// Syntax-highlight `code` as `lang` (a fence tag or file extension), one
/// span list per line. Unknown languages come back unstyled.
pub fn highlight(code: &str, lang: &str) -> Vec<Vec<Span<'static>>> {
    let ss = syntaxes();
    let token = lang.split([',', ' ', '{']).next().unwrap_or("").trim();
    let syntax = (!token.is_empty())
        .then(|| {
            ss.find_syntax_by_token(token)
                .or_else(|| ss.find_syntax_by_extension(token))
        })
        .flatten();
    let Some(syntax) = syntax else {
        return code
            .lines()
            .map(|l| vec![Span::raw(l.to_string())])
            .collect();
    };
    let mut h = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(code) {
        let spans = match h.highlight_line(line, ss) {
            Ok(regions) => regions
                .into_iter()
                .map(|(st, t)| {
                    let c = st.foreground;
                    let mut style = Style::default().fg(Color::Rgb(c.r, c.g, c.b));
                    let fs = st.font_style;
                    if fs.contains(syntect::highlighting::FontStyle::BOLD) {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if fs.contains(syntect::highlighting::FontStyle::ITALIC) {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    Span::styled(t.trim_end_matches(['\n', '\r']).to_string(), style)
                })
                .filter(|s| !s.content.is_empty())
                .collect(),
            Err(_) => vec![Span::raw(line.trim_end_matches(['\n', '\r']).to_string())],
        };
        out.push(spans);
    }
    out
}

/// Greedy word wrap of styled spans. `first` prefixes the first row and
/// `rest` the following ones.
pub fn wrap_spans(
    spans: &[Span<'static>],
    first: &[Span<'static>],
    rest: &[Span<'static>],
    width: usize,
) -> Vec<Line<'static>> {
    let pw = |p: &[Span]| p.iter().map(|s| s.content.width()).sum::<usize>();
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = first.to_vec();
    let mut room = width.saturating_sub(pw(first)).max(1);
    let mut used = 0usize;
    let mut fresh = true;
    let new_row = |lines: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>| {
        lines.push(Line::from(std::mem::take(cur)));
        *cur = rest.to_vec();
    };
    // Tokens: runs of whitespace or of other characters, keeping the style.
    let mut tokens: Vec<(String, Style, bool)> = Vec::new();
    for s in spans {
        let mut buf = String::new();
        let mut ws = None;
        for c in s.content.chars() {
            let w = c.is_whitespace();
            if ws.is_some_and(|x| x != w) {
                tokens.push((std::mem::take(&mut buf), s.style, ws.unwrap()));
            }
            ws = Some(w);
            buf.push(c);
        }
        if !buf.is_empty() {
            tokens.push((buf, s.style, ws.unwrap_or(false)));
        }
    }
    for (tok, style, ws) in tokens {
        let tw = tok.width();
        if ws {
            if fresh || used + tw > room {
                continue; // no leading spaces on a row; spaces at a break vanish
            }
            cur.push(Span::styled(tok, style));
            used += tw;
            continue;
        }
        if used + tw > room && !fresh {
            // Drop trailing spaces before breaking.
            while cur.last().is_some_and(|s| s.content.trim().is_empty()) && cur.len() > 1 {
                cur.pop();
            }
            new_row(&mut lines, &mut cur);
            room = width.saturating_sub(pw(rest)).max(1);
            used = 0;
        }
        if tw <= room {
            cur.push(Span::styled(tok, style));
            used += tw;
        } else {
            // A word longer than a row: break it by character.
            let mut piece = String::new();
            for c in tok.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > room && used > 0 {
                    cur.push(Span::styled(std::mem::take(&mut piece), style));
                    new_row(&mut lines, &mut cur);
                    room = width.saturating_sub(pw(rest)).max(1);
                    used = 0;
                }
                piece.push(c);
                used += cw;
            }
            if !piece.is_empty() {
                cur.push(Span::styled(piece, style));
            }
        }
        fresh = false;
    }
    lines.push(Line::from(cur));
    lines
}

struct Item {
    /// Bullet text for the first row ("• ", "3. ").
    bullet: Option<String>,
    /// Columns the item's text is indented by.
    width: usize,
}

#[derive(Default)]
struct Table {
    align: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    cell: String,
    head_rows: usize,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    items: Vec<Item>,
    quote: usize,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<String>,
    heading: Option<HeadingLevel>,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let dim = Style::default().fg(Color::DarkGray);
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for _ in 0..self.quote {
            first.push(Span::styled("│ ", dim));
            rest.push(Span::styled("│ ", dim));
        }
        let n = self.items.len();
        for (i, it) in self.items.iter_mut().enumerate() {
            if i + 1 == n {
                if let Some(b) = it.bullet.take() {
                    first.push(Span::styled(b, Style::default().fg(Color::Cyan)));
                    rest.push(Span::raw(" ".repeat(it.width)));
                    continue;
                }
            }
            first.push(Span::raw(" ".repeat(it.width)));
            rest.push(Span::raw(" ".repeat(it.width)));
        }
        (first, rest)
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let (first, rest) = self.prefixes();
        self.out
            .extend(wrap_spans(&spans, &first, &rest, self.width));
    }

    fn blank(&mut self) {
        if self
            .out
            .last()
            .is_some_and(|l| l.width() > 0 || !l.spans.is_empty())
        {
            self.out.push(Line::default());
        }
    }

    fn text(&mut self, t: &str) {
        if let Some(tb) = &mut self.table {
            tb.cell.push_str(t);
            return;
        }
        if let Some((_, code)) = &mut self.code {
            code.push_str(t);
            return;
        }
        let s = self.style();
        self.spans.push(Span::styled(t.to_string(), s));
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        let bar = Style::default().fg(Color::DarkGray);
        let (mut first, _) = self.prefixes();
        if !lang.is_empty() {
            let mut l = first.clone();
            l.push(Span::styled(format!("┌ {lang}"), bar));
            self.out.push(Line::from(l));
        }
        first.push(Span::styled("│ ", bar));
        for spans in highlight(code.trim_end_matches('\n'), lang) {
            let mut l = first.clone();
            l.extend(spans);
            self.out.push(Line::from(l));
        }
    }

    fn table(&mut self, t: Table) {
        let cols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut w = vec![0usize; cols];
        for r in &t.rows {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(c.width());
            }
        }
        let sep = Style::default().fg(Color::DarkGray);
        for (ri, r) in t.rows.iter().enumerate() {
            let mut spans = Vec::new();
            for (i, width) in w.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", sep));
                }
                let c = r.get(i).map(String::as_str).unwrap_or("");
                let pad = width.saturating_sub(c.width());
                let (l, rt) = match t.align.get(i) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                let style = if ri < t.head_rows {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(
                    format!("{}{c}{}", " ".repeat(l), " ".repeat(rt)),
                    style,
                ));
            }
            self.out.push(Line::from(spans));
            if ri + 1 == t.head_rows {
                let rule: Vec<String> = w.iter().map(|n| "─".repeat(*n)).collect();
                self.out
                    .push(Line::from(Span::styled(rule.join("─┼─"), sep)));
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.items.is_empty() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.blank();
                self.heading = Some(level);
                let color = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => Color::Cyan,
                    _ => Color::Reset,
                };
                self.push_style(|s| s.fg(color).add_modifier(Modifier::BOLD));
                if level == HeadingLevel::H1 {
                    self.push_style(|s| s.add_modifier(Modifier::UNDERLINED));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.blank();
                self.quote += 1;
                self.push_style(|s| s.add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                if self.items.is_empty() {
                    self.blank();
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let b = format!("{n}. ");
                        *n += 1;
                        b
                    }
                    _ => ["• ", "◦ ", "▪ "][depth % 3].to_string(),
                };
                self.items.push(Item {
                    width: bullet.width(),
                    bullet: Some(bullet),
                });
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.push_style(|s| s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.text("[image: ");
            }
            Tag::Table(align) => {
                self.flush();
                self.blank();
                self.table = Some(Table {
                    align,
                    ..Default::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(level) => {
                if let Some(l) = self.heading.take() {
                    if matches!(
                        l,
                        HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6
                    ) {
                        let marks = "#".repeat(l as usize);
                        self.spans.insert(
                            0,
                            Span::styled(format!("{marks} "), Style::default().fg(Color::DarkGray)),
                        );
                    }
                }
                self.flush();
                self.styles.pop();
                if level == HeadingLevel::H1 {
                    self.styles.pop();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.code_block(&lang, &code);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                let url = self.links.pop().unwrap_or_default();
                let shown: String = self
                    .spans
                    .iter()
                    .rev()
                    .take(1)
                    .map(|s| s.content.to_string())
                    .collect();
                if !url.is_empty() && shown != url && !url.starts_with('#') {
                    self.spans.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            TagEnd::Image => {
                let url = self.links.pop().unwrap_or_default();
                self.text(&format!("]({url})"));
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let c = std::mem::take(&mut t.cell);
                    if let Some(r) = t.rows.last_mut() {
                        r.push(c.trim().to_string());
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head_rows = t.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table(t);
                }
            }
            _ => {}
        }
    }
}

/// Render markdown to lines at most `width` columns wide (code blocks are
/// not wrapped).
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = Renderer {
        width: width.max(10),
        out: Vec::new(),
        spans: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        items: Vec::new(),
        quote: 0,
        code: None,
        table: None,
        links: Vec::new(),
        heading: None,
    };
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(text, opts) {
        match ev {
            Event::Start(t) => r.start(t),
            Event::End(t) => r.end(t),
            Event::Text(t) => r.text(&t),
            Event::Code(t) => {
                if let Some(tb) = &mut r.table {
                    tb.cell.push_str(&t);
                } else {
                    r.spans.push(Span::styled(
                        t.to_string(),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
            Event::SoftBreak => r.text(" "),
            Event::HardBreak => r.flush(),
            Event::Rule => {
                r.flush();
                r.blank();
                r.out.push(Line::from(Span::styled(
                    "─".repeat(r.width.min(60)),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Event::TaskListMarker(done) => {
                let s = if done { "☑ " } else { "☐ " };
                r.spans
                    .push(Span::styled(s, Style::default().fg(Color::Cyan)));
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                let dim = Style::default().fg(Color::DarkGray);
                for (i, l) in h.lines().enumerate() {
                    if i > 0 {
                        r.flush();
                    }
                    r.spans.push(Span::styled(l.to_string(), dim));
                }
            }
            Event::FootnoteReference(f) => r.text(&format!("[{f}]")),
            _ => {}
        }
    }
    r.flush();
    while r.out.first().is_some_and(|l| l.spans.is_empty()) {
        r.out.remove(0);
    }
    while r.out.last().is_some_and(|l| l.spans.is_empty()) {
        r.out.pop();
    }
    r.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn style_of(lines: &[Line], text: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(text))
            .unwrap_or_else(|| panic!("no span with {text:?}"))
            .style
    }

    #[test]
    fn headings_emphasis_and_code() {
        let md = "# Title\n\nSome **bold**, *italic* and `code`.\n\n### Small";
        let l = render(md, 60);
        assert_eq!(
            plain(&l),
            vec!["Title", "", "Some bold, italic and code.", "", "### Small"]
        );
        assert!(style_of(&l, "Title").add_modifier.contains(Modifier::BOLD));
        assert!(style_of(&l, "bold").add_modifier.contains(Modifier::BOLD));
        assert!(style_of(&l, "italic")
            .add_modifier
            .contains(Modifier::ITALIC));
        assert_eq!(style_of(&l, "code").fg, Some(Color::Yellow));
    }

    #[test]
    fn lists_wrap_under_their_bullets() {
        let md = "- one two three four five six\n- [x] done\n\n1. first\n2. second\n   - nested";
        let l = plain(&render(md, 16));
        assert_eq!(
            l,
            vec![
                "• one two three",
                "  four five six",
                "• ☑ done",
                "",
                "1. first",
                "2. second",
                "   ◦ nested",
            ]
        );
    }

    #[test]
    fn links_show_their_target() {
        let l = plain(&render("see [docs](https://x.dev) or <https://y.dev>", 80));
        assert_eq!(l, vec!["see docs (https://x.dev) or https://y.dev"]);
    }

    #[test]
    fn fenced_code_is_highlighted_and_not_wrapped() {
        let md = "```rust\nfn main() { let a_very_long_identifier_here = 1; }\n```";
        let l = render(md, 20);
        let p = plain(&l);
        assert_eq!(p[0], "┌ rust");
        assert_eq!(p[1], "│ fn main() { let a_very_long_identifier_here = 1; }");
        let fn_style = style_of(&l, "fn");
        assert!(
            matches!(fn_style.fg, Some(Color::Rgb(..))),
            "keywords are colored: {fn_style:?}"
        );
        let plain_block = render("```\nx\n```", 20);
        assert_eq!(plain(&plain_block), vec!["│ x"]);
    }

    #[test]
    fn tables_and_quotes() {
        let md = "| a | bb |\n|---|---:|\n| ccc | d |\n\n> quoted";
        let p = plain(&render(md, 40));
        assert_eq!(p, vec!["a   │ bb", "────┼───", "ccc │  d", "", "│ quoted"]);
    }

    #[test]
    fn wrapping_breaks_long_words() {
        let l = wrap_spans(&[Span::raw("abcdefghij kl")], &[], &[], 4);
        assert_eq!(plain(&l), vec!["abcd", "efgh", "ij", "kl"]);
    }

    #[test]
    fn unfinished_streams_render() {
        let p = plain(&render("Here:\n```py\nprint(1)", 40));
        assert_eq!(p, vec!["Here:", "", "┌ py", "│ print(1)"]);
    }
}
