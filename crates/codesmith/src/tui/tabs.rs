//! Session tabs, as OpenCode's (`context/session-tabs-model`,
//! `component/session-tabs`): several sessions open at once in a strip
//! above the transcript, each with its own turn, background commands and
//! prompt draft. This file holds the pure parts: the tab list's
//! operations (open, close, move, cycle, the selection history and the
//! closed-tab stack), the strip's adaptive layout and drawing, and the
//! list kept across restarts. `tabs_host.rs` runs the tabs.

use super::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde_json::{json, Map, Value};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

/// An open tab: its session and the title it last showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub session: String,
    pub title: Option<String>,
}

impl Tab {
    pub fn new(session: &str) -> Tab {
        Tab {
            session: session.to_string(),
            title: None,
        }
    }

    #[cfg(test)]
    pub fn titled(session: &str, title: &str) -> Tab {
        Tab {
            session: session.to_string(),
            title: Some(title.to_string()),
        }
    }
}

/// Why a tab is marked unread: its turn ended, or ended in an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    Activity,
    Error,
}

/// A tab waiting for its user: a permission request or a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    Permission,
    Question,
}

/// What a tab's indicator shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    pub busy: bool,
    pub attention: Option<Attention>,
    pub unread: Option<Unread>,
}

/// The title of a tab whose session has none yet.
pub const NEW_SESSION_TITLE: &str = "New session";

/// The tab's ordinal, as its number indicator shows it.
pub fn number_label(index: usize) -> String {
    (index + 1).to_string()
}

pub const TAB_WIDTH: usize = 22;
pub const TAB_MAX_WIDTH: usize = 32;
pub const TAB_MIN_WIDTH: usize = 8;

/// The width of an overflow marker: its arrow, its count and a gap cell
/// (`‹12 `, ` 12›`).
pub fn overflow_width(count: usize) -> usize {
    count.to_string().len() + 2
}

/// Open `tab` (at the end) or refresh its title; `false` when nothing changed.
pub fn open(tabs: &mut Vec<Tab>, tab: Tab) -> bool {
    match tabs.iter_mut().find(|t| t.session == tab.session) {
        None => {
            tabs.push(tab);
            true
        }
        Some(t) if tab.title.is_none() || t.title == tab.title => false,
        Some(t) => {
            t.title = tab.title;
            true
        }
    }
}

/// Close `session`'s tab, and the tab to select next: the one on its
/// right, else on its left. A tab already gone (closed by another
/// window sharing the list) picks the neighbour at its last known index.
/// The `bool` says whether a tab was removed.
pub fn close(tabs: &mut Vec<Tab>, session: &str, last: Option<usize>) -> (bool, Option<String>) {
    match tabs.iter().position(|t| t.session == session) {
        None => {
            let next = last
                .filter(|_| !tabs.is_empty())
                .map(|i| tabs[i.min(tabs.len() - 1)].session.clone());
            (false, next)
        }
        Some(i) => {
            let next = tabs
                .get(i + 1)
                .or_else(|| i.checked_sub(1).and_then(|j| tabs.get(j)))
                .map(|t| t.session.clone());
            tabs.remove(i);
            (true, next)
        }
    }
}

/// A closed tab, where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub tab: Tab,
    pub index: usize,
}

const CLOSED_LIMIT: usize = 25;

/// Remember a closed tab (one entry per session, the newest last).
pub fn record_closed(stack: &mut Vec<Closed>, tab: Tab, index: usize) {
    stack.retain(|c| c.tab.session != tab.session);
    stack.push(Closed { tab, index });
    if stack.len() > CLOSED_LIMIT {
        stack.drain(..stack.len() - CLOSED_LIMIT);
    }
}

/// Reopen the most recently closed tab that is not open (or `session`'s)
/// at its old place: the new tab list and the session to select. Entries
/// for sessions already open are used up on the way, so repeated reopens
/// walk the stack; reopening one by name leaves the others.
pub fn reopen(
    stack: &mut Vec<Closed>,
    tabs: &[Tab],
    session: Option<&str>,
) -> Option<(Vec<Tab>, String)> {
    while !stack.is_empty() {
        let i = match session {
            Some(s) => stack.iter().position(|c| c.tab.session == s)?,
            None => stack.len() - 1,
        };
        let entry = stack.remove(i);
        if tabs.iter().any(|t| t.session == entry.tab.session) {
            continue;
        }
        let mut next = tabs.to_vec();
        let id = entry.tab.session.clone();
        next.insert(entry.index.min(tabs.len()), entry.tab);
        return Some((next, id));
    }
    None
}

