//! UI state and input handling. No terminal I/O here: keys and session
//! events go in, state changes and `Action`s for the runner come out.

use super::bridge::{Answers, Ev};
use super::complete::{self, Kind, Popup};
use super::diff::Diff;
use super::editor::Editor;
use codesmith_ai::Usage;
use codesmith_core::config::Cost;
use codesmith_core::permission::{Reply, Request};
use codesmith_core::question::{interpret, Prompt};
use codesmith_core::tool::todo::Todo;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool(Tool),
    Notice(String),
    Error(String),
    Todos(Vec<Todo>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub detail: String,
    /// The first line of the result and whether it failed; `None` while running.
    pub result: Option<(String, bool)>,
    /// The whole result.
    pub output: String,
    pub diff: Option<Diff>,
}

impl Tool {
    /// The tool's own name, without a subagent prefix ("explore › read").
    pub fn base(&self) -> &str {
        self.name.rsplit(" › ").next().unwrap_or(&self.name)
    }
}

/// What a picker chooses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Session,
    Model,
    Agent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    /// Marks the current choice.
    pub current: bool,
}

pub struct Picker {
    pub kind: Pick,
    pub title: String,
    pub items: Vec<PickItem>,
    pub filter: String,
    /// Indexes into `items` matching the filter, best first.
    pub shown: Vec<usize>,
    /// Position in `shown`.
    pub sel: usize,
}

impl Picker {
    pub fn new(kind: Pick, title: &str, items: Vec<PickItem>) -> Self {
        let mut p = Picker {
            kind,
            title: title.into(),
            items,
            filter: String::new(),
            shown: vec![],
            sel: 0,
        };
        p.refilter();
        p.sel = p
            .shown
            .iter()
            .position(|&i| p.items[i].current)
            .unwrap_or(0);
        p
    }

    fn refilter(&mut self) {
        let keys: Vec<String> = self
            .items
            .iter()
            .map(|i| format!("{} {}", i.label, i.detail))
            .collect();
        self.shown = complete::filter(&keys, &self.filter, |k| k.as_str(), false, usize::MAX);
        self.sel = 0;
    }

    pub fn selected(&self) -> Option<&PickItem> {
        self.shown.get(self.sel).map(|&i| &self.items[i])
    }
}

pub enum Modal {
    Permission {
        req: Request,
        reply: Option<oneshot::Sender<Reply>>,
        feedback: String,
        /// The edit being asked about, when known.
        diff: Option<Diff>,
        /// First row of the request text shown when it is too long to fit
        /// (the view clamps it).
        scroll: std::cell::Cell<usize>,
    },
    Question {
        prompts: Vec<Prompt>,
        idx: usize,
        answers: Vec<Vec<String>>,
        input: String,
        reply: Option<oneshot::Sender<Answers>>,
    },
    Picker(Picker),
    Help,
}

/// What the runner should do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Submit(String),
    Slash {
        name: String,
        args: String,
    },
    Interrupt,
    Quit,
    Picked(Pick, String),
    /// Switch to the next (+1) or previous (-1) primary agent.
    CycleAgent(i32),
}

#[derive(Default)]
pub struct Header {
    pub agent: String,
    pub model: String,
    pub session: String,
    pub title: Option<String>,
    /// The model's context window, when known.
    pub context_limit: Option<u64>,
    /// The model's price, when configured.
    pub cost: Option<Cost>,
}

#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: Editor,
    pub history: Vec<String>,
    hist: Option<usize>,
    /// Lines scrolled up from the bottom of the transcript.
    pub scroll: usize,
    pub busy: bool,
    pub header: Header,
    pub usage: Usage,
    /// Tokens the last model call used (prompt plus reply): the context in use.
    pub context: u64,
    pub modal: Option<Modal>,
    pub show_reasoning: bool,
    /// Show tool output and diffs in full rather than collapsed.
    pub expand_tools: bool,
    pub quit: bool,
    /// Slash commands offered by autocomplete.
    pub commands: Vec<complete::Item>,
    /// Project files offered for `@` mentions.
    pub files: Vec<String>,
    pub popup: Option<Popup>,
    /// The input text when the popup was dismissed with Esc (it stays closed
    /// until the text changes).
    dismissed: Option<String>,
    /// Ctrl+X was pressed: the next key is a leader command.
    leader: bool,
    /// Pasted image and PDF paths, attached on submit while their
    /// placeholder is still in the prompt.
    pub attachments: Vec<super::attach::Pending>,
    /// Where pasted relative paths resolve.
    pub cwd: std::path::PathBuf,
    /// The session's background commands still running.
    pub background: usize,
}

