//! What mini mode writes to the scrollback, and when.
//!
//! The transcript is cut into rows as the full UI draws it: an entry, or a
//! run of entries one group holds (see [`crate::tui::group`]). A row is
//! written once it is finished, which is when the turn is over, or when a
//! later row has started and none of its calls is still running. It is
//! rendered then, once, with the same renderers as the full UI (Markdown,
//! diffs, groups, the theme) at the terminal's width, and never changes
//! after: the scrollback is the terminal's.
//!
//! A streamed answer is written a block at a time, as each is finished:
//! up to the last blank line outside a code fence, when the line after it
//! starts a new block. Code blocks are held until they close.
//!
//! Rows not written yet are the footer's live view.

use super::settings::Settings;
use crate::tui::app::{App, Entry};
use crate::tui::view;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;
use std::time::Instant;

#[derive(Default)]
pub struct Committer {
    /// Entries written whole.
    sealed: usize,
    /// The answer being streamed: its entry, and how many of its bytes are
    /// written.
    partial: Option<(usize, usize)>,
    /// The session the entries are of.
    session: Option<String>,
    /// The rows not written yet, as last drawn.
    pub live: Vec<Line<'static>>,
    /// The last row written was blank (or none was).
    blank: bool,
    /// What kind of row was written last.
    last: Option<Kind>,
    /// When the running turn started, and with which agent.
    turn: Option<(Instant, String)>,
    /// `/clear`: the screen should be cleared.
    pub clear: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Prompts, answers, notices: set apart by a blank row.
    Text,
    /// Tool calls and thoughts, which follow one another closely.
    Activity,
}

/// The transcript's rows from entry `from` on, as ranges of entries. An
/// entry that draws nothing (an empty answer) goes with the row after it,
/// or with the group it is inside; trailing ones are in no row.
pub fn rows(app: &App, from: usize) -> Vec<Range<usize>> {
    let e = &app.entries;
    let mut out = Vec::new();
    let mut i = from;
    while i < e.len() {
        let start = i;
        while i < e.len() && view::entry_path(app, &e[i]).is_none() {
            i += 1;
        }
        let Some(path) = e.get(i).and_then(|x| view::entry_path(app, x)) else {
            break;
        };
        i += 1;
        if let Some(&kind) = path.first() {
            while i < e.len() {
                match view::entry_path(app, &e[i]) {
                    Some(p) if p.first() != Some(&kind) => break,
                    _ => i += 1,
                }
            }
        }
        out.push(start..i);
    }
    out
}

fn running(app: &App, r: &Range<usize>) -> bool {
    app.entries[r.clone()]
        .iter()
        .any(|e| matches!(e, Entry::Tool(t) if t.result.is_none()))
}

/// A row that cannot change any more, even as the last one: a prompt, a
/// notice or an error.
fn settled(app: &App, r: &Range<usize>) -> bool {
    matches!(
        &app.entries[r.end - 1],
        Entry::User(_) | Entry::Notice(_) | Entry::Error(_) | Entry::Media(_)
    )
}

fn is_shell(name: &str) -> bool {
    matches!(name, "shell" | "bash")
}

/// Where a streamed answer can be cut and its start written: just after
/// the last blank line (outside a code fence) that ends a block, when the
/// line after it has begun at its first column. `from` when there is none.
pub fn safe_cut(text: &str, from: usize) -> usize {
    let mut cut = from;
    let mut fence: Option<(char, usize)> = None;
    let mut blank = false;
    let mut seen = false;
    let mut at = from;
    for line in text[from..].split_inclusive('\n') {
        let start = at;
        at += line.len();
        let trimmed = line.trim();
        if blank && fence.is_none() && seen && !line.starts_with([' ', '\t', '\n', '\r']) {
            cut = start;
            seen = false;
        }
        if !line.ends_with('\n') {
            break;
        }
        let t = line.trim_start();
        let marker = t.chars().next().filter(|c| *c == '`' || *c == '~');
        if let Some(m) = marker {
            let n = t.chars().take_while(|c| *c == m).count();
            if n >= 3 {
                match fence {
                    None => fence = Some((m, n)),
                    Some((fm, fnn))
                        if fm == m && n >= fnn && t.trim_end().chars().all(|c| c == m) =>
                    {
                        fence = None
                    }
                    _ => {}
                }
            }
        }
        blank = trimmed.is_empty();
        if !blank {
            seen = true;
        }
    }
    cut
}

impl Committer {
    fn kind(app: &App, ids: &[usize]) -> Kind {
        match ids.first().map(|&i| &app.entries[i]) {
            Some(Entry::Tool(_) | Entry::Reasoning(_)) => Kind::Activity,
            _ => Kind::Text,
        }
    }

