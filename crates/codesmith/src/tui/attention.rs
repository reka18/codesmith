//! Telling the user something needs them while they look elsewhere:
//! system notifications (`OSC 9` or `OSC 777`, which the terminal shows as
//! a desktop notification) and sounds, as OpenCode's notifications plugin
//! and attention host decide them. A permission request, a question, a
//! finished turn and a failed one each notify once; a subagent's only
//! sounds. Notifications are shown while the terminal is out of focus,
//! sounds always; both are off unless `tui.attention` turns them on.

use super::settings::Attention as Settings;

/// An attention sound, by OpenCode's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// For a request naming none (OpenCode's plugins); nothing here does.
    #[allow(dead_code)]
    Default,
    Question,
    Permission,
    Error,
    Done,
    SubagentDone,
}

impl Sound {
    pub fn key(self) -> &'static str {
        match self {
            Sound::Default => "default",
            Sound::Question => "question",
            Sound::Permission => "permission",
            Sound::Error => "error",
            Sound::Done => "done",
            Sound::SubagentDone => "subagent_done",
        }
    }
}

/// One notify request, as the plugin makes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Notify {
    pub title: Option<String>,
    pub message: String,
    /// Show a system notification while the terminal is out of focus
    /// (subagents only sound).
    pub notification: bool,
    /// Played whatever the focus.
    pub sound: Sound,
}

/// A failure shown in the UI too (OpenCode's session-scoped toast).
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub session: String,
    pub title: String,
    pub message: String,
}

/// What the notifier asks about a session.
pub trait Sessions {
    fn title(&self, session: &str) -> Option<String>;
    /// It is a subagent's session.
    fn is_child(&self, session: &str) -> bool;
    /// One of its direct subagents still runs (its report wakes the
    /// session again, so it is not done yet).
    fn running_child(&self, session: &str) -> bool;
}

