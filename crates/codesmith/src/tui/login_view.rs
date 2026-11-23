//! The sign-in picker (`/login`): every provider with whether it is
//! signed in and where its login comes from; Enter on one shows its ways
//! in (a browser or device login, a key, Claude Code's own login, an AWS
//! profile, …) and signing out of a saved login. The logins are
//! `codesmith auth login`'s (see [`crate::cli::run_login`]); this view
//! only asks what they need and shows how they go.
//!
//! Keys and pasted redirects are typed into this dialog, never the
//! prompt: they are drawn masked, kept out of `{:?}`
//! ([`crate::cli::Secret`]), and go only to the auth store or the login
//! waiting for them. Everything else shown (names, URLs, messages) has its
//! control characters escaped. Keyboard only.

use super::keymap::Keymap;
use super::theme;
use crate::cli::{LoginRequest, Secret};
use codesmith_core::providers::signin::{Choice, Kind, State};
use codesmith_core::providers::verified;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::BTreeMap;

/// What a login reports while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum LoginEv {
    /// The URL to sign in at (opened in the browser too).
    Url(String),
    /// A device code to enter at `uri`.
    Device {
        uri: String,
        code: String,
    },
    Note(String),
    /// It ended: what to say, or why it failed.
    Done(Result<String, String>),
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Nothing,
    Close,
    /// Run this sign-in.
    Login(LoginRequest),
    /// Remove the provider's saved login.
    Logout(String),
    /// Open the page in the browser (a provider's API-key page).
    OpenUrl(String),
    /// Hand this pasted redirect to the login waiting for it.
    Paste(Secret),
    /// Stop the login that is waiting.
    Cancel,
    /// Run `claude setup-token` on the terminal for this choice, then save
    /// the token it prints (or ask for it to be pasted).
    SetupToken {
        choice: usize,
    },
}

/// One answer asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub key: String,
    pub title: String,
    pub placeholder: String,
    pub required: bool,
    /// Drawn masked (a key).
    pub secret: bool,
    pub value: String,
}

