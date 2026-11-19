//! UI state and input handling. No terminal I/O here: keys and session
//! events go in, state changes and `Action`s for the runner come out.

use super::bridge::{Answers, Ev};
use codesmith_ai::Usage;
use codesmith_core::permission::{Reply, Request};
use codesmith_core::question::{interpret, Prompt};
use codesmith_core::store::Info;
use codesmith_core::tool::todo::Todo;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        name: String,
        detail: String,
        result: Option<(String, bool)>,
    },
    Notice(String),
    Error(String),
    Todos(Vec<Todo>),
}

pub enum Modal {
    Permission {
        req: Request,
        reply: Option<oneshot::Sender<Reply>>,
        feedback: String,
    },
    Question {
        prompts: Vec<Prompt>,
        idx: usize,
        answers: Vec<Vec<String>>,
        input: String,
        reply: Option<oneshot::Sender<Answers>>,
    },
    Sessions {
        list: Vec<Info>,
        sel: usize,
    },
    Help,
}

/// What the runner should do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Submit(String),
    Slash { name: String, args: String },
    Interrupt,
    Quit,
    Resume(String),
}

#[derive(Default)]
pub struct Header {
    pub agent: String,
    pub model: String,
    pub session: String,
    pub title: Option<String>,
}

#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
    pub history: Vec<String>,
    hist: Option<usize>,
    /// Lines scrolled up from the bottom of the transcript.
    pub scroll: usize,
    pub busy: bool,
    pub header: Header,
    pub usage: Usage,
    pub modal: Option<Modal>,
    pub show_reasoning: bool,
    pub quit: bool,
}

pub const HELP: &str = "Enter send · Alt+Enter or Ctrl+J newline · ↑/↓ history · PgUp/PgDn scroll
Esc or Ctrl+C interrupt a running turn · Ctrl+C on an empty prompt quits · Ctrl+T show reasoning

/new            start a new session
/sessions       pick a session to continue
/undo, /redo    undo or redo the latest turn's file changes
/agent [id]     switch agent (build, plan, …)
/model [p/m]    switch model
/compact        summarize older history now
/clear          clear the screen (the session is kept)
/help           this help
/exit           quit
/<command>      run a custom or built-in command (/init, /review, …)";

impl App {
    pub fn new() -> Self {
        App::default()
    }

