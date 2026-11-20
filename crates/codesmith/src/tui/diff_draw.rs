//! Drawing the diff viewer: the source header, the file tree, the patch
//! cards (only the rows in view) and the footer, in the theme's diff and
//! surface colors. Patch lines are syntax-colored a file side at a time,
//! carrying the highlighter's state from line to line so a construct that
//! spans hunks or many rows (a block comment) keeps its color; the work is
//! done as far down as has been shown, and kept.

use super::diff_source::{LineKind, Status};
use super::diff_tree::{self, NodeKind};
use super::diff_view::{Geom, Item, Layout, Load, Memory, Viewer, HEADER_HEIGHT};
use super::keymap::Keymap;
use super::theme::{self, Palette};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::collections::HashMap;
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter};
use syntect::parsing::{ParseState, ScopeStack};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Lines longer than this are not parsed for colors.
const HIGHLIGHT_LINE_MAX: usize = 4000;

/// One line's colors: (end byte, style) runs.
type Runs = Vec<(u32, Style)>;

struct Side {
    lines: Vec<Runs>,
    /// Where the next line to color is: (hunk, line).
    cursor: (usize, usize),
    state: Option<(ParseState, HighlightState)>,
}

/// Syntax colors worked out so far, per file side, for one palette.
#[derive(Default)]
pub struct Highlights {
    epoch: u64,
    sides: HashMap<(usize, bool), Side>,
}

fn syntax_for(file: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    let ss = super::markdown::syntaxes();
    let name = file.rsplit('/').next().unwrap_or(file);
    match name.rsplit_once('.') {
        // syntect's own set has no TypeScript: JavaScript is close.
        Some((_, "ts" | "mts" | "cts" | "tsx")) => ss.find_syntax_by_extension("js"),
        Some((_, ext)) => ss.find_syntax_by_extension(ext),
        None => ss.find_syntax_by_extension(name),
    }
    .or_else(|| ss.find_syntax_by_token(name))
}

impl Highlights {
    /// The colors of side `new` of `file` up to its line `index`.
    fn runs(
        &mut self,
        v: &Viewer,
        p: &Palette,
        file: usize,
        new: bool,
        index: usize,
    ) -> Option<Runs> {
        if self.epoch != p.epoch {
            self.sides.clear();
            self.epoch = p.epoch;
        }
        if !v.highlights(file) {
            return None;
        }
        let syntax = syntax_for(&v.files[file].file)?;
        let side = self.sides.entry((file, new)).or_insert_with(|| Side {
            lines: vec![],
            cursor: (0, 0),
            state: Some((
                ParseState::new(syntax),
                HighlightState::new(&Highlighter::new(p.syntect()), ScopeStack::new()),
            )),
        });
        if side.lines.len() <= index {
            let ss = super::markdown::syntaxes();
            let highlighter = Highlighter::new(p.syntect());
            let hunks = &v.files[file].hunks;
            let (mut h, mut l) = side.cursor;
            let mut line = String::new();
            while side.lines.len() <= index && h < hunks.len() {
                if l >= hunks[h].lines.len() {
                    h += 1;
                    l = 0;
                    continue;
                }
                let pl = &hunks[h].lines[l];
                l += 1;
                let on_side = match pl.kind {
                    LineKind::Context => true,
                    LineKind::Add => new,
                    LineKind::Del => !new,
                };
                if !on_side {
                    continue;
                }
                let Some((parse, state)) = side.state.as_mut() else {
                    side.lines.push(vec![]);
                    continue;
                };
                if pl.text.len() > HIGHLIGHT_LINE_MAX {
                    side.lines.push(vec![]);
                    continue;
                }
                line.clear();
                line.push_str(&pl.text);
                line.push('\n');
                let runs = match parse.parse_line(&line, ss) {
                    Ok(ops) => {
                        let mut end = 0u32;
                        HighlightIterator::new(state, &ops, &line, &highlighter)
                            .map(|(st, text)| {
                                end += text.len() as u32;
                                let c = st.foreground;
                                let mut style = Style::default().fg(Color::Rgb(c.r, c.g, c.b));
                                if st
                                    .font_style
                                    .contains(syntect::highlighting::FontStyle::BOLD)
                                {
                                    style = style.add_modifier(Modifier::BOLD);
                                }
                                if st
                                    .font_style
                                    .contains(syntect::highlighting::FontStyle::ITALIC)
                                {
                                    style = style.add_modifier(Modifier::ITALIC);
                                }
                                (end, style)
                            })
                            .collect()
                    }
                    Err(_) => {
                        side.state = None;
                        vec![]
                    }
                };
                side.lines.push(runs);
            }
            side.cursor = (h, l);
        }
        side.lines.get(index).cloned()
    }
}