/// Move `session`'s tab to `index` (clamped); `false` when nothing moved.
pub fn move_tab(tabs: &mut Vec<Tab>, session: &str, index: isize) -> bool {
    let Some(from) = tabs.iter().position(|t| t.session == session) else {
        return false;
    };
    let to = index.clamp(0, tabs.len() as isize - 1) as usize;
    if from == to {
        return false;
    }
    let t = tabs.remove(from);
    tabs.insert(to, t);
    true
}

/// The next (`dir` 1) or previous (-1) tab from `active` that `matches`,
/// wrapping around; from no (or an unknown) active tab, the first or last.
pub fn cycle<'a>(
    tabs: &'a [Tab],
    active: Option<&str>,
    dir: i32,
    matches: impl Fn(&Tab) -> bool,
) -> Option<&'a Tab> {
    let n = tabs.len() as i64;
    if n == 0 {
        return None;
    }
    let start = match active.and_then(|a| tabs.iter().position(|t| t.session == a)) {
        Some(i) => i as i64,
        None if dir == 1 => -1,
        None => 0,
    };
    (0..n)
        .map(|o| &tabs[(start + dir as i64 * (o + 1)).rem_euclid(n) as usize])
        .find(|t| matches(t))
}

/// The order tabs were selected in, to go back and forth through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub entries: Vec<String>,
    pub index: Option<usize>,
}

/// Selections kept: the oldest fall off.
const HISTORY_LIMIT: usize = 100;

impl History {
    /// Record a selection, dropping the forward part; `false` when it is
    /// the current entry already.
    pub fn record(&mut self, session: &str) -> bool {
        if self
            .index
            .and_then(|i| self.entries.get(i))
            .map(String::as_str)
            == Some(session)
        {
            return false;
        }
        self.entries.truncate(self.index.map_or(0, |i| i + 1));
        self.entries.push(session.to_string());
        if self.entries.len() > HISTORY_LIMIT {
            self.entries.drain(..self.entries.len() - HISTORY_LIMIT);
        }
        self.index = Some(self.entries.len() - 1);
        true
    }

    /// Step back (-1) or forward (1) to the nearest entry that is open and
    /// not `active`. With no active tab, the current entry, if open.
    pub fn step(&mut self, tabs: &[Tab], active: Option<&str>, dir: i32) -> Option<String> {
        let open = |s: &str| tabs.iter().any(|t| t.session == s);
        let Some(active) = active else {
            let s = self.index.and_then(|i| self.entries.get(i))?;
            return open(s).then(|| s.clone());
        };
        let here = self.index?;
        let found = if dir < 0 {
            (0..here)
                .rev()
                .find(|&i| self.entries[i] != active && open(&self.entries[i]))
        } else {
            (here + 1..self.entries.len())
                .find(|&i| self.entries[i] != active && open(&self.entries[i]))
        }?;
        self.index = Some(found);
        Some(self.entries[found].clone())
    }
}

/// Where the strip puts its tabs: a window of `count` tabs from `start`,
/// their widths, and how many are hidden either side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    pub start: usize,
    pub count: usize,
    pub widths: Vec<usize>,
    pub before: usize,
    pub after: usize,
    pub total: usize,
}

