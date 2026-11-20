//! The session's subagent and shell panel (OpenCode's composer: a
//! Subagents tab and a Shell tab), and the background command output
//! viewer (OpenCode's `DialogShellOutput`).
//!
//! The panel lists the session's subagents (running ones, or with ctrl+a
//! the others), nested ones under their parent as OpenCode's
//! `sessionFamily` draws them, and its background commands, one line each.
//! The viewer follows a command's output file: it reads what was added
//! since the last look, keeps the latest 64 KiB (from a whole character),
//! and shows it without terminal escapes.

use super::keymap::Keymap;
use super::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// The viewer keeps this much of a command's latest output, as OpenCode's.
pub const PAGE_BYTES: usize = 64 * 1024;
/// Most bytes read in one look (a big file is read on later frames).
const READ_MAX: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Subagents,
    Shell,
}

/// A subagent the session ran.
#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    pub id: String,
    /// The subagent that ran it, for a nested one.
    pub parent: Option<String>,
    pub agent: String,
    pub title: String,
    pub running: bool,
}

/// A background command of the session.
#[derive(Debug, Clone, PartialEq)]
pub struct Shell {
    pub id: String,
    pub command: String,
    pub file: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Panel {
    pub tab: Tab,
    pub sel: usize,
    /// Running subagents (the default), or the others.
    pub active: bool,
}

/// The sessions of `id`'s family below its root, depth first, each with
/// its tree prefix (OpenCode's `sessionFamily`): `├─ `, `└─ `, `│  `.
pub fn family<'a>(sessions: &[(&'a str, Option<&'a str>)], id: &str) -> Vec<(&'a str, String)> {
    let parent = |s: &str| sessions.iter().find(|(i, _)| *i == s).and_then(|(_, p)| *p);
    let mut root = id.to_string();
    let mut guard = 0;
    while let Some(p) = parent(&root) {
        root = p.to_string();
        guard += 1;
        if guard > sessions.len() {
            break;
        }
    }
    fn walk<'a>(
        sessions: &[(&'a str, Option<&'a str>)],
        of: &str,
        indent: &str,
        top: bool,
        out: &mut Vec<(&'a str, String)>,
        depth: usize,
    ) {
        if depth > 64 {
            return;
        }
        let kids: Vec<&'a str> = sessions
            .iter()
            .filter(|(_, p)| *p == Some(of))
            .map(|(i, _)| *i)
            .collect();
        for (n, k) in kids.iter().enumerate() {
            let last = n + 1 == kids.len();
            let (mark, more) = match (top, last) {
                (true, _) => (String::new(), String::new()),
                (false, false) => (format!("{indent}├─ "), format!("{indent}│  ")),
                (false, true) => (format!("{indent}└─ "), format!("{indent}   ")),
            };
            out.push((k, mark));
            walk(sessions, k, &more, false, out, depth + 1);
        }
    }
    let mut out = Vec::new();
    walk(sessions, &root, "", true, &mut out, 0);
    out
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// The subagent rows the panel shows: the filtered family, with prefixes.
pub fn subagent_rows(all: &[Subagent], active: bool) -> Vec<(&Subagent, String)> {
    let pairs: Vec<(&str, Option<&str>)> = all
        .iter()
        .map(|s| (s.id.as_str(), s.parent.as_deref().or(Some(""))))
        .collect();
    family(&pairs, "")
        .into_iter()
        .filter_map(|(id, prefix)| all.iter().find(|s| s.id == id).map(|s| (s, prefix)))
        .filter(|(s, _)| s.running == active)
        .collect()
}

impl Panel {
    pub fn lines(
        &self,
        subagents: &[Subagent],
        shells: &[Shell],
        width: usize,
        km: &Keymap,
    ) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let tab = |name: &'static str, on: bool| {
            if on {
                Span::styled(name, fg(p.dialog_text).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(name, fg(p.dialog_muted))
            }
        };
        let mut out = vec![
            Line::from(vec![
                tab("Subagents", self.tab == Tab::Subagents),
                Span::raw("   "),
                tab("Shell", self.tab == Tab::Shell),
            ]),
            Line::default(),
        ];
        let mut hints: Vec<(String, String)> = Vec::new();
        let row = |text: String, on: bool| {
            let mut t: String = text.chars().take(width.max(4)).collect();
            if t.chars().count() < text.chars().count() {
                t.pop();
                t.push('…');
            }
            if on {
                Line::from(Span::styled(
                    format!(" {t} "),
                    Style::default()
                        .fg(p.selected_fg)
                        .bg(p.selected_bg)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(format!(" {t} "), fg(p.dialog_text)))
            }
        };
        match self.tab {
            Tab::Subagents => {
                let rows = subagent_rows(subagents, self.active);
                if rows.is_empty() {
                    out.push(Line::from(Span::styled(
                        format!(
                            " No {} subagents",
                            if self.active { "active" } else { "inactive" }
                        ),
                        fg(p.dialog_muted),
                    )));
                }
                for (i, (s, prefix)) in rows.iter().enumerate() {
                    let state = if s.running { "running" } else { "done" };
                    out.push(row(
                        format!(
                            "{prefix}{}: {} · {state}",
                            safe(&titlecase(&s.agent)),
                            safe(&first_line(&s.title))
                        ),
                        i == self.sel,
                    ));
                }
                if rows.get(self.sel).is_some_and(|(s, _)| s.running) {
                    hints.push(("interrupt".into(), km.hint("composer.subagent.interrupt")));
                }
                hints.push((
                    format!("show {}", if self.active { "inactive" } else { "active" }),
                    "ctrl+a".into(),
                ));
            }
            Tab::Shell => {
                if shells.is_empty() {
                    out.push(Line::from(Span::styled(
                        " No shell commands",
                        fg(p.dialog_muted),
                    )));
                } else {
                    hints.push(("output".into(), km.hint("composer.shell.select")));
                    hints.push(("kill".into(), km.hint("composer.shell.kill")));
                }
                for (i, s) in shells.iter().enumerate() {
                    out.push(row(safe(&first_line(&s.command)), i == self.sel));
                }
            }
        }
        out.push(Line::default());
        let mut spans = vec![
            Span::styled("←→", fg(p.dialog_text)),
            Span::styled(" tab   ", fg(p.dialog_muted)),
        ];
        for (label, key) in hints.into_iter().filter(|(_, k)| !k.is_empty()) {
            spans.push(Span::styled(label, fg(p.dialog_text)));
            spans.push(Span::styled(format!(" {key}   "), fg(p.dialog_muted)));
        }
        spans.push(Span::styled("esc", fg(p.dialog_text)));
        spans.push(Span::styled(" close", fg(p.dialog_muted)));
        out.push(Line::from(spans));
        out
    }
}

fn first_line(s: &str) -> String {
    s.split('\n').next().unwrap_or("").to_string()
}

/// The output viewer of one background command.
#[derive(Debug, Clone)]
pub struct ShellView {
    pub id: String,
    pub command: String,
    pub file: PathBuf,
    /// `Running`, `Exited · code 0`, `Timed out`, `Killed`.
    pub status: String,
    /// The latest output, escapes taken out (at most [`PAGE_BYTES`]).
    pub text: String,
    /// Where the next read starts; `None` before the first.
    cursor: Option<u64>,
    /// Bytes of a character split by the end of the last read.
    partial: Vec<u8>,
    /// Earlier output was left out.
    pub omitted: bool,
    /// The first output row shown; `None` follows the end.
    pub top: Option<usize>,
    pub error: Option<String>,
    /// The file is gone: no more reads.
    pub missing: bool,
    /// What was copied last (`command` or `output`).
    pub copied: Option<&'static str>,
    /// The panel to go back to.
    pub back: Option<Panel>,
    /// The output's rows and the rows shown, as last drawn.
    pub shape: std::cell::Cell<(usize, usize)>,
}

/// The output's last `max` bytes, from a whole character.
pub fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    &s[cut..]
}