/// A row of a provider's menu.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Row {
    Method(usize),
    KeyPage,
    SignOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    List,
    Methods {
        choice: usize,
        sel: usize,
    },
    Ask {
        choice: usize,
        method: usize,
        inputs: Vec<Input>,
        at: usize,
        /// Why a client id is asked for, and where to register one.
        help: Option<String>,
        error: Option<String>,
    },
    Waiting {
        choice: usize,
        label: String,
        /// Whether a pasted redirect completes it.
        paste: bool,
        url: Option<String>,
        device: Option<(String, String)>,
        typed: String,
        notes: Vec<String>,
    },
    Done {
        ok: bool,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginView {
    pub choices: Vec<Choice>,
    /// For providers whose browser or device login has no client id
    /// configured: why, and where to register one.
    pub client_help: BTreeMap<String, String>,
    pub filter: String,
    /// The selected row of the filtered list.
    pub sel: usize,
    pub stage: Stage,
}

/// Masked text: one bullet a character, at most 24.
fn mask(s: &str) -> String {
    let n = s.chars().count();
    let shown = "•".repeat(n.min(24));
    if n > 24 {
        format!("{shown}… ({n} characters)")
    } else {
        shown
    }
}

/// Typed or pasted text without control characters.
fn clean(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().filter(|c| !c.is_control())
}

impl LoginView {
    pub fn new(choices: Vec<Choice>, client_help: BTreeMap<String, String>) -> LoginView {
        LoginView {
            choices,
            client_help,
            filter: String::new(),
            sel: 0,
            stage: Stage::List,
        }
    }

    /// The choices the filter keeps, by index.
    pub fn shown(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                f.is_empty()
                    || c.id.to_lowercase().contains(&f)
                    || c.name.to_lowercase().contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Open on provider `id`: its sign-in methods when it has any, else
    /// selected in the list (with its note). `false` when it is not listed.
    pub fn focus(&mut self, id: &str) -> bool {
        let Some(choice) = self.choices.iter().position(|c| c.id == id) else {
            return false;
        };
        self.filter.clear();
        self.sel = choice;
        let c = &self.choices[choice];
        if !c.methods.is_empty() || c.state.saved() {
            self.stage = Stage::Methods { choice, sel: 0 };
        }
        true
    }

    /// New states (after a sign-in or out), keeping the selection.
    pub fn refresh(&mut self, choices: Vec<Choice>, client_help: BTreeMap<String, String>) {
        self.choices = choices;
        self.client_help = client_help;
        self.sel = self.sel.min(self.shown().len().saturating_sub(1));
    }

    /// A login's progress.
    pub fn event(&mut self, ev: LoginEv) {
        match ev {
            LoginEv::Done(r) => {
                if matches!(self.stage, Stage::Waiting { .. }) {
                    self.stage = match r {
                        Ok(message) => Stage::Done { ok: true, message },
                        Err(message) => Stage::Done { ok: false, message },
                    };
                }
            }
            ev => {
                if let Stage::Waiting {
                    url, device, notes, ..
                } = &mut self.stage
                {
                    match ev {
                        LoginEv::Url(u) => *url = Some(u),
                        LoginEv::Device { uri, code } => *device = Some((uri, code)),
                        LoginEv::Note(n) => {
                            notes.push(n);
                            if notes.len() > 3 {
                                notes.remove(0);
                            }
                        }
                        LoginEv::Done(_) => {}
                    }
                }
            }
        }
    }

    /// Add a line to what a finished sign-in says (the model it selected).
    pub fn add_note(&mut self, line: &str) {
        if let Stage::Done { ok: true, message } = &mut self.stage {
            message.push('\n');
            message.push_str(line);
        }
    }

    /// A signing out ended.
    pub fn signed_out(&mut self, result: Result<String, String>) {
        self.stage = match result {
            Ok(message) => Stage::Done { ok: true, message },
            Err(message) => Stage::Done { ok: false, message },
        };
    }

    fn rows(&self, choice: usize) -> Vec<Row> {
        let c = &self.choices[choice];
        let mut rows: Vec<Row> = (0..c.methods.len()).map(Row::Method).collect();
        if c.key_page.is_some() && c.methods.iter().any(|m| m.kind == Kind::Key) {
            rows.push(Row::KeyPage);
        }
        if c.state.saved() {
            rows.push(Row::SignOut);
        }
        rows
    }

    fn row_label(&self, choice: usize, row: Row) -> String {
        let c = &self.choices[choice];
        match row {
            Row::Method(i) => {
                let m = &c.methods[i];
                match m.kind {
                    Kind::Browser { paste: false } => {
                        format!("{} (browser on this machine only)", m.label)
                    }
                    _ => m.label.clone(),
                }
            }
            Row::KeyPage => format!("Open the API key page ({})", c.key_page.unwrap_or("")),
            Row::SignOut => "Sign out (remove the saved login)".into(),
        }
    }

    /// Ask for a Claude Code token to be pasted for `choice` (after
    /// `claude setup-token` printed none CodeSmith could read). False when
    /// the choice has no such method.
    pub fn ask_token(&mut self, choice: usize) -> bool {
        let Some(m) = self.choices.get(choice).and_then(|c| {
            c.methods
                .iter()
                .position(|m| m.kind == (Kind::ClaudeToken { create: false }))
        }) else {
            return false;
        };
        let _ = self.pick_method(choice, m);
        true
    }

    /// Start method `m` of `choice`: ask what it needs, or run it.
    fn pick_method(&mut self, choice: usize, m: usize) -> Outcome {
        let c = &self.choices[choice];
        let method = &c.methods[m];
        if method.kind == (Kind::ClaudeToken { create: true }) {
            return Outcome::SetupToken { choice };
        }
        let mut inputs = Vec::new();
        let help = match method.kind {
            Kind::Browser { .. } | Kind::Device => self.client_help.get(&c.id).cloned(),
            _ => None,
        };
        if help.is_some() {
            inputs.push(Input {
                key: "clientId".into(),
                title: "Client id of your OAuth app".into(),
                placeholder: "from the provider's developer settings".into(),
                required: true,
                secret: false,
                value: String::new(),
            });
        }
        if method.kind == (Kind::ClaudeToken { create: false }) {
            inputs.push(Input {
                key: "key".into(),
                title: "Claude Code token".into(),
                placeholder: "paste the token `claude setup-token` printed; it is not shown".into(),
                required: true,
                secret: true,
                value: String::new(),
            });
        }
        if method.kind == Kind::Key {
            inputs.push(Input {
                key: "key".into(),
                title: format!("API key for {}", c.name),
                placeholder: "paste or type it; it is not shown".into(),
                required: true,
                secret: true,
                value: String::new(),
            });
        }
        for f in &method.fields {
            inputs.push(Input {
                key: f.key.to_string(),
                title: f.title.to_string(),
                placeholder: f.placeholder.to_string(),
                required: f.required,
                secret: false,
                value: String::new(),
            });
        }
        if inputs.is_empty() {
            return self.start(choice, m, Vec::new());
        }
        self.stage = Stage::Ask {
            choice,
            method: m,
            inputs,
            at: 0,
            help,
            error: None,
        };
        Outcome::Nothing
    }

    /// Run method `m` of `choice` with these answers.
    fn start(&mut self, choice: usize, m: usize, inputs: Vec<Input>) -> Outcome {
        let c = &self.choices[choice];
        let method = &c.methods[m];
        let mut key = None;
        let mut client_id = None;
        let mut answers = BTreeMap::new();
        for i in inputs {
            match i.key.as_str() {
                "key" if i.secret => key = Some(Secret(i.value)),
                "clientId" => client_id = Some(i.value.trim().to_string()),
                _ => {
                    answers.insert(i.key, i.value.trim().to_string());
                }
            }
        }
        let provider = match method.kind {
            Kind::ClaudeCode | Kind::ClaudeToken { .. } => {
                codesmith_core::providers::claude_agent::ID.to_string()
            }
            _ => c.id.clone(),
        };
        let label = match method.kind {
            Kind::ClaudeCode => "Checking Claude Code…".to_string(),
            Kind::ClaudeToken { .. } => "Saving the token…".to_string(),
            Kind::Key => "Saving the key…".to_string(),
            _ => method.label.clone(),
        };
        self.stage = Stage::Waiting {
            choice,
            label,
            paste: method.kind == Kind::Browser { paste: true },
            url: None,
            device: None,
            typed: String::new(),
            notes: Vec::new(),
        };
        Outcome::Login(LoginRequest {
            provider,
            kind: method.kind,
            key,
            answers,
            client_id,
        })
    }

    /// Pasted text: into the answer being typed or the paste box.
    pub fn paste(&mut self, s: &str) {
        let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        match &mut self.stage {
            Stage::List => {
                self.filter.extend(clean(line));
                self.sel = 0;
            }
            Stage::Ask { inputs, at, .. } => {
                if let Some(i) = inputs.get_mut(*at) {
                    i.value.extend(clean(line));
                }
            }
            Stage::Waiting {
                paste: true, typed, ..
            } => typed.extend(clean(line)),
            _ => {}
        }
    }

    pub fn key(&mut self, k: &KeyEvent, km: &Keymap) -> Outcome {
        let st = super::keymap::stroke(k);
        let hit = |id: &'static str| {
            st.as_ref()
                .is_some_and(|s| !km.lookup(&[id], std::slice::from_ref(s)).exact.is_empty())
        };
        let typed = match k.code {
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(c)
            }
            _ => None,
        };
        match &mut self.stage {
            Stage::List => {
                let shown = self.shown();
                if k.code == KeyCode::Esc {
                    return Outcome::Close;
                }
                if k.code == KeyCode::Up || (typed.is_none() && hit("dialog.select.prev")) {
                    self.sel = self.sel.saturating_sub(1);
                } else if k.code == KeyCode::Down || (typed.is_none() && hit("dialog.select.next"))
                {
                    self.sel = (self.sel + 1).min(shown.len().saturating_sub(1));
                } else if k.code == KeyCode::Enter {
                    if let Some(&choice) = shown.get(self.sel) {
                        if self.choices[choice].methods.is_empty()
                            && !self.choices[choice].state.saved()
                        {
                            return Outcome::Nothing;
                        }
                        self.stage = Stage::Methods { choice, sel: 0 };
                    }
                } else if k.code == KeyCode::Backspace {
                    self.filter.pop();
                    self.sel = 0;
                } else if let Some(c) = typed {
                    self.filter.push(c);
                    self.sel = 0;
                }
                Outcome::Nothing
            }
            Stage::Methods { choice, sel } => {
                let (choice, cur) = (*choice, *sel);
                let rows = self.rows(choice);
                if k.code == KeyCode::Esc {
                    self.stage = Stage::List;
                } else if k.code == KeyCode::Up || hit("dialog.select.prev") {
                    self.stage = Stage::Methods {
                        choice,
                        sel: cur.saturating_sub(1),
                    };
                } else if k.code == KeyCode::Down || hit("dialog.select.next") {
                    self.stage = Stage::Methods {
                        choice,
                        sel: (cur + 1).min(rows.len().saturating_sub(1)),
                    };
                } else if k.code == KeyCode::Enter {
                    return match rows.get(cur) {
                        Some(Row::Method(m)) => self.pick_method(choice, *m),
                        Some(Row::KeyPage) => self.choices[choice]
                            .key_page
                            .map_or(Outcome::Nothing, |u| Outcome::OpenUrl(u.to_string())),
                        Some(Row::SignOut) => {
                            self.stage = Stage::Waiting {
                                choice,
                                label: "Signing out…".into(),
                                paste: false,
                                url: None,
                                device: None,
                                typed: String::new(),
                                notes: Vec::new(),
                            };
                            Outcome::Logout(self.choices[choice].id.clone())
                        }
                        None => Outcome::Nothing,
                    };
                }
                Outcome::Nothing
            }
            Stage::Ask {
                choice,
                method,
                inputs,
                at,
                error,
                ..
            } => {
                let (choice, method) = (*choice, *method);
                match k.code {
                    KeyCode::Esc => {
                        self.stage = Stage::Methods { choice, sel: 0 };
                    }
                    KeyCode::Backspace => {
                        if let Some(i) = inputs.get_mut(*at) {
                            i.value.pop();
                        }
                    }
                    KeyCode::Up if *at > 0 => *at -= 1,
                    KeyCode::Enter => {
                        let i = &inputs[*at];
                        if i.required && i.value.trim().is_empty() {
                            *error = Some(format!("{} is required", i.title));
                            return Outcome::Nothing;
                        }
                        *error = None;
                        if *at + 1 < inputs.len() {
                            *at += 1;
                        } else {
                            let inputs = std::mem::take(inputs);
                            return self.start(choice, method, inputs);
                        }
                    }
                    _ => {
                        if let (Some(c), Some(i)) = (typed, inputs.get_mut(*at)) {
                            i.value.push(c);
                        }
                    }
                }
                Outcome::Nothing
            }
            Stage::Waiting {
                choice,
                paste,
                typed: buf,
                ..
            } => {
                let choice = *choice;
                match k.code {
                    KeyCode::Esc => {
                        self.stage = Stage::Methods { choice, sel: 0 };
                        return Outcome::Cancel;
                    }
                    KeyCode::Backspace if *paste => {
                        buf.pop();
                    }
                    KeyCode::Enter if *paste && !buf.trim().is_empty() => {
                        return Outcome::Paste(Secret(std::mem::take(buf)));
                    }
                    _ => {
                        if let (Some(c), true) = (typed, *paste) {
                            buf.push(c);
                        }
                    }
                }
                Outcome::Nothing
            }
            Stage::Done { .. } => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Enter) {
                    self.stage = Stage::List;
                }
                Outcome::Nothing
            }
        }
    }

    pub fn title(&self) -> String {
        let name = |i: usize| super::view::display_safe(&self.choices[i].name);
        match &self.stage {
            Stage::List | Stage::Done { .. } => "Sign in".into(),
            Stage::Methods { choice, .. }
            | Stage::Ask { choice, .. }
            | Stage::Waiting { choice, .. } => format!("Sign in · {}", name(*choice)),
        }
    }

    /// The dialog's lines, the list at most `rows` providers tall.
    pub fn lines(&self, rows: usize) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let hint = |k: &str, what: &str| {
            vec![
                Span::styled(what.to_string(), fg(p.dialog_text)),
                Span::styled(format!(" {k}   "), fg(p.dialog_muted)),
            ]
        };
        let selected = Style::default()
            .fg(p.selected_fg)
            .bg(p.selected_bg)
            .add_modifier(Modifier::BOLD);
        let mut out = Vec::new();
        match &self.stage {
            Stage::List => {
                out.push(Line::from(vec![
                    Span::styled("> ", fg(p.primary)),
                    Span::styled(safe(&self.filter), fg(p.dialog_text)),
                    Span::styled(
                        if self.filter.is_empty() {
                            "type to filter"
                        } else {
                            ""
                        },
                        fg(p.dialog_muted),
                    ),
                ]));
                out.push(Line::default());
                let shown = self.shown();
                if shown.is_empty() {
                    out.push(Line::from(Span::styled("No matches.", fg(p.dialog_muted))));
                }
                let rows = rows.max(1);
                let top = (self.sel + 1).saturating_sub(rows);
                for (pos, &i) in shown.iter().enumerate().skip(top).take(rows) {
                    let c = &self.choices[i];
                    let color = match &c.state {
                        State::Saved(_) => p.success,
                        State::Env(_) | State::Config | State::Ambient(_) => p.accent,
                        State::None => p.dialog_muted,
                    };
                    let mark = if c.state.signed_in() { "● " } else { "  " };
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("{mark}{:<32}", safe(&c.name)),
                            if pos == self.sel {
                                selected
                            } else {
                                fg(p.dialog_text)
                            },
                        ),
                        Span::styled(format!(" {}", safe(&c.state.describe())), fg(color)),
                        Span::styled(
                            if verified::verified(&c.id) {
                                String::new()
                            } else {
                                format!(" · {}", verified::TAG)
                            },
                            fg(p.warning),
                        ),
                    ]));
                }
                out.push(Line::default());
                let mut keys = hint("enter", "sign in");
                keys.extend(hint("↑/↓", "move"));
                keys.extend(hint("esc", "close"));
                out.push(Line::from(keys));
            }
            Stage::Methods { choice, sel } => {
                let c = &self.choices[*choice];
                out.push(Line::from(Span::styled(
                    safe(&c.state.describe()),
                    fg(p.dialog_muted),
                )));
                out.push(Line::default());
                if let Some(n) = &c.note {
                    out.push(Line::from(Span::styled(safe(n), fg(p.warning))));
                    out.push(Line::default());
                }
                if let Some(w) = verified::warning(&c.id, &c.name) {
                    out.push(Line::from(Span::styled(
                        format!("⚠︎ {}", safe(&w)),
                        fg(p.warning),
                    )));
                    out.push(Line::default());
                }
                for (i, r) in self.rows(*choice).into_iter().enumerate() {
                    let label = format!(" {} ", safe(&self.row_label(*choice, r)));
                    out.push(Line::from(Span::styled(
                        label,
                        if i == *sel {
                            selected
                        } else {
                            fg(p.dialog_text)
                        },
                    )));
                }
                out.push(Line::default());
                let mut keys = hint("enter", "choose");
                keys.extend(hint("esc", "back"));
                out.push(Line::from(keys));
            }
            Stage::Ask {
                inputs,
                at,
                help,
                error,
                ..
            } => {
                if let Some(h) = help {
                    for l in safe(h).split('\n') {
                        out.push(Line::from(Span::styled(l.to_string(), fg(p.dialog_muted))));
                    }
                    out.push(Line::default());
                }
                for (i, input) in inputs.iter().enumerate() {
                    let value = if input.secret {
                        mask(&input.value)
                    } else {
                        safe(&input.value)
                    };
                    let current = i == *at;
                    let mut spans = vec![Span::styled(
                        format!("{}: ", safe(&input.title)),
                        if current {
                            fg(p.primary).add_modifier(Modifier::BOLD)
                        } else {
                            fg(p.dialog_text)
                        },
                    )];
                    if value.is_empty() && current {
                        spans.push(Span::styled(safe(&input.placeholder), fg(p.dialog_muted)));
                    } else {
                        spans.push(Span::styled(value, fg(p.dialog_text)));
                    }
                    if current {
                        spans.push(Span::styled("▏", fg(p.primary)));
                    }
                    out.push(Line::from(spans));
                }
                if let Some(e) = error {
                    out.push(Line::default());
                    out.push(Line::from(Span::styled(safe(e), fg(p.error))));
                }
                out.push(Line::default());
                let mut keys = hint("enter", "next");
                keys.extend(hint("esc", "back"));
                out.push(Line::from(keys));
            }
            Stage::Waiting {
                label,
                paste,
                url,
                device,
                typed,
                notes,
                ..
            } => {
                out.push(Line::from(safe(label)));
                out.push(Line::default());
                if let Some((uri, code)) = device {
                    out.push(Line::from(format!("Open {} on any device", safe(uri))));
                    out.push(Line::from(vec![
                        Span::raw("and enter the code "),
                        Span::styled(safe(code), fg(p.primary).add_modifier(Modifier::BOLD)),
                    ]));
                    out.push(Line::default());
                }
                if let Some(u) = url {
                    out.push(Line::from(Span::styled(
                        "Open this URL to sign in (it should open by itself):",
                        fg(p.dialog_muted),
                    )));
                    out.push(Line::from(safe(u)));
                    out.push(Line::default());
                }
                if url.is_some() || device.is_some() {
                    out.push(Line::from(Span::styled(
                        "Waiting for authorization… finish in the browser.",
                        fg(p.dialog_muted),
                    )));
                }
                if *paste {
                    out.push(Line::from(Span::styled(
                        "Browser on another machine? Paste the URL it ends up on:",
                        fg(p.dialog_muted),
                    )));
                    out.push(Line::from(vec![
                        Span::styled("> ", fg(p.primary)),
                        Span::raw(mask(typed)),
                        Span::styled("▏", fg(p.primary)),
                    ]));
                }
                for n in notes {
                    out.push(Line::from(Span::styled(safe(n), fg(p.warning))));
                }
                out.push(Line::default());
                let mut keys = Vec::new();
                if *paste {
                    keys.extend(hint("enter", "use the pasted URL"));
                }
                keys.extend(hint("esc", "cancel"));
                out.push(Line::from(keys));
            }
            Stage::Done { ok, message } => {
                let color = if *ok { p.success } else { p.error };
                for l in safe(message).split('\n') {
                    out.push(Line::from(Span::styled(l.to_string(), fg(color))));
                }
                out.push(Line::default());
                out.push(Line::from(hint("enter", "back to the list")));
            }
        }
        out
    }
}