/// OpenCode's `adaptiveSessionTabLayout`: as many tabs as fit in
/// `available` columns with the active one at full width, the window
/// moving only when the selection leaves it. Roomy strips share their
/// width equally (up to the maximum each); crowded ones give the active
/// tab its width and the others the rest.
pub fn layout(n: usize, active: Option<usize>, available: usize, previous_start: usize) -> Layout {
    if n == 0 {
        return Layout::default();
    }
    let (ni, avail) = (n as i64, available as i64);
    let (tw, min) = (TAB_WIDTH as i64, TAB_MIN_WIDTH as i64);
    let fit = |width: i64| -> i64 {
        let w = width.max(0);
        let k = match active {
            None => w.div_euclid(min),
            Some(_) => 1 + (w - tw).div_euclid(min),
        };
        k.max(1).min(ni)
    };
    let ow = |c: i64| overflow_width(c as usize) as i64;
    let mut count = fit(avail);
    let mut start = previous_start as i64;
    for attempts in (0..=3).rev() {
        let bounded = start.max(0).min(ni - count);
        let next_start = match active.map(|a| a as i64) {
            None => bounded,
            Some(a) if a < bounded => a,
            Some(a) if a >= bounded + count => a - count + 1,
            Some(_) => bounded,
        }
        .max(0)
        .min(ni - count);
        let markers = if next_start > 0 { ow(next_start) } else { 0 }
            + if next_start + count < ni {
                ow(ni - next_start - count)
            } else {
                0
            };
        let next_count = fit(avail - markers);
        start = next_start;
        if next_count == count || attempts == 0 {
            break;
        }
        count = next_count;
    }
    let before = start;
    let after = ni - start - count;
    let content =
        (avail - if before > 0 { ow(before) } else { 0 } - if after > 0 { ow(after) } else { 0 })
            .max(1);
    let roomy = content >= tw * count;
    let total = if roomy {
        content.min(TAB_MAX_WIDTH as i64 * count)
    } else {
        content
    };
    let widths: Vec<i64> = if roomy || active.is_none() {
        let w = total / count;
        let rem = total - w * count;
        (0..count).map(|i| w + i64::from(i < rem)).collect()
    } else {
        let inactive = if count == 1 {
            0
        } else {
            tw.min(min.max((total - tw.min(total)).div_euclid(count - 1)))
        };
        let active_w = if count == 1 {
            total
        } else {
            total - inactive * (count - 1)
        };
        let a = active.map(|a| a as i64);
        (start..start + count)
            .map(|i| if Some(i) == a { active_w } else { inactive })
            .collect()
    };
    Layout {
        start: start as usize,
        count: count as usize,
        widths: widths.into_iter().map(|w| w.max(0) as usize).collect(),
        before: before as usize,
        after: after as usize,
        total: total as usize,
    }
}

/// What the column shows under a tab's title: the project, and the
/// branch when it is a worktree's that is not the default branch
/// (`project ⎇ branch`).
pub fn detail(
    project: &str,
    current: Option<&str>,
    default: Option<&str>,
    worktree: bool,
) -> String {
    let branch = current.filter(|c| worktree && Some(*c) != default);
    match branch {
        Some(b) if !project.is_empty() => format!("{project} ⎇ {b}"),
        Some(b) => b.to_string(),
        None => project.to_string(),
    }
}

/// The project's name and, in a linked worktree, its branch, for
/// [`detail`]: read from `.git` (a worktree's is a file naming its git
/// directory, whose `HEAD` names the branch).
pub fn project_detail(root: &Path) -> String {
    let project = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let git = root.join(".git");
    let worktree = git.is_file();
    let head = if worktree {
        std::fs::read_to_string(&git).ok().and_then(|t| {
            let dir = t.trim().strip_prefix("gitdir:")?.trim().to_string();
            std::fs::read_to_string(root.join(dir).join("HEAD")).ok()
        })
    } else {
        std::fs::read_to_string(git.join("HEAD")).ok()
    };
    let branch = head
        .as_deref()
        .and_then(|h| h.trim().strip_prefix("ref: refs/heads/"));
    detail(&project, branch, None, worktree)
}

/// The column's width (OpenCode's `SESSION_SIDEBAR_WIDTH`), and the
/// session's preferred width beside it: narrower terminals get the strip.
pub const COLUMN_WIDTH: u16 = 42;
const CONTENT_PREFERRED_WIDTH: u16 = 64;

pub fn fits_vertically(total: u16) -> bool {
    total >= COLUMN_WIDTH + CONTENT_PREFERRED_WIDTH
}

/// `tui.tabs`, as OpenCode's `tabs` settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    /// Resolved from `mode` and the terminal's environment.
    pub enabled: bool,
    pub scope: Scope,
    pub layout: Orientation,
    pub indicators: Indicators,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    On,
    Off,
}

/// Whether the open tabs are kept per working directory or shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Cwd,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// What the indicator before a title shows: the tab's status, or always
/// its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicators {
    Status,
    Numbers,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::Auto,
            enabled: true,
            scope: Scope::Cwd,
            layout: Orientation::Horizontal,
            indicators: Indicators::Status,
        }
    }
}