/// What happened, as the plugin hears it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    FormCreated {
        id: &'a str,
        session: &'a str,
        title: Option<&'a str>,
    },
    FormResolved(&'a str),
    PermissionAsked {
        id: &'a str,
        session: &'a str,
    },
    PermissionReplied(&'a str),
    Started(&'a str),
    Succeeded(&'a str),
    Interrupted(&'a str),
    /// A failed turn and its error; `limit` for a usage limit, which the
    /// UI explains itself.
    Failed {
        session: &'a str,
        message: &'a str,
        limit: bool,
    },
}

/// OpenCode's notifications plugin: which events notify, deduplicated.
#[derive(Debug, Default)]
pub struct Notifier {
    errored: std::collections::HashSet<String>,
    terminal: std::collections::HashSet<String>,
    forms: std::collections::HashSet<String>,
    permissions: std::collections::HashSet<String>,
    pub notifications: Vec<Notify>,
    pub toasts: Vec<Toast>,
}

impl Notifier {
    fn notify(
        &mut self,
        sessions: &dyn Sessions,
        session: &str,
        message: &str,
        sound: Sound,
        title: Option<&str>,
    ) {
        self.notifications.push(Notify {
            title: title
                .map(str::to_string)
                .or_else(|| sessions.title(session)),
            message: message.to_string(),
            notification: !sessions.is_child(session),
            sound,
        });
    }

    fn ended(&mut self, sessions: &dyn Sessions, session: &str) {
        if !self.terminal.insert(session.to_string()) {
            return;
        }
        if self.errored.remove(session) {
            return;
        }
        let sound = if sessions.is_child(session) {
            Sound::SubagentDone
        } else {
            Sound::Done
        };
        self.notify(sessions, session, "Session done", sound, None);
    }

    pub fn on(&mut self, sessions: &dyn Sessions, ev: Event) {
        match ev {
            Event::FormCreated { id, session, title } => {
                if self.forms.insert(id.to_string()) {
                    self.notify(
                        sessions,
                        session,
                        "Input needs response",
                        Sound::Question,
                        title,
                    );
                }
            }
            Event::FormResolved(id) => {
                self.forms.remove(id);
            }
            Event::PermissionAsked { id, session } => {
                if self.permissions.insert(id.to_string()) {
                    self.notify(
                        sessions,
                        session,
                        "Permission needs input",
                        Sound::Permission,
                        None,
                    );
                }
            }
            Event::PermissionReplied(id) => {
                self.permissions.remove(id);
            }
            Event::Started(s) => {
                self.errored.remove(s);
                self.terminal.remove(s);
            }
            Event::Succeeded(s) => {
                if !sessions.is_child(s) && sessions.running_child(s) {
                    return;
                }
                self.ended(sessions, s);
            }
            Event::Interrupted(s) => self.ended(sessions, s),
            Event::Failed {
                session,
                message,
                limit,
            } => {
                if self.terminal.contains(session) {
                    return;
                }
                if self.errored.contains(session) {
                    return self.ended(sessions, session);
                }
                self.errored.insert(session.to_string());
                if !limit {
                    self.notify(sessions, session, message, Sound::Error, None);
                    self.toasts.push(Toast {
                        session: session.to_string(),
                        title: "Session failed".into(),
                        message: message.to_string(),
                    });
                }
                self.ended(sessions, session);
            }
        }
    }
}

/// Whether the terminal has the focus, once it has said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Unknown,
    Focused,
    Blurred,
}

const DEFAULT_TITLE: &str = "CodeSmith";
const TITLE_LIMIT: usize = 80;
const MESSAGE_LIMIT: usize = 240;

/// Text for a notification: escape sequences and control characters
/// dropped, lines joined, at most `limit` characters (`fallback` if none
/// are left), as OpenCode's `normalizeText`.
pub fn normalize(input: Option<&str>, fallback: &str, limit: usize) -> String {
    let text = super::app::strip_escapes(input.unwrap_or(""));
    let mut joined = String::with_capacity(text.len());
    for (i, line) in text
        .split(['\r', '\n'])
        .filter(|l| !l.trim().is_empty())
        .enumerate()
    {
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(line.trim_matches([' ', '\t']));
    }
    let text: String = joined.chars().filter(|c| !c.is_control()).collect();
    let text = text.trim();
    let text = if text.is_empty() { fallback } else { text };
    text.chars().take(limit).collect()
}

/// How the terminal shows a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// `OSC 9`: iTerm2, WezTerm, Ghostty, kitty, Windows Terminal.
    Osc9,
    /// `OSC 777;notify`: VTE terminals (GNOME Terminal and others), foot,
    /// urxvt; it carries the title as well.
    Osc777,
}

impl Protocol {
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Protocol {
        let term = env("TERM").unwrap_or_default();
        if env("VTE_VERSION").is_some() || term.starts_with("foot") || term.starts_with("rxvt") {
            Protocol::Osc777
        } else {
            Protocol::Osc9
        }
    }
}

/// A notification's escape sequence. Inside tmux it is passed through to
/// the terminal around it.
pub fn sequence(protocol: Protocol, title: &str, message: &str, tmux: bool) -> String {
    let seq = match protocol {
        Protocol::Osc9 => format!("\x1b]9;{message}\x07"),
        Protocol::Osc777 => format!("\x1b]777;notify;{};{message}\x07", title.replace(';', ",")),
    };
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    }
}

/// What a notify request comes to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    /// The notification to show (its sequence).
    pub notification: Option<String>,
    /// The sound to play, and the file configured for it.
    pub sound: Option<(Sound, Option<String>)>,
}