fn fill(buf: &mut Buffer, x: u16, y: u16, w: u16, style: Style) {
    let area = buf.area;
    if y >= area.bottom() {
        return;
    }
    for cx in x..(x + w).min(area.right()) {
        buf[(cx, y)].reset();
        buf[(cx, y)].set_style(style);
    }
}

fn glyphs(buf: &mut Buffer, x: u16, y: u16, w: u16, glyph: &str, style: Style) {
    let area = buf.area;
    if y >= area.bottom() {
        return;
    }
    for cx in x..(x + w).min(area.right()) {
        buf[(cx, y)].set_symbol(glyph).set_style(style);
    }
}

/// Write `s` at (`x`, `y`) in at most `max` columns; the columns used.
fn put(buf: &mut Buffer, x: u16, y: u16, max: usize, s: &str, style: Style) -> usize {
    if y >= buf.area.bottom() || x >= buf.area.right() || max == 0 {
        return 0;
    }
    let (end, _) = buf.set_stringn(x, y, s, max, style);
    (end - x) as usize
}

/// `s` cut to `max` columns.
fn cut(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// A path cut from the left to `max` columns, keeping its file name: the
/// folder part and the name.
fn path_parts(path: &str, max: usize) -> (String, String) {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n.to_string()),
        None => (String::new(), path.to_string()),
    };
    if dir.width() + name.width() <= max {
        return (dir, name);
    }
    if name.width() + 1 >= max {
        return (String::new(), cut(&name, max));
    }
    let room = max - name.width() - 1;
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in dir.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > room {
            break;
        }
        w += cw;
        tail.push(c);
    }
    tail.reverse();
    (format!("…{}", tail.into_iter().collect::<String>()), name)
}