/// Read `tui.tabs` (`v` is `tui`): `mode` (`auto`, `on`, `off`; the
/// legacy `enabled` stands for `on` or `off` without one), `scope`,
/// `layout` and `indicators`. `auto` turns tabs on unless the terminal
/// multiplexer herdr (`HERDR_ENV=1`), which has tabs of its own, runs us.
pub fn load(v: &Value, env: impl Fn(&str) -> Option<String>) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut errors = Vec::new();
    let Some(t) = v.get("tabs") else {
        s.enabled = env("HERDR_ENV").as_deref() != Some("1");
        return (s, errors);
    };
    if !t.is_object() {
        errors.push("tui.tabs: must be an object".into());
    }
    let mut pick = |key: &str, values: &[&str]| -> Option<usize> {
        let v = t.get(key)?;
        let found = v.as_str().and_then(|x| values.iter().position(|w| *w == x));
        if found.is_none() {
            let list: Vec<String> = values.iter().map(|w| format!("\"{w}\"")).collect();
            errors.push(format!(
                "tui.tabs.{key}: must be {}",
                match list.split_last() {
                    Some((last, rest)) if !rest.is_empty() =>
                        format!("{} or {last}", rest.join(", ")),
                    _ => list.join(""),
                }
            ));
        }
        found
    };
    let mode = pick("mode", &["auto", "on", "off"]);
    if let Some(i) = pick("scope", &["cwd", "global"]) {
        s.scope = [Scope::Cwd, Scope::Global][i];
    }
    if let Some(i) = pick("layout", &["horizontal", "vertical"]) {
        s.layout = [Orientation::Horizontal, Orientation::Vertical][i];
    }
    if let Some(i) = pick("indicators", &["status", "numbers"]) {
        s.indicators = [Indicators::Status, Indicators::Numbers][i];
    }
    let legacy = match t.get("enabled") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            errors.push("tui.tabs.enabled: must be true or false".into());
            None
        }
    };
    s.mode = match (mode, legacy) {
        (Some(i), _) => [Mode::Auto, Mode::On, Mode::Off][i],
        (None, Some(true)) => Mode::On,
        (None, Some(false)) => Mode::Off,
        (None, None) => Mode::Auto,
    };
    s.enabled = match s.mode {
        Mode::On => true,
        Mode::Off => false,
        Mode::Auto => env("HERDR_ENV").as_deref() != Some("1"),
    };
    (s, errors)
}

/// A tab as it is kept across restarts: its session, title, and when its
/// user last looked at it (ms), against which the session's last change
/// tells whether it is unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub tab: Tab,
    pub viewed: Option<i64>,
}

/// A kept tab is unread when its session changed (ms) after it was last
/// shown, as OpenCode compares a session's idle and viewed times.
pub fn unread_since(updated: i64, viewed: Option<i64>) -> Option<Unread> {
    viewed
        .is_some_and(|v| updated > v)
        .then_some(Unread::Activity)
}

/// Where the open tabs are kept.
pub fn state_file() -> PathBuf {
    codesmith_core::paths::data_dir().join("tui-tabs.json")
}

fn scope_key(scope: Scope, cwd: &Path) -> Option<String> {
    (scope == Scope::Cwd).then(|| cwd.display().to_string())
}

/// The tabs kept for `scope` (and `cwd`) in the state file's `text`, in
/// OpenCode's shape (`{"global": {"tabs": [...]}, "cwd": {dir: {"tabs":
/// [...]}}}`, each tab `{"sessionID", "title"}`, CodeSmith adding
/// `viewed`).
pub fn saved(text: Option<&str>, scope: Scope, cwd: &Path) -> Vec<Saved> {
    let Some(v) = text.and_then(|t| serde_json::from_str::<Value>(t).ok()) else {
        return vec![];
    };
    let state = match scope_key(scope, cwd) {
        None => v.get("global"),
        Some(k) => v.get("cwd").and_then(|c| c.get(&k)),
    };
    let mut out: Vec<Saved> = Vec::new();
    for t in state
        .and_then(|s| s.get("tabs"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = t.get("sessionID").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || out.iter().any(|s| s.tab.session == id) {
            continue;
        }
        out.push(Saved {
            tab: Tab {
                session: id.to_string(),
                title: t.get("title").and_then(Value::as_str).map(str::to_string),
            },
            viewed: t.get("viewed").and_then(Value::as_i64),
        });
    }
    out
}

/// The state file's `text` with `tabs` kept for `scope` (and `cwd`); the
/// other directories' tabs stay as they are.
pub fn with_saved(text: Option<&str>, scope: Scope, cwd: &Path, tabs: &[Saved]) -> String {
    let mut root = text
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let list: Vec<Value> = tabs
        .iter()
        .map(|s| {
            let mut o = Map::new();
            o.insert("sessionID".into(), json!(s.tab.session));
            if let Some(t) = &s.tab.title {
                o.insert("title".into(), json!(t));
            }
            if let Some(v) = s.viewed {
                o.insert("viewed".into(), json!(v));
            }
            Value::Object(o)
        })
        .collect();
    let state = json!({ "tabs": list });
    let o = root.as_object_mut().expect("an object");
    match scope_key(scope, cwd) {
        None => {
            o.insert("global".into(), state);
        }
        Some(k) => {
            let cwd = o.entry("cwd").or_insert_with(|| json!({}));
            if !cwd.is_object() {
                *cwd = json!({});
            }
            cwd.as_object_mut().expect("an object").insert(k, state);
        }
    }
    serde_json::to_string_pretty(&root).unwrap_or_default()
}

/// One tab as the strip draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    pub status: Status,
    /// The new session page's tab (`+ New session`).
    pub new: bool,
}