/// OpenCode's attention host: a request shown and played as the settings
/// and the focus allow.
pub fn decide(
    req: &Notify,
    settings: &Settings,
    focus: Focus,
    protocol: Protocol,
    tmux: bool,
) -> Outcome {
    let mut out = Outcome::default();
    if !settings.notifications && !settings.sound {
        return out;
    }
    let message = normalize(Some(&req.message), "", MESSAGE_LIMIT);
    if message.is_empty() {
        return out;
    }
    if settings.notifications && req.notification && focus == Focus::Blurred {
        let title = normalize(req.title.as_deref(), DEFAULT_TITLE, TITLE_LIMIT);
        out.notification = Some(sequence(protocol, &title, &message, tmux));
    }
    if settings.sound && settings.volume > 0.0 {
        out.sound = Some((req.sound, settings.sounds.get(req.sound.key()).cloned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    /// The sessions of OpenCode's test harness.
    struct Harness {
        running: RefCell<HashSet<&'static str>>,
    }

    const SESSIONS: &[(&str, &str, Option<&str>)] = &[
        ("session", "Demo session", None),
        ("subagent", "Subagent session", Some("session")),
        ("second", "Second subagent session", Some("session")),
        ("nested", "Nested subagent session", Some("subagent")),
    ];

    impl Sessions for Harness {
        fn title(&self, session: &str) -> Option<String> {
            SESSIONS
                .iter()
                .find(|s| s.0 == session)
                .map(|s| s.1.to_string())
        }
        fn is_child(&self, session: &str) -> bool {
            SESSIONS.iter().any(|s| s.0 == session && s.2.is_some())
        }
        fn running_child(&self, session: &str) -> bool {
            SESSIONS
                .iter()
                .any(|s| s.2 == Some(session) && self.running.borrow().contains(s.0))
        }
    }

    fn harness() -> (Harness, Notifier) {
        (
            Harness {
                running: RefCell::default(),
            },
            Notifier::default(),
        )
    }

    fn n(title: &str, message: &str, notification: bool, sound: Sound) -> Notify {
        Notify {
            title: Some(title.into()),
            message: message.into(),
            notification,
            sound,
        }
    }

    fn failed(session: &str) -> Event<'_> {
        Event::Failed {
            session,
            message: "boom",
            limit: false,
        }
    }

    // notifications.test.ts: "shows execution failures as session-scoped
    // toasts without needing an assistant message"
    #[test]
    fn oc_shows_execution_failures_as_session_scoped_toasts() {
        let (h, mut n) = harness();
        n.on(&h, Event::Started("session"));
        n.on(&h, failed("session"));
        n.on(&h, failed("session"));
        let toast = Toast {
            session: "session".into(),
            title: "Session failed".into(),
            message: "boom".into(),
        };
        assert_eq!(n.toasts, vec![toast]);
        n.on(&h, Event::Started("session"));
        n.on(&h, failed("session"));
        assert_eq!(n.toasts.len(), 2);
        // A usage limit is neither toasted nor notified.
        n.on(&h, Event::Started("session"));
        n.on(
            &h,
            Event::Failed {
                session: "session",
                message: "Free usage limit reached.",
                limit: true,
            },
        );
        assert_eq!(n.toasts.len(), 2);
        assert_eq!(n.notifications.len(), 2);
    }

    // notifications.test.ts: "notifies for form and permission requests with
    // blurred notifications and always-on sounds"
    #[test]
    fn oc_notifies_for_forms_and_permissions() {
        let (h, mut n) = harness();
        n.on(
            &h,
            Event::FormCreated {
                id: "form-1",
                session: "session",
                title: Some("Confirm deployment"),
            },
        );
        n.on(
            &h,
            Event::PermissionAsked {
                id: "permission-1",
                session: "session",
            },
        );
        assert_eq!(
            n.notifications,
            vec![
                super::Notify {
                    title: Some("Confirm deployment".into()),
                    message: "Input needs response".into(),
                    notification: true,
                    sound: Sound::Question
                },
                self::n(
                    "Demo session",
                    "Permission needs input",
                    true,
                    Sound::Permission
                ),
            ]
        );
    }

    // notifications.test.ts: "notifies for global forms once the TUI can render them"
    #[test]
    fn oc_notifies_for_global_forms() {
        let (h, mut n) = harness();
        n.on(
            &h,
            Event::FormCreated {
                id: "form-1",
                session: "global",
                title: Some("demo-mcp is requesting input"),
            },
        );
        assert_eq!(
            n.notifications,
            vec![self::n(
                "demo-mcp is requesting input",
                "Input needs response",
                true,
                Sound::Question
            )]
        );
    }

    // notifications.test.ts: "dedupes pending forms and permissions until they are resolved"
    #[test]
    fn oc_dedupes_pending_forms_and_permissions_until_resolved() {
        let (h, mut n) = harness();
        let form = Event::FormCreated {
            id: "form-1",
            session: "session",
            title: Some("Input requested"),
        };
        n.on(&h, form.clone());
        n.on(&h, form.clone());
        n.on(&h, Event::FormResolved("form-1"));
        n.on(&h, form);
        let ask = Event::PermissionAsked {
            id: "permission-1",
            session: "session",
        };
        n.on(&h, ask.clone());
        n.on(&h, ask.clone());
        n.on(&h, Event::PermissionReplied("permission-1"));
        n.on(&h, ask);
        let f = self::n(
            "Input requested",
            "Input needs response",
            true,
            Sound::Question,
        );
        let p = self::n(
            "Demo session",
            "Permission needs input",
            true,
            Sound::Permission,
        );
        assert_eq!(n.notifications, vec![f.clone(), f, p.clone(), p]);
    }

    // notifications.test.ts: "notifies for terminal lifecycle events even
    // when attached after execution started"
    #[test]
    fn oc_notifies_for_lifecycle_events_attached_after_start() {
        let (h, mut n) = harness();
        n.on(&h, Event::Succeeded("session"));
        n.on(&h, Event::Started("session"));
        n.on(&h, Event::Succeeded("session"));
        let done = self::n("Demo session", "Session done", true, Sound::Done);
        assert_eq!(n.notifications, vec![done.clone(), done]);
    }

    // notifications.test.ts: "uses sound-only notifications and
    // subagent_done sound for subagent sessions"
    #[test]
    fn oc_subagents_only_sound() {
        let (h, mut n) = harness();
        n.on(
            &h,
            Event::FormCreated {
                id: "form-1",
                session: "subagent",
                title: Some("Questions"),
            },
        );
        n.on(&h, Event::Started("subagent"));
        n.on(&h, Event::Succeeded("subagent"));
        assert_eq!(
            n.notifications,
            vec![
                self::n("Questions", "Input needs response", false, Sound::Question),
                self::n(
                    "Subagent session",
                    "Session done",
                    false,
                    Sound::SubagentDone
                ),
            ]
        );
    }

    // notifications.test.ts: "plays done only once none of the session's
    // direct children is running"
    #[test]
    fn oc_plays_done_once_no_direct_child_runs() {
        let (h, mut n) = harness();
        h.running
            .borrow_mut()
            .extend(["subagent", "second", "nested"]);
        n.on(&h, Event::Started("session"));
        n.on(&h, Event::Succeeded("session"));
        h.running.borrow_mut().remove("subagent");
        n.on(&h, Event::Started("session"));
        n.on(&h, Event::Succeeded("session"));
        assert!(n.notifications.is_empty());
        h.running.borrow_mut().remove("second");
        n.on(&h, Event::Started("session"));
        n.on(&h, Event::Succeeded("session"));
        assert_eq!(
            n.notifications,
            vec![self::n("Demo session", "Session done", true, Sound::Done)]
        );
    }

    // notifications.test.ts: "notifies session errors once and suppresses
    // the following idle done notification"
    #[test]
    fn oc_notifies_errors_once_and_suppresses_the_done() {
        let (h, mut n) = harness();
        n.on(&h, Event::Started("session"));
        n.on(&h, failed("session"));
        n.on(&h, Event::Succeeded("session"));
        assert_eq!(
            n.notifications,
            vec![self::n("Demo session", "boom", true, Sound::Error)]
        );
    }

    // config.test.ts: "defines attention sound names"
    #[test]
    fn oc_defines_attention_sound_names() {
        assert_eq!(
            super::super::settings::SOUND_NAMES,
            &[
                "default",
                "question",
                "permission",
                "error",
                "done",
                "subagent_done"
            ]
        );
        let all = [
            Sound::Default,
            Sound::Question,
            Sound::Permission,
            Sound::Error,
            Sound::Done,
            Sound::SubagentDone,
        ];
        let keys: Vec<&str> = all.iter().map(|s| s.key()).collect();
        assert_eq!(keys, super::super::settings::SOUND_NAMES);
    }

    #[test]
    fn notifications_wait_for_a_blurred_terminal_and_sounds_do_not() {
        let on = Settings {
            notifications: true,
            sound: true,
            ..Settings::default()
        };
        let req = self::n("Fix\x1b]0;x\x07 it", "Session\ndone\x07", true, Sound::Done);
        let shown = decide(&req, &on, Focus::Blurred, Protocol::Osc777, false);
        assert_eq!(
            shown.notification.as_deref(),
            Some("\x1b]777;notify;Fix it;Session done\x07")
        );
        assert_eq!(shown.sound, Some((Sound::Done, None)));
        for focus in [Focus::Focused, Focus::Unknown] {
            let o = decide(&req, &on, focus, Protocol::Osc9, false);
            assert_eq!(o.notification, None);
            assert!(o.sound.is_some());
        }
        let off = Settings::default();
        assert_eq!(
            decide(&req, &off, Focus::Blurred, Protocol::Osc9, false),
            Outcome::default()
        );
        assert_eq!(
            sequence(Protocol::Osc9, "t", "m", true),
            "\x1bPtmux;\x1b\x1b]9;m\x07\x1b\\"
        );
    }
}