/// A folder chain cut to `max` columns by whole leading segments
/// (`…/c/d`), as OpenCode's `truncateDirectoryChain`.
pub fn truncate_chain(name: &str, max: usize) -> String {
    if name.width() <= max {
        return name.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut w = "…/".width();
    for seg in name.split('/').rev() {
        let next = seg.width() + usize::from(!kept.is_empty());
        if w + next > max {
            break;
        }
        kept.insert(0, seg);
        w += next;
    }
    if kept.is_empty() {
        let (d, n) = path_parts(name, max);
        return format!("{d}{n}");
    }
    format!("…/{}", kept.join("/"))
}

/// The card's background: raised once reviewed.
fn card_bg(v: &Viewer, p: &Palette, file: usize) -> Color {
    if v.is_reviewed(file) {
        p.raised_high
    } else {
        p.diff_context_bg
    }
}

/// The help key's shortcut, if bound.
fn help_key(km: &Keymap) -> Option<String> {
    km.list("diff.help").into_iter().next()
}

/// Draw the viewer on a `area`-sized terminal.
pub fn draw(buf: &mut Buffer, area: Rect, v: &Viewer, mem: &Memory, km: &Keymap) {
    v.term.set((area.width, area.height));
    let g = v.geom(area.width, area.height);
    let p = theme::current();
    let surface = if g.docked { p.raised } else { p.bg };
    let page = Style::default().bg(p.bg);
    for y in 0..g.height {
        fill(buf, g.x, y, g.w, Style::default().fg(p.text).bg(surface));
    }
    if !g.tree {
        let y = u16::from(g.docked);
        let right = if g.compact { 1 } else { 2 };
        header_row(
            buf,
            &p,
            v,
            g.x + 2,
            y,
            g.w.saturating_sub(2 + right),
            surface,
            false,
        );
        if !g.compact {
            if let Some(h) = help_key(km) {
                put(
                    buf,
                    g.x + g.w - 1,
                    0,
                    1,
                    if h.width() == 1 { &h } else { "?" },
                    Style::default().fg(p.text).bg(surface),
                );
            }
        }
    }
    let note_y = if g.compact { g.view_y } else { g.view_y + 1 };
    let note = |buf: &mut Buffer, text: &str, color: Color| {
        put(
            buf,
            g.x + 2,
            note_y,
            g.w.saturating_sub(4) as usize,
            text,
            Style::default().fg(color).bg(surface),
        );
    };
    match &v.load {
        Load::Loading => note(buf, "Loading diff…", p.muted),
        Load::Failed(_) => {
            let text = if v.source.needs_base() && v.source_base(mem).is_none() {
                "Could not load diff. Choose a base branch from Diff source, or select Uncommitted."
            } else {
                "Could not load diff. Reopen the diff viewer to try again."
            };
            note(buf, text, p.error);
        }
        Load::Unavailable => note(
            buf,
            "Committed comparison unavailable without base metadata. Choose a base branch from Diff source.",
            p.muted,
        ),
        Load::Ready { .. } if v.files.is_empty() => note(buf, "No changes to show", p.muted),
        Load::Ready { .. } => {
            if g.tree {
                tree(buf, &p, &g, v, km);
            }
            patches(buf, &p, &g, v);
        }
    }
    if g.compact {
        footer(buf, &p, &g, v, km, surface, page);
    }
}

/// `Label · detail` on the left and the review count on the right.
#[allow(clippy::too_many_arguments)]
fn header_row(
    buf: &mut Buffer,
    p: &Palette,
    v: &Viewer,
    x: u16,
    y: u16,
    w: u16,
    bg: Color,
    reviewed_word: bool,
) {
    let count = format!(
        "{}/{}{}",
        v.reviewed_count(),
        v.files.len(),
        if reviewed_word { " reviewed" } else { "" }
    );
    let cw = count.width();
    let room = (w as usize).saturating_sub(cw + 1);
    let label = v.source.label();
    let used = put(
        buf,
        x,
        y,
        room,
        label,
        Style::default()
            .fg(p.action_secondary)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    if let Some(d) = v.detail() {
        let rest = room.saturating_sub(used);
        put(
            buf,
            x + used as u16,
            y,
            rest,
            &cut(&format!(" · {d}"), rest),
            Style::default().fg(p.muted).bg(bg),
        );
    }
    if w as usize >= cw {
        put(
            buf,
            x + w - cw as u16,
            y,
            cw,
            &count,
            Style::default().fg(p.muted).bg(bg),
        );
    }
}

fn tree(buf: &mut Buffer, p: &Palette, g: &Geom, v: &Viewer, km: &Keymap) {
    let bg = p.raised;
    for y in 0..g.height {
        fill(buf, g.x, y, g.tree_w, Style::default().fg(p.text).bg(bg));
    }
    let x = g.x + 2;
    let w = g.tree_w.saturating_sub(4);
    header_row(buf, p, v, x, 1, w, bg, false);
    let help = help_key(km);
    let bottom = if help.is_some() {
        g.height.saturating_sub(4)
    } else {
        g.height.saturating_sub(1)
    };
    if let Some(h) = &help {
        let y = g.height.saturating_sub(3);
        let used = put(buf, x, y, w as usize, h, Style::default().fg(p.text).bg(bg));
        put(
            buf,
            x + used as u16,
            y,
            (w as usize).saturating_sub(used),
            " help",
            Style::default().fg(p.muted).bg(bg),
        );
    }
    let rows = diff_tree::flatten(&v.tree, Some(&v.expanded));
    let room = bottom.saturating_sub(3) as usize;
    let selected = v.tree_selected();
    if let Some(i) = rows
        .iter()
        .position(|r| r.file.is_some() && r.file == selected)
    {
        let top = v.tree_scroll.get();
        if i < top {
            v.tree_scroll.set(i);
        } else if i >= top + room {
            v.tree_scroll.set(i + 1 - room);
        }
    }
    let top = v
        .tree_scroll
        .get()
        .min(rows.len().saturating_sub(room.max(1)));
    let content = (g.tree_w as usize).saturating_sub(6);
    for (n, row) in rows.iter().skip(top).take(room).enumerate() {
        let y = 3 + n as u16;
        let indent = "│ ".repeat(row.depth.min(content.saturating_sub(3) / 2));
        let marker = match row.kind {
            NodeKind::File => "≡ ",
            NodeKind::Directory if v.expanded.contains(&row.id) => "▾ ",
            NodeKind::Directory => "▸ ",
        };
        let mut cx = x;
        cx += put(
            buf,
            cx,
            y,
            w as usize,
            &indent,
            Style::default().fg(p.tree_rail).bg(bg),
        ) as u16;
        cx += put(
            buf,
            cx,
            y,
            (w as usize).saturating_sub((cx - x) as usize),
            marker,
            Style::default().fg(p.tree_marker).bg(bg),
        ) as u16;
        let name_w = content.saturating_sub(indent.width() + marker.width());
        let reviewed = row.file.is_some_and(|f| v.is_reviewed(f));
        let is_sel = row.file.is_some() && row.file == selected;
        let mut style = Style::default().bg(bg).fg(match row.kind {
            NodeKind::Directory => p.muted,
            _ if is_sel => p.action_selected,
            _ if reviewed => p.muted,
            _ => p.text,
        });
        if is_sel {
            style = style.add_modifier(Modifier::BOLD);
        }
        let name = match row.kind {
            NodeKind::Directory => truncate_chain(&row.name, name_w),
            NodeKind::File => {
                let (d, n) = path_parts(&row.name, name_w);
                format!("{d}{n}")
            }
        };
        put(buf, cx, y, name_w, &name, style);
        if let Some(f) = row.file {
            let file = &v.files[f];
            let (text, color) = if reviewed {
                ("✓", p.muted)
            } else {
                match file.status {
                    Status::Added => ("A", p.diff_added),
                    Status::Deleted => ("D", p.diff_removed),
                    Status::Modified => ("M", p.muted),
                }
            };
            put(
                buf,
                g.x + g.tree_w - 3,
                y,
                1,
                text,
                Style::default().fg(color).bg(bg),
            );
        }
    }
}

fn footer(
    buf: &mut Buffer,
    p: &Palette,
    g: &Geom,
    v: &Viewer,
    km: &Keymap,
    surface: Color,
    page: Style,
) {
    let h = g.height;
    let y = if g.docked {
        h.saturating_sub(2)
    } else {
        h.saturating_sub(1)
    };
    if g.docked {
        let edge = h.saturating_sub(3);
        glyphs(
            buf,
            g.x,
            edge,
            g.w,
            "▀",
            Style::default().fg(surface).bg(p.bg),
        );
        fill(buf, g.x, h.saturating_sub(1), g.w, page);
    }
    fill(buf, g.x, y, g.w, page);
    let focused = !g.docked || v.focused;
    let (id, label) = if g.docked {
        ("pane.focus.left", " focus prompt")
    } else {
        ("diff.close", " close")
    };
    let key_style = if focused {
        Style::default().fg(p.action_focused).bg(p.bg)
    } else {
        Style::default()
            .fg(p.action_disabled)
            .bg(p.bg)
            .add_modifier(Modifier::DIM)
    };
    let label_style = if focused {
        Style::default().fg(p.muted).bg(p.bg)
    } else {
        key_style
    };
    let help = help_key(km).map(|k| format!("{k} see all"));
    let help_w = help.as_ref().map_or(0, |s| s.width());
    let room = (g.w as usize).saturating_sub(3 + help_w + 1);
    if let Some(k) = km.list(id).into_iter().next() {
        let used = put(buf, g.x + 2, y, room, &k, key_style);
        put(
            buf,
            g.x + 2 + used as u16,
            y,
            room.saturating_sub(used),
            label,
            label_style,
        );
    }
    if let Some(hs) = help {
        let k = help_key(km).unwrap_or_default();
        let x = g.x + g.w - 1 - help_w as u16;
        let (ks, ls) = if focused {
            (
                Style::default().fg(p.text).bg(p.bg),
                Style::default().fg(p.muted).bg(p.bg),
            )
        } else {
            (key_style, key_style)
        };
        let used = put(buf, x, y, help_w, &k, ks);
        put(buf, x + used as u16, y, help_w - used, &hs[k.len()..], ls);
    }
}

/// The card at the top of the view (whose edge color the pane's top edge
/// takes).
fn top_card(l: &Layout, scroll: usize) -> Option<usize> {
    l.cards
        .iter()
        .rev()
        .find(|(_, c)| c.node <= scroll)
        .map(|(f, _)| *f)
}

fn patches(buf: &mut Buffer, p: &Palette, g: &Geom, v: &Viewer) {
    let l = v.layout(g);
    let card_x = g.patch_x + g.inset;
    let card_w = g.card_w();
    let scroll = v.scroll.min(l.total.saturating_sub(g.view_h as usize));
    // The pane's fixed top edge, in the color of the card under it.
    let edge = top_card(&l, scroll).map_or(p.diff_context_bg, |f| card_bg(v, p, f));
    fill(buf, card_x, g.view_y - 1, card_w, Style::default().bg(edge));
    let Some((mut i, mut skip)) = l.at(scroll) else {
        return;
    };
    let mut y = g.view_y;
    let end = g.view_y + g.view_h;
    let mut hl = v.highlight.borrow_mut();
    while y < end && i < l.items.len() {
        let h = l.height(i);
        let n = (h - skip).min((end - y) as usize);
        item(buf, p, g, v, l.items[i], skip, n, y, &mut hl);
        y += n as u16;
        i += 1;
        skip = 0;
    }
    // The title of the card at the top stays in view while its card does.
    if let Some((f, c)) = l
        .cards
        .iter()
        .find(|(_, c)| c.header <= scroll && scroll < c.end)
    {
        if scroll > c.header {
            let shift = (scroll - c.header).min((c.end - c.header).saturating_sub(HEADER_HEIGHT));
            let row = c.header + shift;
            let at = row as isize - scroll as isize;
            for r in 0..HEADER_HEIGHT {
                let sy = at + r as isize;
                if sy >= 0 && (sy as u16) < g.view_h {
                    item(
                        buf,
                        p,
                        g,
                        v,
                        Item::Header(*f),
                        r,
                        1,
                        g.view_y + sy as u16,
                        &mut hl,
                    );
                }
            }
        }
    }
}

/// Rows `skip..skip + n` of an item, drawn from screen row `y`.
#[allow(clippy::too_many_arguments)]
fn item(
    buf: &mut Buffer,
    p: &Palette,
    g: &Geom,
    v: &Viewer,
    it: Item,
    skip: usize,
    n: usize,
    y: u16,
    hl: &mut Highlights,
) {
    let card_x = g.patch_x + g.inset;
    let card_w = g.card_w();
    let surface = if g.docked { p.raised } else { p.bg };
    let f = it.file();
    let bg = card_bg(v, p, f);
    let gap = |buf: &mut Buffer, y: u16, glyph: &str, card: Color| {
        if g.inset > 0 {
            glyphs(
                buf,
                g.patch_x,
                y,
                g.inset,
                glyph,
                Style::default().fg(surface).bg(p.bg),
            );
            glyphs(
                buf,
                card_x + card_w,
                y,
                g.inset,
                glyph,
                Style::default().fg(surface).bg(p.bg),
            );
        }
        glyphs(
            buf,
            card_x,
            y,
            card_w,
            glyph,
            Style::default().fg(card).bg(p.bg),
        );
    };
    match it {
        Item::Gap(_) => gap(buf, y, "▄", bg),
        Item::Header(_) => {
            for r in skip..skip + n {
                let y = y + (r - skip) as u16;
                if r == 0 {
                    title(buf, p, g, v, f, y, bg);
                } else if v.is_reviewed(f) {
                    gap(buf, y, "▀", bg);
                } else {
                    fill(buf, card_x, y, card_w, Style::default().bg(bg));
                }
            }
        }
        Item::Message(_) => {
            for r in skip..skip + n {
                let y = y + (r - skip) as u16;
                fill(buf, card_x, y, card_w, Style::default().bg(bg));
                if r == 0 {
                    let pad = if g.compact { 2 } else { 1 };
                    let text = v.message(f).unwrap_or_default();
                    put(
                        buf,
                        card_x + pad,
                        y,
                        (card_w as usize).saturating_sub(pad as usize + 1),
                        text,
                        Style::default().fg(p.muted).bg(bg),
                    );
                }
            }
        }
        Item::HunkGap(_) => fill(
            buf,
            card_x,
            y,
            card_w,
            Style::default().bg(p.diff_context_bg),
        ),
        Item::Unified { hunk, line, .. } => {
            let pl = &v.files[f].hunks[hunk].lines[line];
            side(
                buf,
                p,
                v,
                f,
                hunk,
                Some(line),
                pl.kind != LineKind::Del,
                card_x,
                card_w,
                skip,
                n,
                y,
                g.compact,
                hl,
            );
        }
        Item::Split {
            hunk, left, right, ..
        } => {
            let half = card_w / 2;
            side(
                buf, p, v, f, hunk, left, false, card_x, half, skip, n, y, g.compact, hl,
            );
            side(
                buf,
                p,
                v,
                f,
                hunk,
                right,
                true,
                card_x + half,
                card_w - half,
                skip,
                n,
                y,
                g.compact,
                hl,
            );
        }
    }
}

fn title(buf: &mut Buffer, p: &Palette, g: &Geom, v: &Viewer, f: usize, y: u16, bg: Color) {
    let card_x = g.patch_x + g.inset;
    let card_w = g.card_w() as usize;
    fill(buf, card_x, y, card_w as u16, Style::default().bg(bg));
    let file = &v.files[f];
    let reviewed = v.is_reviewed(f);
    let image = v.prep[f].image;
    let mut right: Vec<(String, Color)> = Vec::new();
    if reviewed {
        right.push(("✓".into(), p.muted));
    }
    if image {
        right.push(("Image".into(), p.muted));
    } else {
        right.push((
            format!("+{}", file.additions),
            if reviewed { p.muted } else { p.diff_added },
        ));
        right.push((
            format!("-{}", file.deletions),
            if reviewed { p.muted } else { p.diff_removed },
        ));
    }
    let right_w: usize = right.iter().map(|(s, _)| s.width() + 1).sum::<usize>();
    let pad = if g.compact { 2 } else { 1 };
    let max = card_w.saturating_sub(pad + 1 + right_w + 1);
    let (dir, name) = path_parts(&file.file, max);
    let mut x = card_x + pad as u16;
    x += put(buf, x, y, max, &dir, Style::default().fg(p.muted).bg(bg)) as u16;
    put(
        buf,
        x,
        y,
        max.saturating_sub(dir.width()),
        &name,
        Style::default()
            .fg(if reviewed { p.muted } else { p.text })
            .bg(bg),
    );
    let mut rx = (card_x as usize + card_w).saturating_sub(1 + right_w) as u16 + 1;
    for (s, c) in right {
        rx += put(buf, rx, y, s.width(), &s, Style::default().fg(c).bg(bg)) as u16 + 1;
    }
}

/// One side of a patch row (all of a unified one): gutter, sign and the
/// line's text wrapped by character, rows `skip..skip + n`.
#[allow(clippy::too_many_arguments)]
fn side(
    buf: &mut Buffer,
    p: &Palette,
    v: &Viewer,
    f: usize,
    hunk: usize,
    line: Option<usize>,
    new: bool,
    x: u16,
    w: u16,
    skip: usize,
    n: usize,
    y: u16,
    compact: bool,
    hl: &mut Highlights,
) {
    let gutter = v.gutter(f, compact);
    let Some(li) = line else {
        for r in 0..n {
            fill(
                buf,
                x,
                y + r as u16,
                w,
                Style::default().bg(p.diff_context_bg),
            );
        }
        return;
    };
    let pl = &v.files[f].hunks[hunk].lines[li];
    let (bg, ln_bg, sign, sign_fg) = match pl.kind {
        LineKind::Context => (p.diff_context_bg, p.diff_context_bg, " ", p.diff_context),
        LineKind::Add => (
            p.diff_added_bg,
            p.diff_line_number_added_bg,
            "+",
            p.diff_sign_added,
        ),
        LineKind::Del => (
            p.diff_removed_bg,
            p.diff_line_number_removed_bg,
            "-",
            p.diff_sign_removed,
        ),
    };
    let room = (w as usize).saturating_sub(gutter + 2).max(1);
    for r in 0..n {
        let sy = y + r as u16;
        fill(buf, x, sy, gutter as u16, Style::default().bg(ln_bg));
        fill(
            buf,
            x + gutter as u16,
            sy,
            w.saturating_sub(gutter as u16),
            Style::default().bg(bg),
        );
        if skip + r == 0 {
            let num = if pl.kind == LineKind::Del {
                pl.old
            } else {
                pl.new
            };
            let digits = v.prep[f].digits;
            let text = format!("{num:>digits$}");
            put(
                buf,
                x + (gutter - 1 - digits) as u16,
                sy,
                digits,
                &text,
                Style::default().fg(p.diff_line_number).bg(ln_bg),
            );
            put(
                buf,
                x + gutter as u16,
                sy,
                1,
                sign,
                Style::default().fg(sign_fg).bg(bg),
            );
        }
    }
    let (o, nw) = v.prep[f].side[hunk][li];
    let index = if new { nw } else { o } as usize;
    let runs = hl.runs(v, p, f, new, index).unwrap_or_default();
    let base = Style::default().fg(p.text).bg(bg);
    // Walk the text once, cutting it into rows of `room` columns.
    let tx = x + gutter as u16 + 2;
    let mut row = 0usize;
    let mut col = 0usize;
    let mut run = 0usize;
    let mut seg = String::new();
    let mut seg_style = base;
    let mut seg_x = 0usize;
    let flush = |buf: &mut Buffer, seg: &mut String, style: Style, row: usize, sx: usize| {
        if !seg.is_empty() && row >= skip && row < skip + n {
            put(
                buf,
                tx + sx as u16,
                y + (row - skip) as u16,
                room - sx,
                seg,
                style,
            );
        }
        seg.clear();
    };
    for (bi, c) in pl.text.char_indices() {
        if row >= skip + n {
            break;
        }
        while run < runs.len() && bi as u32 >= runs[run].0 {
            run += 1;
        }
        let style = runs.get(run).map_or(base, |(_, s)| base.patch(*s));
        let cw = c.width().unwrap_or(0);
        if col + cw > room && col > 0 {
            flush(buf, &mut seg, seg_style, row, seg_x);
            row += 1;
            col = 0;
            seg_x = 0;
        }
        if style != seg_style {
            flush(buf, &mut seg, seg_style, row, seg_x);
            seg_style = style;
            seg_x = col;
        }
        if seg.is_empty() {
            seg_x = col;
        }
        seg.push(c);
        col += cw;
    }
    flush(buf, &mut seg, seg_style, row, seg_x);
}

/// The shortcuts dialog's rows, in OpenCode's groups, with the bindings in
/// effect (`unbound` where there is none). `single`: single-file view;
/// `tree`: the layout has a tree to toggle.
pub fn help_lines(km: &Keymap, single: bool, tree: bool) -> Vec<(String, String, String)> {
    let all = |id: &str| km.list(id).join(" / ");
    let first = |ids: &[&str]| {
        ids.iter()
            .filter_map(|id| km.list(id).into_iter().next())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let mut out = Vec::new();
    let mut group = |title: &str, rows: Vec<(String, &str)>| {
        for (k, label) in rows {
            let k = if k.is_empty() { "unbound".into() } else { k };
            out.push((title.to_string(), k, label.to_string()));
        }
    };
    group(
        "Review",
        vec![
            (all("diff.next_file"), "Next file"),
            (all("diff.previous_file"), "Previous file"),
            (
                first(&["diff.mark_reviewed"]),
                if single {
                    "Review + next / reopen"
                } else {
                    "Review + collapse / reopen"
                },
            ),
            (
                first(&["diff.next_hunk", "diff.previous_hunk"]),
                "Next / previous change",
            ),
        ],
    );
    group(
        "Scroll",
        vec![
            (first(&["diff.down", "diff.up"]), "Down / up"),
            (
                first(&["diff.half_page.down", "diff.half_page.up"]),
                "Half page down / up",
            ),
            (first(&["diff.page.down", "diff.page.up"]), "Page down / up"),
            (first(&["diff.first", "diff.last"]), "First / last"),
        ],
    );
    let mut view = vec![
        (first(&["diff.toggle_view"]), "Split / unified"),
        (first(&["diff.single_patch"]), "All files / single file"),
    ];
    if tree {
        view.push((first(&["diff.toggle_file_tree"]), "Show / hide file tree"));
    }
    view.push((first(&["diff.switch_source"]), "Switch diff source"));
    view.push((
        first(&["diff.toggle_fullscreen"]),
        "Full screen / split view",
    ));
    view.push((first(&["pane.focus.left"]), "Focus the prompt (docked)"));
    view.push((all("diff.close"), "Close diff viewer"));
    group("View", view);
    out
}