/// The strip's contents, set by the runner before each frame.
#[derive(Debug, Clone, Default)]
pub struct Strip {
    pub items: Vec<Item>,
    pub active: Option<usize>,
    pub numbers: bool,
    /// `layout: vertical`: a column on the left, where it fits.
    pub vertical: bool,
    /// The line under each title in the column: the project, and its
    /// branch in a worktree ([`detail`]).
    pub detail: String,
    /// Where the window of visible tabs started last frame, so it moves
    /// only when the selection leaves it.
    pub start: Cell<usize>,
}

/// The busy indicator: a forge ember. A tab shows it as a single static
/// glyph (the tab strip draws no animation); the input box animates the
/// ember and its heat while a turn runs.
pub const SPINNER: &str = "✶";
/// The unread marker.
pub const UNREAD: &str = "•";

/// The indicator before a tab's title: its number, or its status (`!`
/// for a permission request, `?` for a question, the spinner while it
/// works, `+` for the new session page, `•` when unread).
pub fn indicator(item: &Item, index: usize, numbers: bool) -> String {
    if numbers {
        return if item.new {
            "+".into()
        } else {
            number_label(index)
        };
    }
    let s = item.status;
    match s.attention {
        Some(Attention::Permission) => return "!".into(),
        Some(Attention::Question) => return "?".into(),
        None => {}
    }
    if s.busy {
        return SPINNER.into();
    }
    if item.new {
        return "+".into();
    }
    if s.unread.is_some() {
        return UNREAD.into();
    }
    String::new()
}

/// The whole graphemes of `s` that fit in `n` columns.
fn take(s: &str, n: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut o = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = g.width();
        if w + gw > n {
            break;
        }
        o.push_str(g);
        w += gw;
    }
    o
}

/// Draw the strip on `area`'s first row.
pub fn draw(buf: &mut Buffer, area: Rect, strip: &Strip) {
    let p = theme::current();
    let row = Rect { height: 1, ..area };
    buf.set_style(row, Style::default().fg(p.text).bg(p.bg));
    let l = layout(
        strip.items.len(),
        strip.active,
        area.width as usize,
        strip.start.get(),
    );
    strip.start.set(l.start);
    let (mut x, y, right) = (area.x, area.y, area.right());
    let put = |buf: &mut Buffer, x: &mut u16, text: &str, width: usize, style: Style| {
        if *x >= right {
            return;
        }
        let room = (right - *x) as usize;
        let w = width.min(room);
        let shown = take(text, w);
        let pad = w - shown.width();
        buf.set_stringn(*x, y, format!("{shown}{}", " ".repeat(pad)), w, style);
        *x += w as u16;
    };
    if l.before > 0 {
        let text = format!("‹{}", l.before);
        put(
            buf,
            &mut x,
            &text,
            overflow_width(l.before),
            Style::default().fg(p.muted),
        );
    }
    let nw = strip.items.len().to_string().len().max(2);
    for (k, width) in l.widths.iter().enumerate() {
        let i = l.start + k;
        let item = &strip.items[i];
        let selected = strip.active == Some(i);
        let bg = if selected { p.raised } else { p.bg };
        let label = indicator(item, i, strip.numbers);
        let mark = match (strip.numbers, item.status) {
            (
                false,
                Status {
                    attention: Some(_), ..
                },
            ) => p.accent,
            (false, Status { busy: true, .. }) => p.primary,
            (
                false,
                Status {
                    unread: Some(Unread::Error),
                    ..
                },
            ) => p.error,
            (
                false,
                Status {
                    unread: Some(Unread::Activity),
                    ..
                },
            ) => p.accent,
            _ if selected => p.text,
            _ => p.muted,
        };
        let mut title_style = Style::default()
            .bg(bg)
            .fg(if selected { p.text } else { p.muted });
        let mut mark_style = Style::default().bg(bg).fg(mark);
        if selected {
            title_style = title_style.add_modifier(Modifier::BOLD);
            mark_style = mark_style.add_modifier(Modifier::BOLD);
        }
        let start = x;
        let lw = nw.min(*width);
        let shown = take(&label, lw);
        let text = format!("{}{shown}", " ".repeat(lw - shown.width()));
        put(buf, &mut x, &text, lw, mark_style);
        if *width > lw {
            put(buf, &mut x, " ", 1, title_style);
            let rest = width.saturating_sub(lw + 1);
            put(buf, &mut x, &item.title, rest, title_style);
        }
        x = x.max(start + *width as u16).min(right);
    }
    if l.after > 0 {
        let text = format!(" {}›", l.after);
        put(
            buf,
            &mut x,
            &text,
            overflow_width(l.after),
            Style::default().fg(p.muted),
        );
    }
}