pub const HELP: &str = "Enter send · Shift/Alt+Enter or Ctrl+J newline · ↑/↓ lines, then history
←/→ Home/End, Ctrl/Alt+←/→ words · Ctrl+A/E line start/end · Ctrl+W/K/U delete
Tab next agent (Shift+Tab previous) · Ctrl+O expand tool output · Ctrl+T reasoning
Ctrl+X then m models · a agents · l sessions · n new session
/ commands and @ files complete as you type (Tab or Enter to accept)
Esc or Ctrl+C interrupt a running turn · Ctrl+C on an empty prompt quits
PgUp/PgDn scroll · Ctrl+End follow

/new            start a new session
/sessions       pick a session to continue
/undo, /redo    undo or redo the latest turn's file changes
/agent [id]     switch agent (build, plan, …)
/model [p/m]    switch model (p/m#variant picks a variant)
/variant [name] cycle the model's variants (reasoning effort), or pick one
/compact        summarize older history now
/clear          clear the screen (the session is kept)
/help           this help
/exit           quit
/calibrate      survey the codebase and choose review dimensions
/review [what]  review changes (uncommitted, a commit, a..b, a branch, #pr)
/<command>      run a custom or built-in command (/init, …)";

/// Built-in slash commands for autocomplete: (name, description).
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("new", "start a new session"),
    ("sessions", "pick a session to continue"),
    ("undo", "undo the latest turn's file changes"),
    ("redo", "redo the latest undone turn"),
    ("agent", "switch agent"),
    ("model", "switch model"),
    (
        "variant",
        "cycle or pick the model variant (reasoning effort)",
    ),
    ("compact", "summarize older history now"),
    ("clear", "clear the screen"),
    ("thinking", "show or hide reasoning"),
    ("help", "keys and commands"),
    ("exit", "quit"),
];

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

impl App {
    pub fn new() -> Self {
        let mut a = App::default();
        a.set_commands(&[]);
        a
    }

    /// Set the autocomplete commands: the built-ins plus `extra`.
    pub fn set_commands(&mut self, extra: &[(String, String)]) {
        let mut cmds: Vec<complete::Item> = BUILTIN_COMMANDS
            .iter()
            .map(|(n, d)| complete::Item {
                label: n.to_string(),
                detail: d.to_string(),
            })
            .collect();
        for (n, d) in extra {
            if !cmds.iter().any(|c| &c.label == n) {
                cmds.push(complete::Item {
                    label: n.clone(),
                    detail: d.clone(),
                });
            }
        }
        self.commands = cmds;
    }

    pub fn notice(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Notice(s.into()));
    }

    pub fn error(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Error(s.into()));
    }

    /// The share of the context window in use, as a percentage.
    pub fn context_percent(&self) -> Option<u64> {
        let limit = self.header.context_limit.filter(|l| *l > 0)?;
        (self.context > 0).then(|| (self.context * 100).div_ceil(limit).min(100))
    }

    /// The session's cost so far in dollars, when the model is priced.
    pub fn cost(&self) -> Option<f64> {
        self.header
            .cost
            .map(|c| codesmith_core::stats::price(&self.usage, &c))
    }

    /// Apply a session event.
    pub fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Text(d) => match self.entries.last_mut() {
                Some(Entry::Assistant(t)) => t.push_str(&d),
                _ => self.entries.push(Entry::Assistant(d)),
            },
            Ev::Reasoning(d) => match self.entries.last_mut() {
                Some(Entry::Reasoning(t)) => t.push_str(&d),
                _ => self.entries.push(Entry::Reasoning(d)),
            },
            Ev::ToolStart {
                id,
                name,
                detail,
                diff,
            } => self.entries.push(Entry::Tool(Tool {
                id,
                name,
                detail,
                diff,
                ..Default::default()
            })),
            Ev::ToolEnd {
                id,
                name,
                output,
                error,
            } => {
                if let Some(Entry::Tool(t)) = self.entries.iter_mut().rev().find(|e| {
                    matches!(e, Entry::Tool(t) if t.result.is_none()
                        && (t.id == id || (id.is_empty() && t.name == name)))
                }) {
                    t.result = Some((first_line(&output), error));
                    if error {
                        // A failed edit changed nothing.
                        t.diff = None;
                    }
                    t.output = output;
                }
            }
            Ev::Todos(t) => {
                self.entries.retain(|e| !matches!(e, Entry::Todos(_)));
                self.entries.push(Entry::Todos(t));
            }
            Ev::Notice(n) => self.notice(n),
            Ev::Compacted(b, a) => {
                self.context = a;
                self.notice(format!("compacted context: ~{b} → ~{a} tokens"))
            }
            Ev::StepEnd(u) => {
                self.usage.input += u.input;
                self.usage.output += u.output;
                self.usage.cache_read += u.cache_read;
                self.usage.cache_write += u.cache_write;
                self.context = u.input + u.cache_read + u.cache_write + u.output;
            }
            Ev::Permission(req, reply) => {
                // The asking tool is the latest one still running.
                let diff = if req.action == "edit" {
                    self.entries.iter().rev().find_map(|e| match e {
                        Entry::Tool(t) if t.result.is_none() => Some(t.diff.clone()),
                        _ => None,
                    })
                } else {
                    None
                }
                .flatten();
                self.modal = Some(Modal::Permission {
                    req,
                    reply: Some(reply),
                    feedback: String::new(),
                    diff,
                    scroll: Default::default(),
                });
            }
            Ev::Question(prompts, reply) => {
                self.modal = Some(Modal::Question {
                    prompts,
                    idx: 0,
                    answers: vec![],
                    input: String::new(),
                    reply: Some(reply),
                });
            }
            Ev::Key(_)
            | Ev::Paste(_)
            | Ev::Resize
            | Ev::Files(_)
            | Ev::Done { .. }
            | Ev::Stopped
            | Ev::Background => {}
        }
    }

    /// Cancel an open prompt (the session sees a rejection or dismissal).
    pub fn cancel_modal(&mut self) {
        match self.modal.take() {
            Some(Modal::Permission { reply: Some(r), .. }) => {
                let _ = r.send(Reply::Reject(None));
            }
            Some(Modal::Question { reply: Some(r), .. }) => {
                let _ = r.send(None);
            }
            _ => {}
        }
    }

    pub fn on_paste(&mut self, s: &str) {
        match &mut self.modal {
            Some(Modal::Permission { feedback, .. }) => {
                feedback.push_str(s.lines().next().unwrap_or(""))
            }
            Some(Modal::Question { input, .. }) => input.push_str(s.lines().next().unwrap_or("")),
            Some(Modal::Picker(p)) => {
                p.filter.push_str(s.lines().next().unwrap_or(""));
                p.refilter();
            }
            Some(Modal::Help) => {}
            None => {
                if let Some(files) = super::attach::pasted_files(s, &self.cwd) {
                    for path in files {
                        let label = super::attach::label(&self.attachments, &path);
                        self.input.insert_str(&format!("{label} "));
                        self.attachments
                            .push(super::attach::Pending { label, path });
                    }
                } else {
                    self.input
                        .insert_str(&s.replace("\r\n", "\n").replace('\r', "\n"));
                }
                self.edited();
            }
        }
    }

    /// The project file list arrived (it is listed in the background).
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = files;
        if self.popup.is_some() {
            self.refresh_popup();
        }
    }

    /// Recompute the autocomplete popup after the input changed.
    fn edited(&mut self) {
        self.hist = None;
        self.refresh_popup();
    }

    fn refresh_popup(&mut self) {
        if self.dismissed.as_deref() == Some(self.input.text()) {
            self.popup = None;
            return;
        }
        self.dismissed = None;
        self.popup = complete::popup(
            self.input.text(),
            self.input.cursor(),
            &self.commands,
            &self.files,
        );
    }

    /// Accept the highlighted completion. `run` submits a slash command.
    fn accept(&mut self, run: bool) -> Option<Action> {
        let p = self.popup.take()?;
        let item = p.items.get(p.sel)?.clone();
        match p.kind {
            Kind::Slash => {
                if run {
                    self.input.set(format!("/{}", item.label));
                    return self.submit();
                }
                self.input
                    .replace_before_cursor(p.start, &format!("/{} ", item.label));
            }
            Kind::File => {
                // A directory stays open for drilling down.
                let sep = if item.label.ends_with('/') { "" } else { " " };
                self.input
                    .replace_before_cursor(p.start, &format!("@{}{sep}", item.label));
            }
        }
        self.edited();
        None
    }

    fn popup_key(&mut self, k: &KeyEvent, ctrl: bool) -> Option<Option<Action>> {
        let p = self.popup.as_mut()?;
        let n = p.items.len();
        match k.code {
            KeyCode::Up => p.sel = (p.sel + n - 1) % n,
            KeyCode::Char('p') if ctrl => p.sel = (p.sel + n - 1) % n,
            KeyCode::Down => p.sel = (p.sel + 1) % n,
            KeyCode::Char('n') if ctrl => p.sel = (p.sel + 1) % n,
            KeyCode::Tab => return Some(self.accept(false)),
            KeyCode::Enter if k.modifiers.is_empty() => {
                let run = p.kind == Kind::Slash;
                return Some(self.accept(run));
            }
            KeyCode::Esc => {
                self.popup = None;
                self.dismissed = Some(self.input.text().to_string());
            }
            _ => return None,
        }
        Some(None)
    }

    fn leader_key(&mut self, k: &KeyEvent) -> Option<Action> {
        self.leader = false;
        let cmd = match k.code {
            KeyCode::Char('m') => "model",
            KeyCode::Char('a') => "agent",
            KeyCode::Char('l') => "sessions",
            KeyCode::Char('n') => "new",
            KeyCode::Char('h') => "help",
            KeyCode::Char('c') => "compact",
            KeyCode::Char('u') => "undo",
            KeyCode::Char('r') => "redo",
            _ => return None,
        };
        Some(Action::Slash {
            name: cmd.into(),
            args: String::new(),
        })
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if self.modal.is_some() {
            return self.modal_key(k, ctrl);
        }
        if self.leader {
            return self.leader_key(&k);
        }
        if let Some(a) = self.popup_key(&k, ctrl) {
            return a;
        }
        let ed = &mut self.input;
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    return Some(Action::Interrupt);
                }
                if ed.is_empty() {
                    self.quit = true;
                    return Some(Action::Quit);
                }
                ed.clear();
            }
            KeyCode::Char('d') if ctrl && ed.is_empty() => {
                self.quit = true;
                return Some(Action::Quit);
            }
            KeyCode::Char('x') if ctrl => {
                self.leader = true;
                return None;
            }
            KeyCode::Char('t') if ctrl => {
                self.show_reasoning = !self.show_reasoning;
                return None;
            }
            KeyCode::Char('o') if ctrl => {
                self.expand_tools = !self.expand_tools;
                return None;
            }
            KeyCode::Char('u') if ctrl => ed.clear(),
            KeyCode::Char('w') if ctrl => ed.delete_word_back(),
            KeyCode::Char('k') if ctrl => ed.kill_to_end(),
            KeyCode::Char('a') if ctrl => ed.home(),
            KeyCode::Char('e') if ctrl => ed.end(),
            KeyCode::Char('b') if ctrl => ed.left(),
            KeyCode::Char('f') if ctrl => ed.right(),
            KeyCode::Char('d') if ctrl => ed.delete(),
            KeyCode::Char('b') if alt => ed.word_left(),
            KeyCode::Char('f') if alt => ed.word_right(),
            KeyCode::Char('d') if alt => ed.delete_word_forward(),
            KeyCode::Char('j') if ctrl => ed.insert('\n'),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => ed.insert('\n'),
            KeyCode::Enter => return self.submit(),
            KeyCode::Esc => {
                if self.busy {
                    return Some(Action::Interrupt);
                }
                ed.clear();
            }
            KeyCode::Backspace if alt || ctrl => ed.delete_word_back(),
            KeyCode::Backspace => ed.backspace(),
            KeyCode::Delete => ed.delete(),
            KeyCode::Left if alt || ctrl => ed.word_left(),
            KeyCode::Right if alt || ctrl => ed.word_right(),
            KeyCode::Left => ed.left(),
            KeyCode::Right => ed.right(),
            KeyCode::End if ctrl => {
                self.scroll = 0;
                return None;
            }
            KeyCode::Home => ed.home(),
            KeyCode::End => ed.end(),
            KeyCode::Up => {
                if !ed.up() {
                    self.history_step(-1);
                }
                self.popup = None;
                return None;
            }
            KeyCode::Down => {
                if !ed.down() {
                    self.history_step(1);
                }
                self.popup = None;
                return None;
            }
            KeyCode::PageUp => {
                self.scroll += 10;
                return None;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                return None;
            }
            KeyCode::Tab => return Some(Action::CycleAgent(1)),
            KeyCode::BackTab => return Some(Action::CycleAgent(-1)),
            KeyCode::Char(c) => ed.insert(c),
            _ => return None,
        }
        self.edited();
        None
    }

    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.hist, dir) {
            (None, -1) => Some(self.history.len() - 1),
            (None, _) => return,
            (Some(i), -1) => Some(i.saturating_sub(1)),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.hist = next;
        self.input
            .set(next.map(|i| self.history[i].clone()).unwrap_or_default());
    }

    fn submit(&mut self) -> Option<Action> {
        let text = self.input.text().trim().to_string();
        self.popup = None;
        if text.is_empty() {
            return None;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.hist = None;
        self.input.clear();
        self.scroll = 0;
        if let Some((name, args)) = codesmith_core::command::parse_invocation(&text) {
            return Some(Action::Slash { name, args });
        }
        if self.busy {
            self.notice("A turn is still running. Press Esc to interrupt it first.");
            self.input.set(text);
            return None;
        }
        Some(Action::Submit(text))
    }

    fn modal_key(&mut self, k: KeyEvent, ctrl: bool) -> Option<Action> {
        if ctrl && k.code == KeyCode::Char('c') {
            let was_prompt = matches!(
                self.modal,
                Some(Modal::Permission { .. } | Modal::Question { .. })
            );
            self.cancel_modal();
            return if self.busy && was_prompt {
                Some(Action::Interrupt)
            } else {
                None
            };
        }
        match self.modal.as_mut().unwrap() {
            Modal::Permission {
                feedback,
                reply,
                scroll,
                ..
            } => {
                let answer = match k.code {
                    KeyCode::Char('y') if feedback.is_empty() => Some(Reply::Once),
                    KeyCode::Char('a') if feedback.is_empty() => Some(Reply::Always),
                    KeyCode::Char('n') if feedback.is_empty() => Some(Reply::Reject(None)),
                    KeyCode::Esc => Some(Reply::Reject(None)),
                    // Allowing takes a deliberate y or a: a stray Enter rejects.
                    KeyCode::Enter if feedback.trim().is_empty() => Some(Reply::Reject(None)),
                    KeyCode::Enter => Some(Reply::Reject(Some(feedback.trim().to_string()))),
                    KeyCode::Up => {
                        scroll.set(scroll.get().saturating_sub(1));
                        None
                    }
                    KeyCode::Down => {
                        scroll.set(scroll.get() + 1);
                        None
                    }
                    KeyCode::PageUp => {
                        scroll.set(scroll.get().saturating_sub(10));
                        None
                    }
                    KeyCode::PageDown => {
                        scroll.set(scroll.get() + 10);
                        None
                    }
                    KeyCode::Backspace => {
                        feedback.pop();
                        None
                    }
                    KeyCode::Char(c) => {
                        feedback.push(c);
                        None
                    }
                    _ => None,
                };
                if let Some(a) = answer {
                    if let Some(r) = reply.take() {
                        let _ = r.send(a);
                    }
                    self.modal = None;
                }
            }
            Modal::Question {
                prompts,
                idx,
                answers,
                input,
                reply,
            } => match k.code {
                KeyCode::Esc => self.cancel_modal(),
                KeyCode::Enter => {
                    answers.push(interpret(input, &prompts[*idx]));
                    input.clear();
                    *idx += 1;
                    if *idx >= prompts.len() {
                        if let Some(r) = reply.take() {
                            let _ = r.send(Some(std::mem::take(answers)));
                        }
                        self.modal = None;
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
            Modal::Picker(p) => match k.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                KeyCode::Char('p') if ctrl => p.sel = p.sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab if p.sel + 1 < p.shown.len() => p.sel += 1,
                KeyCode::Char('n') if ctrl && p.sel + 1 < p.shown.len() => p.sel += 1,
                KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
                KeyCode::PageDown => p.sel = (p.sel + 10).min(p.shown.len().saturating_sub(1)),
                KeyCode::Enter => {
                    let picked = p.selected().map(|i| (p.kind, i.id.clone()));
                    if picked.is_some() {
                        self.modal = None;
                    }
                    return picked.map(|(k, id)| Action::Picked(k, id));
                }
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.refilter();
                }
                KeyCode::Char(c) if !ctrl => {
                    p.filter.push(c);
                    p.refilter();
                }
                _ => {}
            },
            Modal::Help => self.modal = None,
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }
    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            modifiers,
            ..key(code)
        }
    }
    fn ctrl(c: char) -> KeyEvent {
        with(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typ(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_submitting() {
        let mut a = App::new();
        typ(&mut a, "hello");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Submit("hello".into()))
        );
        assert!(a.input.is_empty());
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            None,
            "empty input does nothing"
        );
    }

    #[test]
    fn newlines_and_editing() {
        let mut a = App::new();
        typ(&mut a, "ab");
        a.on_key(ctrl('j'));
        typ(&mut a, "c");
        a.on_key(key(KeyCode::Backspace));
        assert_eq!(a.input.text(), "ab\n");
        a.on_key(ctrl('u'));
        assert!(a.input.is_empty());
    }

    #[test]
    fn editing_in_the_middle() {
        let mut a = App::new();
        typ(&mut a, "fix the bug");
        a.on_key(with(KeyCode::Left, KeyModifiers::ALT));
        a.on_key(with(KeyCode::Left, KeyModifiers::CONTROL));
        typ(&mut a, "nasty ");
        assert_eq!(a.input.text(), "fix nasty the bug");
        a.on_key(ctrl('a'));
        a.on_key(key(KeyCode::Delete));
        typ(&mut a, "F");
        a.on_key(ctrl('e'));
        a.on_key(ctrl('w'));
        assert_eq!(a.input.text(), "Fix nasty the ");
        a.on_key(key(KeyCode::Home));
        a.on_key(with(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(a.input.text(), "\nFix nasty the ");
    }

    #[test]
    fn up_moves_within_text_before_history() {
        let mut a = App::new();
        typ(&mut a, "old");
        a.on_key(key(KeyCode::Enter));
        typ(&mut a, "line1");
        a.on_key(ctrl('j'));
        typ(&mut a, "l2");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.cursor(), 2, "moved up within the text");
        assert_eq!(a.input.text(), "line1\nl2");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "old", "then history");
    }

    #[test]
    fn slash_commands() {
        let mut a = App::new();
        typ(&mut a, "/review HEAD~1");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Slash {
                name: "review".into(),
                args: "HEAD~1".into()
            })
        );
    }

    #[test]
    fn slash_autocomplete() {
        let mut a = App::new();
        a.set_commands(&[("review".into(), "review changes".into())]);
        typ(&mut a, "/ses");
        let p = a.popup.as_ref().expect("popup opens");
        assert_eq!(p.items[0].label, "sessions");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Slash {
                name: "sessions".into(),
                args: String::new()
            }),
            "Enter runs the highlighted command"
        );
        typ(&mut a, "/revi");
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.input.text(), "/review ", "Tab completes for arguments");
        assert!(a.popup.is_none());
        a.on_key(ctrl('u'));
        typ(&mut a, "/zzzz");
        assert!(a.popup.is_none(), "no match, no popup");
    }

    #[test]
    fn popup_navigation_and_dismissal() {
        let mut a = App::new();
        typ(&mut a, "/");
        let n = a.popup.as_ref().unwrap().items.len();
        assert!(n > 3);
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.popup.as_ref().unwrap().sel, 2);
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.popup.as_ref().unwrap().sel, n - 1, "wraps around");
        a.on_key(key(KeyCode::Esc));
        assert!(a.popup.is_none());
        assert_eq!(a.input.text(), "/", "Esc only closes the popup");
        typ(&mut a, "n");
        assert!(a.popup.is_some(), "typing reopens it");
    }

    #[test]
    fn file_mentions_complete() {
        let mut a = App::new();
        a.files = vec!["src/".into(), "src/main.rs".into(), "README.md".into()];
        typ(&mut a, "look at @mai");
        assert_eq!(a.popup.as_ref().unwrap().items[0].label, "src/main.rs");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.input.text(), "look at @src/main.rs ");
        typ(&mut a, "and @sr");
        a.on_key(key(KeyCode::Tab));
        assert_eq!(
            a.input.text(),
            "look at @src/main.rs and @src/",
            "directories stay open"
        );
        assert!(a.popup.is_some());
    }

    #[test]
    fn tab_cycles_agents_and_leader_keys() {
        let mut a = App::new();
        assert_eq!(a.on_key(key(KeyCode::Tab)), Some(Action::CycleAgent(1)));
        assert_eq!(
            a.on_key(key(KeyCode::BackTab)),
            Some(Action::CycleAgent(-1))
        );
        assert_eq!(a.on_key(ctrl('x')), None);
        assert_eq!(
            a.on_key(key(KeyCode::Char('m'))),
            Some(Action::Slash {
                name: "model".into(),
                args: String::new()
            })
        );
        typ(&mut a, "m");
        assert_eq!(a.input.text(), "m", "the leader is used once");
    }

    #[test]
    fn history() {
        let mut a = App::new();
        for m in ["one", "two"] {
            typ(&mut a, m);
            a.on_key(key(KeyCode::Enter));
        }
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "two");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "one");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input.text(), "two");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input.text(), "");
    }

    #[test]
    fn busy_keys() {
        let mut a = App::new();
        a.busy = true;
        assert_eq!(a.on_key(key(KeyCode::Esc)), Some(Action::Interrupt));
        assert_eq!(a.on_key(ctrl('c')), Some(Action::Interrupt));
        typ(&mut a, "more");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            None,
            "no new turn while one runs"
        );
        assert_eq!(a.input.text(), "more", "the text is kept");
    }

    #[test]
    fn quitting() {
        let mut a = App::new();
        typ(&mut a, "x");
        assert_eq!(a.on_key(ctrl('c')), None, "first Ctrl+C clears the input");
        assert_eq!(a.on_key(ctrl('c')), Some(Action::Quit));
        assert!(a.quit);
    }

    fn start(id: &str, name: &str, detail: &str) -> Ev {
        Ev::ToolStart {
            id: id.into(),
            name: name.into(),
            detail: detail.into(),
            diff: None,
        }
    }

    #[test]
    fn streaming_events_build_the_transcript() {
        let mut a = App::new();
        a.on_event(Ev::Text("Hel".into()));
        a.on_event(Ev::Text("lo".into()));
        a.on_event(start("c1", "read", "a.rs"));
        a.on_event(Ev::ToolEnd {
            id: "c1".into(),
            name: "read".into(),
            output: "\nRead file a.rs, lines 1-3\n1: x".into(),
            error: false,
        });
        a.on_event(Ev::Text("Done".into()));
        assert_eq!(
            a.entries,
            vec![
                Entry::Assistant("Hello".into()),
                Entry::Tool(Tool {
                    id: "c1".into(),
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-3".into(), false)),
                    output: "\nRead file a.rs, lines 1-3\n1: x".into(),
                    diff: None,
                }),
                Entry::Assistant("Done".into()),
            ]
        );
    }

    #[test]
    fn todos_replace_the_previous_list() {
        let mut a = App::new();
        a.on_event(Ev::Todos(vec![]));
        a.on_event(Ev::Text("x".into()));
        a.on_event(Ev::Todos(vec![]));
        assert_eq!(
            a.entries
                .iter()
                .filter(|e| matches!(e, Entry::Todos(_)))
                .count(),
            1
        );
    }

    fn req() -> Request {
        Request {
            action: "shell".into(),
            resources: vec!["rm x".into()],
            save: vec!["rm *".into()],
            title: "$ rm x".into(),
        }
    }

    #[test]
    fn permission_answers() {
        for (keys, want) in [
            (vec![key(KeyCode::Char('y'))], Reply::Once),
            (vec![key(KeyCode::Char('a'))], Reply::Always),
            (vec![key(KeyCode::Char('n'))], Reply::Reject(None)),
            // An empty Enter rejects (it used to allow once).
            (vec![key(KeyCode::Enter)], Reply::Reject(None)),
            (vec![key(KeyCode::Esc)], Reply::Reject(None)),
            (
                vec![
                    key(KeyCode::Down),
                    key(KeyCode::Up),
                    key(KeyCode::Char('y')),
                ],
                Reply::Once,
            ),
            (
                vec![
                    key(KeyCode::Char('u')),
                    key(KeyCode::Char('s')),
                    key(KeyCode::Char('e')),
                    key(KeyCode::Enter),
                ],
                Reply::Reject(Some("use".into())),
            ),
        ] {
            let mut a = App::new();
            let (tx, mut rx) = oneshot::channel();
            a.on_event(Ev::Permission(req(), tx));
            for k in keys {
                a.on_key(k);
            }
            assert!(a.modal.is_none());
            assert_eq!(rx.try_recv().unwrap(), want);
        }
    }

    #[test]
    fn edit_permissions_carry_the_running_tools_diff() {
        let mut a = App::new();
        let d = super::super::diff::diff("a\n", "b\n", 1);
        a.on_event(Ev::ToolStart {
            id: "c".into(),
            name: "edit".into(),
            detail: "x.rs".into(),
            diff: Some(d.clone()),
        });
        let (tx, _rx) = oneshot::channel();
        a.on_event(Ev::Permission(
            Request {
                action: "edit".into(),
                resources: vec!["x.rs".into()],
                save: vec![],
                title: "Edit x.rs".into(),
            },
            tx,
        ));
        assert!(matches!(&a.modal, Some(Modal::Permission { diff: Some(x), .. }) if *x == d));
    }

    #[test]
    fn questions_are_answered_in_order() {
        let p = |q: &str| Prompt {
            question: q.into(),
            header: String::new(),
            options: vec![codesmith_core::question::Choice {
                label: "Yes".into(),
                description: String::new(),
            }],
            multiple: false,
        };
        let mut a = App::new();
        let (tx, mut rx) = oneshot::channel();
        a.on_event(Ev::Question(vec![p("A?"), p("B?")], tx));
        typ(&mut a, "1");
        a.on_key(key(KeyCode::Enter));
        assert!(a.modal.is_some());
        typ(&mut a, "maybe");
        a.on_key(key(KeyCode::Enter));
        assert!(a.modal.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec![vec!["Yes".to_string()], vec!["maybe".to_string()]])
        );
    }

    #[test]
    fn cancelling_a_prompt_rejects_it() {
        let mut a = App::new();
        let (tx, mut rx) = oneshot::channel();
        a.on_event(Ev::Permission(req(), tx));
        a.cancel_modal();
        assert_eq!(rx.try_recv().unwrap(), Reply::Reject(None));
    }

    fn items(labels: &[&str]) -> Vec<PickItem> {
        labels
            .iter()
            .map(|l| PickItem {
                id: format!("id-{l}"),
                label: l.to_string(),
                detail: String::new(),
                current: *l == "beta",
            })
            .collect()
    }

    #[test]
    fn pickers_filter_and_choose() {
        let mut a = App::new();
        a.modal = Some(Modal::Picker(Picker::new(
            Pick::Model,
            "Models",
            items(&["alpha", "beta", "gamma"]),
        )));
        let Some(Modal::Picker(p)) = &a.modal else {
            unreachable!()
        };
        assert_eq!(p.sel, 1, "starts on the current item");
        typ(&mut a, "gam");
        let Some(Modal::Picker(p)) = &a.modal else {
            unreachable!()
        };
        assert_eq!(p.shown.len(), 1);
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Picked(Pick::Model, "id-gamma".into()))
        );
        assert!(a.modal.is_none());
        a.modal = Some(Modal::Picker(Picker::new(
            Pick::Session,
            "Sessions",
            items(&["alpha"]),
        )));
        typ(&mut a, "zz");
        assert_eq!(a.on_key(key(KeyCode::Enter)), None, "nothing matches");
        a.on_key(key(KeyCode::Backspace));
        a.on_key(key(KeyCode::Backspace));
        a.on_key(key(KeyCode::Esc));
        assert!(a.modal.is_none());
    }

    #[test]
    fn context_and_cost() {
        let mut a = App::new();
        assert_eq!(a.context_percent(), None);
        a.header.context_limit = Some(1000);
        a.on_event(Ev::StepEnd(Usage {
            input: 200,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        }));
        assert_eq!(a.context_percent(), Some(25));
        assert_eq!(a.cost(), None);
        a.header.cost = Some(Cost {
            input: 1.0,
            output: 2.0,
            cache_read: 0.0,
            cache_write: 0.0,
        });
        let c = a.cost().unwrap();
        assert!((c - 0.0003).abs() < 1e-9, "{c}");
    }

    #[test]
    fn scrolling_and_toggles() {
        let mut a = App::new();
        a.on_key(key(KeyCode::PageUp));
        a.on_key(key(KeyCode::PageUp));
        assert_eq!(a.scroll, 20);
        a.on_key(key(KeyCode::PageDown));
        assert_eq!(a.scroll, 10);
        a.on_key(ctrl('o'));
        assert!(a.expand_tools);
        typ(&mut a, "x");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.scroll, 0, "sending jumps to the bottom");
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut a = App::new();
        typ(&mut a, "ac");
        a.on_key(key(KeyCode::Left));
        a.on_paste("b\r\nb");
        assert_eq!(a.input.text(), "ab\nbc");
    }
}