    /// Add `lines` (a row of kind `kind`) to `out`, a blank row apart from
    /// the one before when either is text, and never two blank rows.
    fn push(&mut self, out: &mut Vec<Line<'static>>, lines: Vec<Line<'static>>, kind: Kind) {
        let blank = |l: &Line| l.spans.iter().all(|s| s.content.trim().is_empty());
        let mut lines = lines.into_iter().peekable();
        while lines.peek().is_some_and(blank) && (self.blank || self.last.is_none()) {
            lines.next();
        }
        let lines: Vec<Line<'static>> = lines.collect();
        if lines.is_empty() {
            return;
        }
        let spaced = self
            .last
            .is_some_and(|k| k == Kind::Text || kind == Kind::Text);
        if spaced && !self.blank && !blank(&lines[0]) {
            out.push(Line::default());
        }
        for l in lines {
            let b = blank(&l);
            if b && self.blank {
                continue;
            }
            self.blank = b;
            out.push(l);
        }
        self.last = Some(kind);
    }

    /// The entries of a row that the scrollback shows.
    fn shown(app: &App, s: &Settings, r: Range<usize>) -> Vec<usize> {
        r.filter(|&i| match &app.entries[i] {
            Entry::Tool(t) => s.tools || t.base().eq_ignore_ascii_case("question"),
            Entry::Reasoning(_) => app.show_reasoning,
            _ => true,
        })
        .collect()
    }

    /// The answer in `ids` (the only entry there that draws anything),
    /// when part of it is written: the rest of it.
    fn rest_of_answer(&self, app: &App, ids: &[usize], w: usize) -> Option<Vec<Line<'static>>> {
        let (i, from) = self.partial?;
        let others_empty = ids
            .iter()
            .all(|&j| j == i || view::entry_path(app, &app.entries[j]).is_none());
        match app.entries.get(i) {
            Some(Entry::Assistant(t)) if ids.contains(&i) && others_empty => Some(
                view::markdown_cached(t[from.min(t.len())..].trim_start_matches('\n'), w),
            ),
            _ => None,
        }
    }

    /// A finished row's lines.
    fn render(&self, app: &mut App, s: &Settings, ids: &[usize], w: usize) -> Vec<Line<'static>> {
        if let Some(lines) = self.rest_of_answer(app, ids, w) {
            return lines;
        }
        // Shell output is left out unless asked for.
        let mut hidden = Vec::new();
        if !s.shell_output {
            for &i in ids {
                if let Entry::Tool(t) = &mut app.entries[i] {
                    if is_shell(t.base()) && t.result.as_ref().is_some_and(|(_, err)| !err) {
                        hidden.push((i, std::mem::take(&mut t.output)));
                    }
                }
            }
        }
        // Shown thoughts are written in full, not summed up in a group.
        let mut lines = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        for &i in ids {
            if matches!(app.entries[i], Entry::Reasoning(_)) {
                lines.extend(view::entries_lines(app, &std::mem::take(&mut run), w, true));
                lines.extend(view::entry_rows(app, i, w));
            } else {
                run.push(i);
            }
        }
        lines.extend(view::entries_lines(app, &run, w, true));
        for (i, out) in hidden {
            if let Entry::Tool(t) = &mut app.entries[i] {
                t.output = out;
            }
        }
        lines
    }

    /// Where a replay of the entries before `end` starts: the newest
    /// `replay_limit` prompts and answers.
    fn replay_start(app: &App, s: &Settings, end: usize) -> usize {
        if !s.replay {
            return end;
        }
        let mut n = 0;
        for i in (0..end).rev() {
            if matches!(&app.entries[i], Entry::User(_) | Entry::Assistant(_)) {
                n += 1;
                if n > s.replay_limit {
                    return i + 1;
                }
            }
        }
        0
    }

    /// The rows to write this frame, at `w` columns; [`Committer::live`] is
    /// left holding the rest.
    pub fn step(&mut self, app: &mut App, s: &Settings, w: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let session = app.header.session.clone();
        if let Some(n) = app.replaced.take() {
            let n = n.min(app.entries.len());
            if self.session.as_deref() == Some(session.as_str()) {
                // The same session reloaded (after an interrupt, an undo, a
                // `/clear`): what was on view of the last turn is kept, and
                // the history already written is not written again.
                let live = std::mem::take(&mut self.live);
                self.push(&mut out, live, Kind::Text);
                self.sealed = n;
                self.clear |= n == 0;
            } else {
                self.sealed = Self::replay_start(app, s, n);
            }
            self.partial = None;
        }
        self.session = Some(session);
        if app.busy && self.turn.is_none() {
            self.turn = Some((Instant::now(), app.header.agent.clone()));
        }
        self.sealed = self.sealed.min(app.entries.len());
        let rows = rows(app, self.sealed);
        let count = rows.len();
        for (k, r) in rows.into_iter().enumerate() {
            let last = k + 1 == count;
            if app.busy && ((last && !settled(app, &r)) || running(app, &r)) {
                // The answer being streamed: its finished blocks.
                let i = r.end - 1;
                let alone = (r.start..i).all(|j| view::entry_path(app, &app.entries[j]).is_none());
                if last && alone {
                    if let Entry::Assistant(t) = &app.entries[i] {
                        let from = match self.partial {
                            Some((p, b)) if p == i => b,
                            _ => 0,
                        };
                        let cut = safe_cut(t, from);
                        if cut > from {
                            let lines =
                                view::markdown_cached(t[from..cut].trim_start_matches('\n'), w);
                            self.push(&mut out, lines, Kind::Text);
                            self.partial = Some((i, cut));
                        }
                    }
                }
                break;
            }
            let ids = Self::shown(app, s, r.clone());
            if !ids.is_empty() {
                let kind = Self::kind(app, &ids);
                let lines = self.render(app, s, &ids, w);
                self.push(&mut out, lines, kind);
            }
            self.sealed = r.end;
            self.partial = None;
        }
        if !app.busy {
            self.sealed = app.entries.len();
            if let Some((start, agent)) = self.turn.take() {
                if s.turn_summary {
                    let line = Line::from(Span::styled(
                        format!(
                            "{agent} \u{b7} {} \u{b7} {}{}",
                            app.header.model,
                            duration(start.elapsed().as_millis() as u64),
                            // The throughput of the turn just ended.
                            match app.last_turn {
                                Some((_, Some(t))) => format!(" \u{b7} {t:.1} tok/s"),
                                _ => String::new(),
                            }
                        ),
                        Style::default()
                            .fg(crate::tui::theme::current().muted)
                            .add_modifier(Modifier::empty()),
                    ));
                    self.push(&mut out, vec![line], Kind::Text);
                }
            }
        }
        app.sealed = self.sealed;
        self.live = self.unsealed(app, w);
        out
    }

    /// The rows not written yet.
    fn unsealed(&self, app: &App, w: usize) -> Vec<Line<'static>> {
        let ids: Vec<usize> = (self.sealed..app.entries.len()).collect();
        self.rest_of_answer(app, &ids, w)
            .unwrap_or_else(|| view::entries_lines(app, &ids, w, false))
    }

    /// Everything written so far, again, at `w` columns (after a resize
    /// cleared the screen): the newest `replay_limit` messages of it.
    pub fn replay(&mut self, app: &mut App, s: &Settings, w: usize) -> Vec<Line<'static>> {
        let (end, partial) = (self.sealed, self.partial);
        self.blank = true;
        self.last = None;
        let mut out = Vec::new();
        for r in rows(app, Self::replay_start(app, s, end)) {
            if r.start >= end {
                break;
            }
            let ids = Self::shown(app, s, r.clone());
            if !ids.is_empty() {
                let kind = Self::kind(app, &ids);
                let lines = self.render(app, s, &ids, w);
                self.push(&mut out, lines, kind);
            }
        }
        if let Some((i, b)) = partial {
            if let Some(Entry::Assistant(t)) = app.entries.get(i) {
                let lines = view::markdown_cached(t[..b].trim_start_matches('\n'), w);
                self.push(&mut out, lines, Kind::Text);
            }
        }
        self.live = self.unsealed(app, w);
        out
    }

    /// Something else was written (a banner): what is written next is a
    /// blank row apart from it.
    pub fn wrote_text(&mut self) {
        self.blank = false;
        self.last = Some(Kind::Text);
    }

    /// When the running turn started.
    pub fn turn_started(&self) -> Option<Instant> {
        self.turn.as_ref().map(|(t, _)| *t)
    }
}