/// Draw the tabs as a column on `area`: each tab's indicator and title on
/// one row and the detail under it, a blank row between tabs, the shown
/// one raised.
pub fn draw_vertical(buf: &mut Buffer, area: Rect, strip: &Strip) {
    let p = theme::current();
    buf.set_style(area, Style::default().fg(p.text).bg(p.bg));
    let n = strip.items.len();
    let nw = n.to_string().len().max(2);
    let width = area.width as usize;
    // Three rows a tab; the window keeps the active one in view.
    let fit = ((area.height as usize).saturating_sub(1) / 3).max(1);
    let active = strip.active.unwrap_or(0);
    let mut start = strip.start.get().min(n.saturating_sub(fit));
    if active < start {
        start = active;
    } else if active >= start + fit {
        start = active + 1 - fit;
    }
    strip.start.set(start);
    let row = |buf: &mut Buffer, y: u16, text: &str, style: Style| {
        if y < area.bottom() {
            let shown = take(text, width);
            let pad = width - shown.width();
            buf.set_stringn(
                area.x,
                y,
                format!("{shown}{}", " ".repeat(pad)),
                width,
                style,
            );
        }
    };
    for (k, i) in (start..n.min(start + fit)).enumerate() {
        let item = &strip.items[i];
        let selected = strip.active == Some(i);
        let y = area.y + 1 + 3 * k as u16;
        let bg = if selected { p.raised } else { p.bg };
        if selected {
            let edge = Style::default().fg(p.raised).bg(p.bg);
            row(buf, y - 1, &"▄".repeat(width), edge);
            if !item.new {
                row(buf, y + 2, &"▀".repeat(width), edge);
            }
        }
        let mut title = Style::default()
            .bg(bg)
            .fg(if selected { p.text } else { p.muted });
        if selected {
            title = title.add_modifier(Modifier::BOLD);
        }
        let label = indicator(item, i, strip.numbers);
        let label = format!("{}{label} ", " ".repeat(nw.saturating_sub(label.width())));
        row(buf, y, &format!("{label}{}", item.title), title);
        if y < area.bottom() {
            let mark = match (strip.numbers, item.status) {
                (
                    false,
                    Status {
                        attention: Some(_), ..
                    },
                ) => Some(p.accent),
                (false, Status { busy: true, .. }) => Some(p.primary),
                (
                    false,
                    Status {
                        unread: Some(Unread::Error),
                        ..
                    },
                ) => Some(p.error),
                (
                    false,
                    Status {
                        unread: Some(Unread::Activity),
                        ..
                    },
                ) => Some(p.accent),
                _ => None,
            };
            if let Some(c) = mark {
                buf.set_style(
                    Rect::new(area.x, y, (nw as u16).min(area.width), 1),
                    Style::default().fg(c),
                );
            }
        }
        if !item.new {
            let detail = format!("{}{}", " ".repeat(nw + 1), strip.detail);
            let muted = Style::default().bg(bg).fg(p.faint);
            row(buf, y + 1, &detail, muted);
        }
    }
}