/// Rebuild the transcript from a session's stored history (when continuing).
pub fn replay(messages: &[codesmith_ai::Message]) -> Vec<Entry> {
    use codesmith_ai::{Part, Role};
    let mut out: Vec<Entry> = Vec::new();
    for m in messages {
        for p in &m.parts {
            match (&m.role, p) {
                // The engine's own notes (the compaction continue note).
                (Role::System, _) => {}
                (Role::User, Part::Text(t)) if t.starts_with("<conversation-checkpoint>") => {
                    out.push(Entry::Notice("(earlier conversation summarized)".into()));
                }
                (Role::User, Part::Text(t))
                    if t.starts_with("<shell ")
                        || t.starts_with("<system-reminder>")
                        || codesmith_core::runner::is_engine_message(t) => {}
                (Role::User, Part::Text(t)) => {
                    out.push(Entry::User(super::attach::typed(t).to_string()))
                }
                (Role::User, Part::Media(m)) => out.push(Entry::Notice(format!(
                    "attached {}",
                    codesmith_core::media::label(m)
                ))),
                (Role::Assistant, Part::Text(t)) => out.push(Entry::Assistant(t.clone())),
                (Role::Assistant, Part::Reasoning(t)) => out.push(Entry::Reasoning(t.to_string())),
                (Role::Assistant, Part::ToolCall { id, name, input }) => {
                    out.push(Entry::Tool(Tool {
                        id: id.clone(),
                        name: name.clone(),
                        detail: crate::render::describe(name, input),
                        ..Default::default()
                    }))
                }
                (
                    Role::User,
                    Part::ToolResult {
                        id,
                        name,
                        content,
                        is_error,
                        ..
                    },
                ) => {
                    if let Some(Entry::Tool(t)) = out.iter_mut().rev().find(|e| {
                        matches!(e, Entry::Tool(t) if t.result.is_none()
                            && (t.id == *id || t.name == *name))
                    }) {
                        t.result = Some((first_line(content), *is_error));
                        t.output = content.clone();
                    }
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use codesmith_ai::{Message, Part, Role};
    use serde_json::json;

    #[test]
    fn replays_a_stored_conversation() {
        let msgs = vec![
            Message::user(
                "<conversation-checkpoint>\n<summary>S</summary>\n</conversation-checkpoint>",
            ),
            Message::user(format!(
                "fix @a.rs{}{{\"filePath\":\"/x/a.rs\"}}\nRead file a.rs",
                super::super::attach::MARKER
            )),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Text("Looking.".into()),
                    Part::ToolCall {
                        id: "c".into(),
                        name: "read".into(),
                        input: json!({"path": "a.rs"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "Read file a.rs, lines 1-2".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
            Message::user("<shell id=\"shell_1\" state=\"completed\" command=\"x\">\nok\n</shell>"),
            Message::system("Continue the task from where it left off."),
            Message::user(codesmith_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT),
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("Fixed.".into())],
            },
        ];
        assert_eq!(
            replay(&msgs),
            vec![
                Entry::Notice("(earlier conversation summarized)".into()),
                Entry::User("fix @a.rs".into()),
                Entry::Assistant("Looking.".into()),
                Entry::Tool(Tool {
                    id: "c".into(),
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-2".into(), false)),
                    output: "Read file a.rs, lines 1-2".into(),
                    diff: None,
                }),
                Entry::Assistant("Fixed.".into()),
            ]
        );
    }
}