/// A turn's length: `850ms`, `12.3s`, `4m 05s`, `1h 02m`.
pub fn duration(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=59_999 => format!("{:.1}s", ms as f64 / 1e3),
        60_000..=3_599_999 => format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_cut_after_finished_blocks() {
        let t = "First paragraph.\n\nSecond one is still";
        assert_eq!(safe_cut(t, 0), "First paragraph.\n\n".len());
        // The line after the blank one has not started: nothing yet.
        assert_eq!(safe_cut("First.\n\n", 0), 0);
        // Inside a code fence, blank lines do not end a block.
        let t = "Intro.\n\n```rust\nfn a() {}\n\nfn b() {}\n";
        assert_eq!(safe_cut(t, 0), "Intro.\n\n".len());
        let t = "Intro.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nAfter";
        assert_eq!(safe_cut(t, 0), t.len() - "After".len());
        // An indented line continues the block before (a list item's
        // paragraph): not a place to cut.
        let t = "1. Item\n\n   more of it\n";
        assert_eq!(safe_cut(t, 0), 0);
        // From a later start, only what follows counts.
        let t = "A.\n\nB.\n\nC";
        assert_eq!(safe_cut(t, 4), t.len() - 1);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(850), "850ms");
        assert_eq!(duration(12_345), "12.3s");
        assert_eq!(duration(245_000), "4m 05s");
        assert_eq!(duration(3_720_000), "1h 02m");
    }
}