/// The picker's providers and client-id notes, from the configuration,
/// the auth store `file` and the environment.
pub fn load(
    config: &codesmith_core::config::Config,
    file: &std::path::Path,
    env: &codesmith_core::providers::Env,
) -> (Vec<Choice>, BTreeMap<String, String>) {
    use codesmith_core::providers::signin;
    let choices = signin::choices(config, file, env);
    let help = choices
        .iter()
        .filter(|c| {
            c.methods
                .iter()
                .any(|m| matches!(m.kind, Kind::Browser { .. } | Kind::Device))
        })
        .filter_map(|c| {
            let p = config.provider(&c.id).unwrap_or_default();
            signin::client_id_missing(&c.id, &p, file, env).map(|h| (c.id.clone(), h))
        })
        .collect();
    (choices, help)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Action, App, Modal};
    use codesmith_core::providers::signin;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn env(pairs: &[(&str, &str)]) -> codesmith_core::providers::Env {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move |k| m.get(k).cloned())
    }

    /// The whole screen, as drawn.
    fn frame(a: &App) -> String {
        let (w, h) = (110, 40);
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| crate::tui::view::draw(f, a)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A picker over a fake auth store: Groq has a saved key, Poe a saved
    /// login, Anthropic a key in the environment.
    fn picker() -> (App, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        codesmith_core::auth::set_in(&file, "groq", "gsk-stored-secret-0001").unwrap();
        codesmith_core::auth::set_entry_in(
            &file,
            "poe",
            serde_json::json!({"type": "oauth", "access": "poe-stored-secret", "refresh": "", "expires": codesmith_core::providers::poe::NEVER}),
        )
        .unwrap();
        let config = codesmith_core::config::Config::default();
        let (choices, help) = load(
            &config,
            &file,
            &env(&[("ANTHROPIC_API_KEY", "sk-ant-env-secret")]),
        );
        let mut a = App::new();
        a.modal = Some(Modal::Login(Box::new(LoginView::new(choices, help))));
        (a, d)
    }

    fn view(a: &App) -> &LoginView {
        match &a.modal {
            Some(Modal::Login(v)) => v,
            _ => panic!("the picker is not open"),
        }
    }

    fn view_mut(a: &mut App) -> &mut LoginView {
        match &mut a.modal {
            Some(Modal::Login(v)) => v,
            _ => panic!("the picker is not open"),
        }
    }

    fn type_text(a: &mut App, s: &str) -> Option<Action> {
        let mut last = None;
        for c in s.chars() {
            last = a.on_key(key(KeyCode::Char(c)));
        }
        last
    }

    /// The row of the list naming `name` (filtered to it).
    fn row(a: &mut App, filter: &str, name: &str) -> String {
        view_mut(a).filter.clear();
        type_text(a, filter);
        let f = frame(a);
        f.lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("no {name} row:\n{f}"))
            .to_string()
    }

    const SECRETS: &[&str] = &[
        "gsk-stored-secret",
        "poe-stored-secret",
        "sk-ant-env-secret",
    ];

    #[test]
    fn the_list_shows_each_providers_state_and_no_key() {
        let (mut a, _d) = picker();
        let f = frame(&a);
        assert!(f.contains("Sign in") && f.contains("type to filter"), "{f}");
        assert!(row(&mut a, "groq", "Groq").contains("signed in · saved key"));
        assert!(row(&mut a, "poe", "Poe").contains("signed in · saved Poe login"));
        assert!(row(&mut a, "anthropic", "Anthropic").contains("key from $ANTHROPIC_API_KEY"));
        assert!(row(&mut a, "fireworks", "Fireworks").contains("not signed in"));
        assert!(row(&mut a, "claude", "Claude Pro/Max (Claude Code)").contains("not signed in"));
        for s in SECRETS {
            assert!(!f.contains(s), "{s} drawn");
        }
        // Backspace widens the filter again; Esc closes.
        for _ in 0..6 {
            a.on_key(key(KeyCode::Backspace));
        }
        assert!(view(&a).filter.is_empty());
        assert_eq!(a.on_key(key(KeyCode::Esc)), None);
        assert!(a.modal.is_none());
    }

    #[test]
    fn untested_providers_are_tagged_and_warned_about_and_tested_ones_are_not() {
        let (mut a, _d) = picker();
        for (filter, name) in [("groq", "Groq"), ("poe", "Poe")] {
            assert!(row(&mut a, filter, name).contains("· untested"), "{name}");
        }
        for (filter, name) in [
            ("anthropic", "Anthropic"),
            ("fireworks", "Fireworks"),
            ("claude", "Claude Pro/Max (Claude Code)"),
        ] {
            assert!(!row(&mut a, filter, name).contains("untested"), "{name}");
        }
        // Choosing an untested provider shows the warning with the links.
        view_mut(&mut a).filter.clear();
        type_text(&mut a, "groq");
        a.on_key(key(KeyCode::Enter));
        let f: String = frame(&a).split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(f.contains("hasn't been tested with CodeSmith"), "{f}");
        // A tested one doesn't.
        a.on_key(key(KeyCode::Esc));
        view_mut(&mut a).filter.clear();
        type_text(&mut a, "fireworks");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains("Paste an API key"), "{f}");
        assert!(!f.contains("hasn't been tested"), "{f}");
    }

    #[test]
    fn a_key_is_typed_masked_and_handed_only_to_the_login() {
        let (mut a, _d) = picker();
        type_text(&mut a, "fireworks");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains("Paste an API key"), "{f}");
        // Fireworks has a well-known key page: offered, opened on Enter.
        assert!(
            f.contains("Open the API key page (https://app.fireworks.ai/settings/users/api-keys)"),
            "{f}"
        );
        a.on_key(key(KeyCode::Down));
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Login(Outcome::OpenUrl(
                "https://app.fireworks.ai/settings/users/api-keys".into()
            )))
        );
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Enter));
        assert!(frame(&a).contains("API key for Fireworks AI"));
        // Enter with nothing typed asks again.
        a.on_key(key(KeyCode::Enter));
        assert!(frame(&a).contains("is required"));
        type_text(&mut a, "fw-typed-");
        a.on_paste("secret-0042\nignored second line");
        let f = frame(&a);
        assert!(!f.contains("fw-typed") && !f.contains("secret-0042"), "{f}");
        assert!(f.contains(&"•".repeat(20)), "{f}");
        let Some(Action::Login(Outcome::Login(req))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        assert_eq!(req.provider, "fireworks");
        assert_eq!(req.kind, Kind::Key);
        assert_eq!(req.key, Some(Secret("fw-typed-secret-0042".into())));
        // Not in `{:?}` (a log line), nor on the screen while it saves.
        assert!(!format!("{req:?}").contains("secret-0042"));
        let f = frame(&a);
        assert!(f.contains("Saving the key"), "{f}");
        assert!(!f.contains("secret-0042"), "{f}");
        view_mut(&mut a).event(LoginEv::Done(Ok("Saved a key for fireworks".into())));
        let f = frame(&a);
        assert!(
            f.contains("Saved a key for fireworks") && !f.contains("secret-0042"),
            "{f}"
        );
        a.on_key(key(KeyCode::Enter));
        assert_eq!(view(&a).stage, Stage::List);
    }

    #[test]
    fn a_browser_login_asks_for_a_client_id_then_takes_a_masked_paste() {
        let (mut a, _d) = picker();
        type_text(&mut a, "openai");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains("Sign in with ChatGPT"), "{f}");
        a.on_key(key(KeyCode::Enter));
        // No vendor client id: where to get one of your own.
        let f = frame(&a);
        assert!(f.contains("Client id of your OAuth app"), "{f}");
        assert!(f.contains("client"), "{f}");
        type_text(&mut a, "app_mine");
        let Some(Action::Login(Outcome::Login(req))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        assert_eq!(req.provider, "openai");
        assert_eq!(req.kind, Kind::Browser { paste: true });
        assert_eq!(req.client_id.as_deref(), Some("app_mine"));
        assert_eq!(req.key, None);
        view_mut(&mut a).event(LoginEv::Url(
            "https://auth.example/authorize?client_id=app_mine\x1b[2J".into(),
        ));
        let f = frame(&a);
        assert!(
            f.contains("https://auth.example/authorize?client_id=app_mine"),
            "{f}"
        );
        assert!(!f.contains('\x1b'), "an escape reached the screen");
        assert!(f.contains("Paste the URL it ends up on"), "{f}");
        let pasted = "http://127.0.0.1:1455/auth/callback?code=CODE-SECRET-77&state=s";
        a.on_paste(pasted);
        let f = frame(&a);
        assert!(!f.contains("CODE-SECRET-77") && !f.contains("code="), "{f}");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Login(Outcome::Paste(Secret(pasted.into()))))
        );
        view_mut(&mut a).event(LoginEv::Note(
            "This URL belongs to a different sign-in; still waiting".into(),
        ));
        assert!(frame(&a).contains("different sign-in"));
        // Esc stops the login and goes back to the provider's menu.
        assert_eq!(
            a.on_key(key(KeyCode::Esc)),
            Some(Action::Login(Outcome::Cancel))
        );
        assert!(matches!(view(&a).stage, Stage::Methods { .. }));
    }

    #[test]
    fn a_device_login_shows_its_code() {
        let (mut a, _d) = picker();
        type_text(&mut a, "copilot");
        a.on_key(key(KeyCode::Enter));
        a.on_key(key(KeyCode::Enter));
        type_text(&mut a, "Iv1.mine");
        let Some(Action::Login(Outcome::Login(req))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        assert_eq!(req.kind, Kind::Device);
        view_mut(&mut a).event(LoginEv::Device {
            uri: "https://github.com/login/device".into(),
            code: "ABCD-1234".into(),
        });
        let f = frame(&a);
        assert!(
            f.contains("https://github.com/login/device") && f.contains("ABCD-1234"),
            "{f}"
        );
        // A device login takes no paste.
        a.on_paste("whatever");
        assert!(!frame(&a).contains("whatever"));
    }

    #[test]
    fn the_claude_code_entry_runs_the_claude_agent_check() {
        let (mut a, _d) = picker();
        type_text(&mut a, "anthropic");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains(signin::CLAUDE_CODE_LABEL), "{f}");
        // Past the API key and the two token entries.
        for _ in 0..3 {
            a.on_key(key(KeyCode::Down));
        }
        let Some(Action::Login(Outcome::Login(req))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        assert_eq!(req.provider, "claude-agent");
        assert_eq!(req.kind, Kind::ClaudeCode);
        assert_eq!(req.key, None);
        assert!(frame(&a).contains("Checking Claude Code"));
        view_mut(&mut a).event(LoginEv::Done(Err(
            "Claude Code is not installed. Install it, then run `claude auth login`.".into(),
        )));
        let f = frame(&a);
        assert!(
            f.contains("Claude Code is not installed") && f.contains("claude auth login"),
            "{f}"
        );
        // A failed login selects nothing.
        view_mut(&mut a).add_note("Now using claude-agent/opus.");
        assert!(!frame(&a).contains("Now using"));
    }

    #[test]
    fn the_claude_code_token_entry_runs_setup_token_then_takes_a_paste() {
        let (mut a, _d) = picker();
        type_text(&mut a, "anthropic");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains(signin::CLAUDE_TOKEN_LABEL), "{f}");
        assert!(f.contains("works over SSH"), "{f}");
        a.on_key(key(KeyCode::Down));
        // The first token entry hands the terminal to `claude setup-token`.
        let Some(Action::Login(Outcome::SetupToken { choice })) = a.on_key(key(KeyCode::Enter))
        else {
            panic!("no setup-token")
        };
        // When no token could be read from it, the token is pasted, unseen.
        assert!(view_mut(&mut a).ask_token(choice));
        let f = frame(&a);
        assert!(f.contains("Claude Code token"), "{f}");
        let token = "sk-ant-oat01-SECRET-TOKEN-0123456789abcdefghijklmnopqrstuvwxyz";
        a.on_paste(token);
        assert!(
            !frame(&a).contains("SECRET-TOKEN"),
            "the token is never shown"
        );
        let Some(Action::Login(Outcome::Login(req))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        assert_eq!(req.provider, "claude-agent");
        assert_eq!(req.kind, Kind::ClaudeToken { create: false });
        assert_eq!(req.key.as_ref().map(|k| k.0.as_str()), Some(token));
        assert!(frame(&a).contains("Saving the token"));
    }

    #[test]
    fn a_provider_without_token_entries_asks_for_no_token() {
        let (mut a, _d) = picker();
        type_text(&mut a, "copilot");
        a.on_key(key(KeyCode::Enter));
        let Stage::Methods { choice, .. } = view(&a).stage else {
            panic!("no methods")
        };
        assert!(!view_mut(&mut a).ask_token(choice));
    }

    // Regression: signing in to Claude Code only said so, and the session
    // kept the model it had.
    #[test]
    fn a_successful_login_says_which_model_it_selected() {
        let (mut a, _d) = picker();
        type_text(&mut a, "anthropic");
        a.on_key(key(KeyCode::Enter));
        // Past the API key and the two token entries.
        for _ in 0..3 {
            a.on_key(key(KeyCode::Down));
        }
        let Some(Action::Login(Outcome::Login(_))) = a.on_key(key(KeyCode::Enter)) else {
            panic!("no login")
        };
        view_mut(&mut a).event(LoginEv::Done(Ok("Claude Code 2.0 is signed in.".into())));
        view_mut(&mut a).add_note("Now using claude-agent/opus (saved as the default).");
        let f = frame(&a);
        assert!(f.contains("Claude Code 2.0 is signed in."), "{f}");
        assert!(f.contains("Now using claude-agent/opus"), "{f}");
    }

    #[test]
    fn a_saved_login_can_be_signed_out_of() {
        let (mut a, _d) = picker();
        type_text(&mut a, "groq");
        a.on_key(key(KeyCode::Enter));
        let f = frame(&a);
        assert!(f.contains("Sign out"), "{f}");
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Login(Outcome::Logout("groq".into())))
        );
        view_mut(&mut a).signed_out(Ok("Removed the saved key for groq".into()));
        assert!(frame(&a).contains("Removed the saved key for groq"));
        // A provider with nothing saved offers no sign-out.
        a.on_key(key(KeyCode::Enter));
        view_mut(&mut a).filter.clear();
        type_text(&mut a, "fireworks");
        a.on_key(key(KeyCode::Enter));
        assert!(!frame(&a).contains("Sign out"));
    }

    #[test]
    fn a_late_or_stray_login_event_changes_nothing_outside_a_wait() {
        let (mut a, _d) = picker();
        view_mut(&mut a).event(LoginEv::Done(Ok("stray".into())));
        assert_eq!(view(&a).stage, Stage::List);
        view_mut(&mut a).event(LoginEv::Url("https://x.example".into()));
        assert!(!frame(&a).contains("x.example"));
    }

    #[test]
    fn names_from_config_are_drawn_without_their_escapes() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        let config: codesmith_core::config::Config = serde_json::from_value(serde_json::json!({
            "providers": {"evil": {"name": "Evil\u{1b}]0;pwned\u{7}Co", "env": ["EVIL_KEY"]}}
        }))
        .unwrap();
        let (choices, help) = load(&config, &file, &env(&[]));
        let mut a = App::new();
        a.modal = Some(Modal::Login(Box::new(LoginView::new(choices, help))));
        type_text(&mut a, "evil");
        let f = frame(&a);
        assert!(!f.contains('\x1b') && !f.contains('\x07'), "{f:?}");
        assert!(f.contains("Evil"), "{f}");
    }
}