/// Terminal output as text: escape sequences out, `\r\n` and `\r` as line
/// breaks.
fn clean(s: &str) -> String {
    super::app::strip_escapes(s)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

impl ShellView {
    pub fn new(shell: &Shell, back: Option<Panel>) -> ShellView {
        ShellView {
            id: shell.id.clone(),
            command: shell.command.clone(),
            file: shell.file.clone(),
            status: "Running".into(),
            text: String::new(),
            cursor: None,
            partial: Vec::new(),
            omitted: false,
            top: None,
            error: None,
            missing: false,
            copied: None,
            back,
            shape: Default::default(),
        }
    }

    /// Read what the command added since the last look. Returns whether
    /// anything changed.
    pub fn refresh(&mut self) -> bool {
        if self.missing {
            return false;
        }
        match self.read() {
            Ok(changed) => {
                let was = self.error.take();
                changed || was.is_some()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.missing = true;
                self.error = Some("Shell output is no longer available.".into());
                true
            }
            Err(_) => {
                let was = self
                    .error
                    .replace("Unable to read shell output. Retrying…".into());
                was.as_deref() != self.error.as_deref()
            }
        }
    }

    fn read(&mut self) -> std::io::Result<bool> {
        let mut f = std::fs::File::open(&self.file)?;
        let size = f.metadata()?.len();
        let start = match self.cursor {
            Some(c) if c <= size => c,
            // A shorter file was rewritten: start over at its tail.
            _ => {
                let c = size.saturating_sub(PAGE_BYTES as u64);
                self.omitted = c > 0;
                self.partial.clear();
                c
            }
        };
        let first = self.cursor.is_none();
        let mut buf = Vec::new();
        f.seek(SeekFrom::Start(start))?;
        f.take(READ_MAX as u64).read_to_end(&mut buf)?;
        self.cursor = Some(start + buf.len() as u64);
        if buf.is_empty() && !first {
            return Ok(false);
        }
        let mut bytes = std::mem::take(&mut self.partial);
        bytes.extend_from_slice(&buf);
        // A tail read may start inside a character.
        if first && start > 0 {
            let skip = bytes.iter().take_while(|b| (**b & 0xc0) == 0x80).count();
            bytes.drain(..skip);
        }
        let valid = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => bytes.len(),
        };
        self.partial = bytes.split_off(valid);
        let mut next = std::mem::take(&mut self.text);
        next.push_str(&clean(&String::from_utf8_lossy(&bytes)));
        if next.len() > PAGE_BYTES {
            self.omitted = true;
        }
        self.text = tail(&next, PAGE_BYTES).to_string();
        Ok(true)
    }

    /// Rows of output the viewer shows on a `rows`-row terminal.
    pub fn height(rows: u16) -> usize {
        ((rows as usize * 6) / 10).saturating_sub(6).max(3)
    }

    pub fn scroll(&mut self, by: isize) {
        let (total, shown) = self.shape.get();
        let max = total.saturating_sub(shown);
        let now = self.top.unwrap_or(max) as isize;
        let next = (now + by).clamp(0, max as isize) as usize;
        self.top = (next < max).then_some(next);
    }

    pub fn lines(&self, width: usize, rows: u16) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let mut out: Vec<Line<'static>> = Vec::new();
        let command = super::view::display_safe(&self.command);
        for l in command.split('\n').take(3) {
            out.push(Line::from(Span::styled(l.to_string(), fg(p.dialog_muted))));
        }
        out.push(Line::default());
        if self.omitted {
            out.push(Line::from(Span::styled(
                "Earlier output omitted · showing recent output",
                fg(p.dialog_muted),
            )));
            out.push(Line::default());
        }
        let height = Self::height(rows);
        let body: Vec<String> = if self.text.is_empty() {
            let msg = if self.cursor.is_none() {
                "Loading output…"
            } else {
                "No captured output. Output redirected to files is not shown here."
            };
            vec![msg.to_string()]
        } else {
            let shown = visible(&self.text);
            shown
                .split('\n')
                .flat_map(|l| {
                    if l.is_empty() {
                        vec![String::new()]
                    } else {
                        textwrap::wrap(l, width.max(1))
                            .into_iter()
                            .map(|c| c.into_owned())
                            .collect()
                    }
                })
                .collect()
        };
        // A final line break leaves no row of its own.
        let total =
            body.len() - usize::from(body.len() > 1 && body.last().is_some_and(|l| l.is_empty()));
        self.shape.set((total, height));
        let max = total.saturating_sub(height);
        let top = self.top.unwrap_or(max).min(max);
        for l in body.into_iter().take(total).skip(top).take(height) {
            out.push(Line::from(Span::styled(l, fg(p.dialog_text))));
        }
        for _ in total.saturating_sub(top).min(height)..height {
            out.push(Line::default());
        }
        if let Some(e) = &self.error {
            out.push(Line::from(Span::styled(e.clone(), fg(p.error))));
        }
        out.push(Line::default());
        let action = |what: &'static str, key: &str| -> Vec<Span<'static>> {
            if self.copied.is_some_and(|c| what.ends_with(c)) {
                vec![Span::styled("✓ copied   ", fg(p.success))]
            } else {
                vec![
                    Span::styled(what, fg(p.dialog_text)),
                    Span::styled(format!(" {key}   "), fg(p.dialog_muted)),
                ]
            }
        };
        let mut footer = action("follow", "end");
        footer.extend(action("copy command", "c"));
        footer.extend(action("copy output", "o"));
        out.push(Line::from(footer));
        out
    }
}

/// Output as shown: tabs as spaces, other control and invisible
/// characters written visibly (the text is the command's, so untrusted).
fn visible(s: &str) -> String {
    super::view::escape_controls(&s.replace('\t', "    "))
}

/// A finished job's status, as OpenCode words a shell's.
pub fn status(job: Option<&codesmith_core::tool::shell::Job>) -> String {
    use codesmith_core::tool::shell::Ending;
    let Some(j) = job else {
        return "Exited".into();
    };
    if j.cancelled {
        return "Killed".into();
    }
    match &j.ending {
        None => "Running".into(),
        Some(Ending::Exited(c)) => format!("Exited · code {c}"),
        Some(Ending::Signaled(_)) => "Killed".into(),
        Some(Ending::TimedOut(_)) => "Timed out".into(),
        Some(Ending::Lost) => "Exited".into(),
    }
}
