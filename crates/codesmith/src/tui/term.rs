//! What the UI tells the terminal outside its frames: the window title,
//! the summary printed on exit, and the program status (whether a session
//! is working or waiting for its user). Everything built from session
//! text (titles, messages) is stripped of control characters first, so a
//! title cannot end the escape sequence it is in and start another.

use std::fmt::Write as _;

/// The title shown with no session title.
pub const APP_TITLE: &str = "CodeSmith";

/// Text with every run of C0 and C1 control characters (newlines too)
/// replaced by a space, as OpenCode's `setTerminalTitle` does.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = false;
    for c in s.chars() {
        if c.is_control() {
            if !run {
                out.push(' ');
            }
            run = true;
        } else {
            out.push(c);
            run = false;
        }
    }
    out
}

/// The window title for a session titled `title`: `CS | <title>` (the
/// title cut to 40 characters), or `CodeSmith` while it has none.
pub fn window_title(title: Option<&str>) -> String {
    let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) else {
        return APP_TITLE.into();
    };
    let t = clean(t);
    if t.chars().count() > 40 {
        let cut: String = t.chars().take(37).collect();
        format!("CS | {cut}…")
    } else {
        format!("CS | {t}")
    }
}

/// The escape sequence that sets the window title (empty clears it).
pub fn set_title(title: &str) -> String {
    format!("\x1b]0;{}\x07", clean(title))
}

/// The escape sequence that puts `text` on the clipboard (`OSC 52`).
pub fn copy(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";

/// What is printed after the UI closes, as OpenCode's `epilogue`: the
/// session and the command that continues it, and, after an update, that
/// the new version needs a restart. Nothing when there is neither.
pub fn epilogue(
    session: Option<(&str, &str)>,
    command: &str,
    updated: Option<&str>,
) -> Option<String> {
    if session.is_none() && updated.is_none() {
        return None;
    }
    let row = |label: &str, value: &str| format!("  {DIM}{label:<10}{RESET}{BOLD}{value}{RESET}");
    let mut out = vec![String::new()];
    if let Some(v) = updated {
        let v = clean(v);
        out.push(format!(
            "  {DIM}Run {RESET}{BOLD}codesmith{RESET}{DIM} to use {RESET}{BOLD}{v}{RESET}"
        ));
        out.push(String::new());
    }
    if let Some((title, id)) = session {
        out.push(row("Session", &clean(title)));
        out.push(row("Continue", &format!("{command} -s {}", clean(id))));
        out.push(String::new());
    }
    Some(out.join("\n"))
}

/// The session states the program status reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Working,
    /// Waiting for its user: a permission or a question, and what it asks.
    Blocked {
        kind: Kind,
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Question => "question",
        }
    }
}

/// A session that is working or waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSession {
    pub id: String,
    pub title: Option<String>,
    pub state: Pending,
}

/// One program status report, as OpenCode's `ProgramStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// `idle`, `working`, `blocked` or `clear`.
    pub state: &'static str,
    pub kind: Option<&'static str>,
    pub id: Option<String>,
    pub app: Option<&'static str>,
    pub title: Option<String>,
    pub message: Option<String>,
}

const APP: &str = "codesmith";
/// The root record's key.
pub const ROOT: &str = "";

/// Text on one line without control characters, cut to `bytes` bytes
/// (with an ellipsis, on a character boundary).
fn clip(input: Option<&str>, bytes: usize) -> Option<String> {
    let text = clean(input.unwrap_or(""));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    if text.len() <= bytes {
        return Some(text);
    }
    let mut cut = bytes - 3;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &text[..cut]))
}

