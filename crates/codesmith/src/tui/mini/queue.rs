//! Mini mode's pending prompts, as OpenCode's mini queue
//! (`mini/runtime.queue`, the footer's queued-prompt menu): a prompt sent
//! while a turn runs waits here instead of being refused, and the runner
//! sends the oldest once the session is idle. Nothing is written to the
//! scrollback until a prompt is sent. `ctrl+x q` lists them: `enter`
//! moves one to the front, `ctrl+d` deletes it, `ctrl+u` takes it back
//! into the draft.
//!
//! CodeSmith has no steering (a prompt joined to the running turn at its
//! next step), so every pending prompt is OpenCode's `queue` delivery. An
//! interrupted or failed turn holds the queue: `enter` on an empty prompt
//! lets it go again.

use crate::tui::app::App;
use crate::tui::attach::Pending;
use crate::tui::keymap;
use crate::tui::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// A prompt waiting for the turn to end.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// What is sent: the prompt with its paste chips expanded, a
    /// `/command`, or a `!shell` command.
    pub text: String,
    /// Its pasted files.
    pub files: Vec<Pending>,
}

/// The queue and its menu.
#[derive(Debug, Clone, Default)]
pub struct Queue {
    pub items: Vec<Queued>,
    /// The menu's selection while it is open.
    pub menu: Option<super::menu::Menu>,
    /// An interrupted or failed turn stopped the queue from running.
    pub held: bool,
}

const MENU_ROWS: usize = super::menu::ROWS;

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The next prompt to send, when the queue may run.
    pub fn next(&mut self) -> Option<Queued> {
        if self.held || self.items.is_empty() {
            return None;
        }
        let q = self.items.remove(0);
        self.settle();
        Some(q)
    }

    /// Hold the queue (a turn was interrupted or failed); true when there
    /// was something to hold.
    pub fn hold(&mut self) -> bool {
        self.held = !self.items.is_empty();
        self.held
    }

    /// Keep the menu's selection in range, closing it once empty.
    fn settle(&mut self) {
        if self.items.is_empty() {
            self.menu = None;
            self.held = false;
        } else if let Some(m) = &mut self.menu {
            m.reconcile(self.items.len(), MENU_ROWS);
        }
    }

    /// The menu's height: a title, its prompts, and the hint row.
    pub fn menu_rows(&self) -> u16 {
        if self.menu.is_none() {
            return 0;
        }
        (self.items.len().clamp(1, MENU_ROWS) + 2) as u16
    }
}

/// Whether a `/name` command sent while a turn runs waits for it: custom
/// and MCP commands and `/compact` (OpenCode's compaction barrier), not
/// the UI's own commands, which act at once.
pub fn queues_command(name: &str, args: &str) -> bool {
    match crate::tui::builtin(name, args) {
        Some(b) => b == "compact",
        None => true,
    }
}