    pub fn notice(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Notice(s.into()));
    }

    pub fn error(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Error(s.into()));
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
            Ev::ToolStart { name, detail } => self.entries.push(Entry::Tool {
                name,
                detail,
                result: None,
            }),
            Ev::ToolEnd {
                name,
                output,
                error,
            } => {
                let first = output
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                if let Some(Entry::Tool { result, .. }) =
                    self.entries.iter_mut().rev().find(
                        |e| matches!(e, Entry::Tool { name: n, result: None, .. } if *n == name),
                    )
                {
                    *result = Some((first, error));
                }
            }
            Ev::Todos(t) => {
                self.entries.retain(|e| !matches!(e, Entry::Todos(_)));
                self.entries.push(Entry::Todos(t));
            }
            Ev::Notice(n) => self.notice(n),
            Ev::Compacted(b, a) => self.notice(format!("compacted context: ~{b} → ~{a} tokens")),
            Ev::StepEnd(u) => {
                self.usage.input += u.input;
                self.usage.output += u.output;
                self.usage.cache_read += u.cache_read;
            }
            Ev::Permission(req, reply) => {
                self.modal = Some(Modal::Permission {
                    req,
                    reply: Some(reply),
                    feedback: String::new(),
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
            Ev::Key(_) | Ev::Paste(_) | Ev::Resize | Ev::Done { .. } => {}
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
            Some(_) => {}
            None => self.input.push_str(&s.replace("\r\n", "\n")),
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if self.modal.is_some() {
            return self.modal_key(k, ctrl);
        }
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    return Some(Action::Interrupt);
                }
                if self.input.is_empty() {
                    self.quit = true;
                    return Some(Action::Quit);
                }
                self.input.clear();
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => {
                self.quit = true;
                return Some(Action::Quit);
            }
            KeyCode::Char('t') if ctrl => self.show_reasoning = !self.show_reasoning,
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('j') if ctrl => self.input.push('\n'),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.push('\n')
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Esc => {
                if self.busy {
                    return Some(Action::Interrupt);
                }
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Up if !self.input.contains('\n') => self.history_step(-1),
            KeyCode::Down if !self.input.contains('\n') => self.history_step(1),
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::End if ctrl => self.scroll = 0,
            KeyCode::Char(c) => {
                self.input.push(c);
                self.hist = None;
            }
            KeyCode::Tab => self.input.push_str("    "),
            _ => {}
        }
        None
    }

    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.hist, dir) {
            (None, -1) => Some(self.history.len() - 1),
            (None, _) => None,
            (Some(i), -1) => Some(i.saturating_sub(1)),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.hist = next;
        self.input = next.map(|i| self.history[i].clone()).unwrap_or_default();
    }

    fn submit(&mut self) -> Option<Action> {
        let text = self.input.trim().to_string();
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
            self.input = text;
            return None;
        }
        Some(Action::Submit(text))
    }

    fn modal_key(&mut self, k: KeyEvent, ctrl: bool) -> Option<Action> {
        if ctrl && k.code == KeyCode::Char('c') {
            self.cancel_modal();
            return if self.busy {
                Some(Action::Interrupt)
            } else {
                None
            };
        }
        match self.modal.as_mut().unwrap() {
            Modal::Permission {
                feedback, reply, ..
            } => {
                let answer = match k.code {
                    KeyCode::Char('y') if feedback.is_empty() => Some(Reply::Once),
                    KeyCode::Char('a') if feedback.is_empty() => Some(Reply::Always),
                    KeyCode::Char('n') if feedback.is_empty() => Some(Reply::Reject(None)),
                    KeyCode::Esc => Some(Reply::Reject(None)),
                    KeyCode::Enter if feedback.trim().is_empty() => Some(Reply::Once),
                    KeyCode::Enter => Some(Reply::Reject(Some(feedback.trim().to_string()))),
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
            Modal::Sessions { list, sel } => match k.code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Up => *sel = sel.saturating_sub(1),
                KeyCode::Down if *sel + 1 < list.len() => *sel += 1,
                KeyCode::Enter => {
                    let id = list.get(*sel).map(|i| i.id.clone());
                    self.modal = None;
                    return id.map(Action::Resume);
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
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
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
        assert_eq!(a.input, "ab\n");
        a.on_key(ctrl('u'));
        assert!(a.input.is_empty());
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
    fn history() {
        let mut a = App::new();
        for m in ["one", "two"] {
            typ(&mut a, m);
            a.on_key(key(KeyCode::Enter));
        }
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input, "two");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input, "one");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input, "two");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input, "");
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
        assert_eq!(a.input, "more", "the text is kept");
    }

    #[test]
    fn quitting() {
        let mut a = App::new();
        typ(&mut a, "x");
        assert_eq!(a.on_key(ctrl('c')), None, "first Ctrl+C clears the input");
        assert_eq!(a.on_key(ctrl('c')), Some(Action::Quit));
        assert!(a.quit);
    }

    #[test]
    fn streaming_events_build_the_transcript() {
        let mut a = App::new();
        a.on_event(Ev::Text("Hel".into()));
        a.on_event(Ev::Text("lo".into()));
        a.on_event(Ev::ToolStart {
            name: "read".into(),
            detail: "a.rs".into(),
        });
        a.on_event(Ev::ToolEnd {
            name: "read".into(),
            output: "\nRead file a.rs, lines 1-3\n1: x".into(),
            error: false,
        });
        a.on_event(Ev::Text("Done".into()));
        assert_eq!(
            a.entries,
            vec![
                Entry::Assistant("Hello".into()),
                Entry::Tool {
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-3".into(), false))
                },
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
            (vec![key(KeyCode::Enter)], Reply::Once),
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

    #[test]
    fn scrolling() {
        let mut a = App::new();
        a.on_key(key(KeyCode::PageUp));
        a.on_key(key(KeyCode::PageUp));
        assert_eq!(a.scroll, 20);
        a.on_key(key(KeyCode::PageDown));
        assert_eq!(a.scroll, 10);
        typ(&mut a, "x");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.scroll, 0, "sending jumps to the bottom");
    }
}

/// Rebuild the transcript from a session's stored history (when continuing).
pub fn replay(messages: &[codesmith_ai::Message]) -> Vec<Entry> {
    use codesmith_ai::{Part, Role};
    let mut out: Vec<Entry> = Vec::new();
    for m in messages {
        for p in &m.parts {
            match (&m.role, p) {
                (Role::User, Part::Text(t)) if t.starts_with("<conversation-checkpoint>") => {
                    out.push(Entry::Notice("(earlier conversation summarized)".into()));
                }
                (Role::User, Part::Text(t))
                    if t.starts_with("<shell ") || t.starts_with("<system-reminder>") => {}
                (Role::User, Part::Text(t)) => out.push(Entry::User(t.clone())),
                (Role::Assistant, Part::Text(t)) => out.push(Entry::Assistant(t.clone())),
                (Role::Assistant, Part::Reasoning(t)) => out.push(Entry::Reasoning(t.clone())),
                (Role::Assistant, Part::ToolCall { name, input, .. }) => out.push(Entry::Tool {
                    name: name.clone(),
                    detail: crate::render::describe(name, input),
                    result: None,
                }),
                (
                    Role::User,
                    Part::ToolResult {
                        name,
                        content,
                        is_error,
                        ..
                    },
                ) => {
                    let first = content
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string();
                    if let Some(Entry::Tool { result, .. }) = out.iter_mut().rev().find(
                        |e| matches!(e, Entry::Tool { name: n, result: None, .. } if n == name),
                    ) {
                        *result = Some((first, *is_error));
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
            Message::user("fix it"),
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
                }],
            },
            Message::user("<shell id=\"shell_1\" state=\"completed\" command=\"x\">\nok\n</shell>"),
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("Fixed.".into())],
            },
        ];
        assert_eq!(
            replay(&msgs),
            vec![
                Entry::Notice("(earlier conversation summarized)".into()),
                Entry::User("fix it".into()),
                Entry::Assistant("Looking.".into()),
                Entry::Tool {
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-2".into(), false))
                },
                Entry::Assistant("Fixed.".into()),
            ]
        );
    }
}
