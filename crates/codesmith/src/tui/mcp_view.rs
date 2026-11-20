//! The MCP servers dialog (OpenCode's `DialogMcp`, `mcp.list`): each
//! configured server with its status; Enter (or Space) on one that needs
//! a sign-in starts its OAuth login and waits for it, and on one that
//! failed shows the error, from where `i` opens a new session with a
//! prompt to investigate it.

use super::keymap::Keymap;
use super::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connected,
    Disabled,
    Failed(String),
    NeedsAuth,
    /// Signed in here: it connects when its session's servers next start.
    SignedIn,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub name: String,
    pub status: Status,
}

impl Server {
    pub fn from_core(name: &str, s: &codesmith_core::mcp::Status) -> Server {
        use codesmith_core::mcp::Status as S;
        Server {
            name: name.to_string(),
            status: match s {
                S::Connected { .. } => Status::Connected,
                S::Disabled => Status::Disabled,
                S::Failed(e) => Status::Failed(e.clone()),
                S::NeedsAuth => Status::NeedsAuth,
            },
        }
    }
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Nothing,
    Close,
    /// Start the OAuth login of the server.
    SignIn(String),
    /// Open a new session with this prompt.
    Investigate(String),
}

#[derive(Debug, Clone, Default)]
pub struct McpView {
    pub servers: Vec<Server>,
    pub sel: usize,
    /// The server whose error is shown.
    pub detail: Option<usize>,
    /// The server whose login is waited for.
    pub waiting: Option<String>,
}

/// OpenCode's investigation prompt for a component that failed.
pub fn investigation(server: &str, status: &str, error: &str) -> String {
    let text = format!(
        "MCP server: {server}\nStatus: {status}\nConfiguration: mcp.servers.{server}\nError: {error}"
    );
    [
        "Investigate why this OpenCode component failed in the current project.",
        &text,
        "Inspect the relevant project and global OpenCode configuration, startup or loading behavior, required environment variables or credentials, dependencies, and logs. Identify the root cause and recommend a fix. Do not expose credentials.",
    ]
    .join("\n\n")
}

impl McpView {
    pub fn new(servers: Vec<Server>) -> McpView {
        McpView {
            servers,
            ..Default::default()
        }
    }

    /// The login of `name` ended: back to the list, on it.
    pub fn signed_in(&mut self, name: &str, ok: bool) {
        if self.waiting.as_deref() == Some(name) {
            self.waiting = None;
        }
        if let Some(i) = self.servers.iter().position(|s| s.name == name) {
            self.sel = i;
            if ok {
                self.servers[i].status = Status::SignedIn;
            }
        }
    }

    pub fn key(&mut self, k: &ratatui::crossterm::event::KeyEvent, km: &Keymap) -> Outcome {
        use ratatui::crossterm::event::KeyCode;
        let st = super::keymap::stroke(k);
        let hit = |id: &'static str| {
            st.as_ref()
                .is_some_and(|s| !km.lookup(&[id], std::slice::from_ref(s)).exact.is_empty())
        };
        if self.waiting.is_some() {
            if k.code == KeyCode::Esc {
                self.waiting = None;
            }
            return Outcome::Nothing;
        }
        if let Some(i) = self.detail {
            return match k.code {
                KeyCode::Esc => {
                    self.detail = None;
                    Outcome::Nothing
                }
                KeyCode::Char('i') => match &self.servers[i].status {
                    Status::Failed(e) => {
                        Outcome::Investigate(investigation(&self.servers[i].name, "failed", e))
                    }
                    _ => Outcome::Nothing,
                },
                _ => Outcome::Nothing,
            };
        }
        let n = self.servers.len();
        if hit("dialog.select.prev") {
            self.sel = self.sel.saturating_sub(1);
            return Outcome::Nothing;
        }
        if hit("dialog.select.next") {
            self.sel = (self.sel + 1).min(n.saturating_sub(1));
            return Outcome::Nothing;
        }
        let pick = hit("dialog.select.submit") || hit("dialog.mcp.toggle");
        if pick {
            let Some(s) = self.servers.get(self.sel) else {
                return Outcome::Nothing;
            };
            return match &s.status {
                Status::NeedsAuth => {
                    self.waiting = Some(s.name.clone());
                    Outcome::SignIn(s.name.clone())
                }
                Status::Failed(_) => {
                    self.detail = Some(self.sel);
                    Outcome::Nothing
                }
                _ => Outcome::Nothing,
            };
        }
        if k.code == KeyCode::Esc {
            return Outcome::Close;
        }
        Outcome::Nothing
    }

    pub fn title(&self) -> String {
        match self.detail {
            Some(i) => format!("MCP server: {}", self.servers[i].name),
            None => "MCP servers".into(),
        }
    }

    pub fn lines(&self, km: &Keymap) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let hint = |k: &str, what: &str| {
            vec![
                Span::styled(what.to_string(), fg(p.dialog_text)),
                Span::styled(format!(" {k}   "), fg(p.dialog_muted)),
            ]
        };
        if let Some(name) = &self.waiting {
            return vec![
                Line::from(format!("Signing in to {}", safe(name))),
                Line::default(),
                Line::from(Span::styled(
                    "Waiting for authorization… finish in the browser.",
                    fg(p.dialog_muted),
                )),
                Line::default(),
                Line::from(hint("esc", "back")),
            ];
        }
        if let Some(i) = self.detail {
            let s = &self.servers[i];
            let error = match &s.status {
                Status::Failed(e) => e.clone(),
                _ => "Unknown MCP connection error".into(),
            };
            let mut out = vec![
                Line::from(Span::styled(
                    "Status: failed".to_string(),
                    fg(p.dialog_muted),
                )),
                Line::from(Span::styled(
                    format!("Configuration: mcp.servers.{}", safe(&s.name)),
                    fg(p.dialog_muted),
                )),
                Line::default(),
            ];
            for l in safe(&error).split('\n') {
                out.push(Line::from(Span::styled(l.to_string(), fg(p.error))));
            }
            out.push(Line::default());
            let mut keys = hint("i", "investigate");
            keys.extend(hint("esc", "back"));
            out.push(Line::from(keys));
            return out;
        }
        let mut out = Vec::new();
        if self.servers.is_empty() {
            out.push(Line::from(Span::styled(
                "No MCP servers configured",
                fg(p.dialog_muted),
            )));
        }
        for (i, s) in self.servers.iter().enumerate() {
            let (status, color) = match &s.status {
                Status::Connected => ("Connected ✓", p.success),
                Status::Disabled => ("Disabled ○", p.dialog_muted),
                Status::Failed(_) => ("Failed !", p.error),
                Status::NeedsAuth => ("Sign in required →", p.warning),
                Status::SignedIn => ("Signed in · connects on restart", p.success),
            };
            let name = Span::styled(
                format!(" {:<24}", safe(&s.name)),
                if i == self.sel {
                    Style::default()
                        .fg(p.selected_fg)
                        .bg(p.selected_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    fg(p.dialog_text)
                },
            );
            out.push(Line::from(vec![
                name,
                Span::styled(format!(" {status}"), fg(color)),
            ]));
        }
        out.push(Line::default());
        let mut keys = Vec::new();
        match self.servers.get(self.sel).map(|s| &s.status) {
            Some(Status::Failed(_)) => keys.extend(hint("enter", "view error")),
            Some(Status::NeedsAuth) => keys.extend(hint("enter", "sign in")),
            _ => {}
        }
        let _ = km;
        keys.extend(hint("esc", "close"));
        out.push(Line::from(keys));
        out
    }
}