/// One line of a prompt for the menu: control characters and newlines
/// shown as spaces.
fn summary(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl App {
    /// Queue `text` (a prompt sent while the turn runs) with the pasted
    /// files it names.
    pub fn enqueue(&mut self, text: String) {
        let files: Vec<Pending> = std::mem::take(&mut self.attachments)
            .into_iter()
            .filter(|a| text.contains(&a.label))
            .collect();
        self.pasted.clear();
        self.queue.items.push(Queued { text, files });
    }

    /// `prompt.queue` (`ctrl+x enter`): send the prompt, or queue it while
    /// a turn runs; the UI's own commands cannot wait.
    pub fn submit_queued(&mut self) -> Option<crate::tui::app::Action> {
        let text = self.input.text().trim();
        if let Some((name, args)) = codesmith_core::command::parse_invocation(text) {
            if !queues_command(&name, &args) {
                self.notice("this prompt cannot be queued");
                return None;
            }
        }
        self.submit()
    }

    /// Open the pending-prompt menu (`session.queued_prompts`).
    pub fn open_queue(&mut self) -> bool {
        if self.queue.is_empty() {
            return false;
        }
        self.popup = None;
        self.queue.menu = Some(Default::default());
        true
    }

    /// A key while the menu is open.
    pub fn queue_key(&mut self, k: KeyEvent) {
        let Some(mut menu) = self.queue.menu else {
            return;
        };
        let sel = menu.selected;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let found = keymap::stroke(&k)
            .map(|s| self.keymap.lookup(keymap::scope::QUEUE, &[s]).exact)
            .unwrap_or_default();
        let n = self.queue.len();
        match found.first().copied() {
            Some("queued_prompt.delete") => {
                self.queue.items.remove(sel);
            }
            Some("queued_prompt.undo") => {
                let q = self.queue.items.remove(sel);
                self.queue.menu = None;
                self.restore_prompt(&q.text);
                for f in q.files {
                    if !self.attachments.iter().any(|a| a.label == f.label) {
                        self.attachments.push(f);
                    }
                }
            }
            _ => match k.code {
                KeyCode::Up => {
                    menu.step(-1, n, MENU_ROWS);
                    self.queue.menu = Some(menu);
                }
                KeyCode::Down => {
                    menu.step(1, n, MENU_ROWS);
                    self.queue.menu = Some(menu);
                }
                KeyCode::Char('p') if ctrl => {
                    menu.step(-1, n, MENU_ROWS);
                    self.queue.menu = Some(menu);
                }
                KeyCode::Char('n') if ctrl => {
                    menu.step(1, n, MENU_ROWS);
                    self.queue.menu = Some(menu);
                }
                // Send this one next.
                KeyCode::Enter => {
                    let q = self.queue.items.remove(sel);
                    self.queue.items.insert(0, q);
                    self.queue.held = false;
                    self.queue.menu = None;
                }
                KeyCode::Esc => self.queue.menu = None,
                KeyCode::Char('c') if ctrl => self.queue.menu = None,
                _ => {}
            },
        }
        self.queue.settle();
    }
}

/// The status line's pending-prompt count, with the key that opens them.
pub fn status_action(app: &App) -> Option<super::width::Action> {
    let n = app.queue.len();
    if n == 0 {
        return None;
    }
    Some(super::width::Action {
        key: app.keymap.hint("session.queued_prompts"),
        label: format!("{n} pending"),
        expanded: app.queue.held.then(|| format!("{n} pending (held)")),
    })
}

/// Draw the menu in `area` (its [`Queue::menu_rows`] high).
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(menu) = app.queue.menu else {
        return;
    };
    let sel = menu.selected;
    let p = theme::current();
    let mono = app.mini.as_ref().is_some_and(|s| s.mono);
    let width = area.width as usize;
    let rows = (area.height as usize).saturating_sub(2).max(1);
    // The menu's own scroll, kept in view of the rows there are.
    let first = if sel < menu.offset || sel >= menu.offset + rows {
        (sel + 1).saturating_sub(rows)
    } else {
        menu.offset
    };
    let mut lines = vec![Line::from(Span::styled(
        "Pending prompts",
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    ))];
    for (i, q) in app.queue.items.iter().enumerate().skip(first).take(rows) {
        let mark = if i == sel {
            if mono {
                "> "
            } else {
                "\u{203a} "
            }
        } else {
            "  "
        };
        let tag = " queued";
        let room = width.saturating_sub(mark.len() + tag.len() + 1);
        let text = summary(&q.text);
        let shown = super::width::take_width(&text, room);
        let style = if i == sel {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark}{shown}"), style),
            Span::styled(tag, Style::default().fg(p.muted)),
        ]));
    }
    let k = &app.keymap;
    let sep = if mono { " - " } else { " \u{b7} " };
    let mut hint = vec!["enter next".to_string()];
    for (id, what) in [
        ("queued_prompt.delete", "delete"),
        ("queued_prompt.undo", "undo"),
    ] {
        let key = k.hint(id);
        if !key.is_empty() {
            hint.push(format!("{key} {what}"));
        }
    }
    hint.push("esc close".into());
    lines.push(Line::from(Span::styled(
        hint.join(sep),
        Style::default().fg(p.muted),
    )));
    f.render_widget(Paragraph::new(lines), area);
}