/// A record ID within the protocol's rules (1 to 32 of `[A-Za-z0-9_.+-]`):
/// the session ID, or a hash of it.
fn record_id(id: &str) -> String {
    let ok = (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if ok {
        return id.to_string();
    }
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(id.as_bytes());
    let mut hex = String::new();
    for b in &h[..8] {
        let _ = write!(hex, "{b:02x}");
    }
    format!("ses_{hex}")
}

fn record(s: &PendingSession, id: Option<String>) -> Status {
    let title = clip(s.title.as_deref(), 192);
    match &s.state {
        Pending::Working => Status {
            state: "working",
            id,
            app: Some(APP),
            title,
            ..Default::default()
        },
        Pending::Blocked { kind, message } => Status {
            state: "blocked",
            kind: Some(kind.key()),
            id,
            app: Some(APP),
            title,
            message: clip(message.as_deref(), 2048),
        },
    }
}

fn root(sessions: &[PendingSession]) -> Status {
    let blocked: Vec<&PendingSession> = sessions
        .iter()
        .filter(|s| matches!(s.state, Pending::Blocked { .. }))
        .collect();
    let working: Vec<&PendingSession> = sessions
        .iter()
        .filter(|s| s.state == Pending::Working)
        .collect();
    let kind = |s: &PendingSession| match &s.state {
        Pending::Blocked { kind, .. } => Some(*kind),
        Pending::Working => None,
    };
    match (blocked.len(), working.len()) {
        (1, _) => record(blocked[0], None),
        (n, _) if n > 1 => Status {
            state: "blocked",
            kind: blocked
                .iter()
                .all(|s| kind(s) == kind(blocked[0]))
                .then(|| kind(blocked[0]).map(Kind::key))
                .flatten(),
            app: Some(APP),
            message: Some(format!("{n} sessions need input")),
            ..Default::default()
        },
        (_, 1) => record(working[0], None),
        (_, n) if n > 1 => Status {
            state: "working",
            app: Some(APP),
            message: Some(format!("{n} sessions working")),
            ..Default::default()
        },
        _ => Status {
            state: "idle",
            app: Some(APP),
            ..Default::default()
        },
    }
}

/// The reports for `sessions`: the root summary first (key [`ROOT`]), then
/// one record per session, by record ID.
pub fn reports(sessions: &[PendingSession]) -> Vec<(String, Status)> {
    let mut out = vec![(ROOT.to_string(), root(sessions))];
    for s in sessions {
        let id = record_id(&s.id);
        if let Some(at) = out.iter().position(|(k, _)| *k == id) {
            out.remove(at);
        }
        out.push((id.clone(), record(s, Some(id))));
    }
    out
}

/// Program status as it was last sent, to send only what changed: a
/// record that went away is cleared, a changed one sent again.
#[derive(Debug, Default)]
pub struct ProgramStatus {
    sent: Vec<(String, Status)>,
}

impl ProgramStatus {
    /// What to send for the sessions pending now.
    pub fn update(&mut self, sessions: &[PendingSession]) -> Vec<Status> {
        let now = reports(sessions);
        let mut out = Vec::new();
        for (k, _) in &self.sent {
            if k != ROOT && !now.iter().any(|(n, _)| n == k) {
                out.push(Status {
                    state: "clear",
                    id: Some(k.clone()),
                    ..Default::default()
                });
            }
        }
        for (k, s) in &now {
            if self.sent.iter().find(|(o, _)| o == k).map(|(_, v)| v) != Some(s) {
                out.push(s.clone());
            }
        }
        self.sent = now;
        out
    }

    /// The UI is closing: clear the root.
    pub fn close(&mut self) -> Status {
        self.sent.clear();
        Status {
            state: "clear",
            ..Default::default()
        }
    }
}

/// The terminal's progress report (`OSC 9;4`) for the root status, which
/// the terminals that know it show on the tab or the taskbar: busy while
/// a session works, paused while one waits for its user. OpenCode's own
/// records (`OSC 7501`, through OpenTUI) have no published encoding, so
/// CodeSmith reports the root summary only, and only to terminals known
/// to read `OSC 9;4` (elsewhere `OSC 9` is a notification).
pub fn progress(s: &Status) -> Option<&'static str> {
    if s.id.is_some() {
        return None;
    }
    Some(match s.state {
        "working" => "\x1b]9;4;3;0\x07",
        "blocked" => "\x1b]9;4;4;100\x07",
        _ => "\x1b]9;4;0;0\x07",
    })
}

/// Whether the terminal reads `OSC 9;4` progress reports.
pub fn reads_progress(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("WT_SESSION").is_some()
        || env("ConEmuPID").is_some()
        || env("TERM_PROGRAM").is_some_and(|t| t.eq_ignore_ascii_case("ghostty"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_their_control_characters() {
        assert_eq!(set_title("a\x1b]0;evil\x07\nb"), "\x1b]0;a ]0;evil b\x07");
        assert_eq!(window_title(None), "CodeSmith");
        assert_eq!(window_title(Some("  ")), "CodeSmith");
        assert_eq!(window_title(Some("Demo session")), "CS | Demo session");
        let long = "x".repeat(41);
        assert_eq!(
            window_title(Some(&long)),
            format!("CS | {}…", "x".repeat(37))
        );
    }

    #[test]
    fn progress_reports_only_the_root() {
        let root = Status {
            state: "working",
            ..Default::default()
        };
        assert_eq!(progress(&root), Some("\x1b]9;4;3;0\x07"));
        let rec = Status {
            state: "working",
            id: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(progress(&rec), None);
        let env = |k: &str| (k == "TERM_PROGRAM").then(|| "ghostty".to_string());
        assert!(reads_progress(&env));
        assert!(!reads_progress(&|_| None));
    }
}
