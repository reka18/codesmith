//! The UI's output outside its frames, kept current before each one: the
//! window title (`tui.terminal.title`), notifications and sounds
//! (`tui.attention`) and the program status. Every open tab is watched,
//! shown or not, so a background tab's permission request or finished
//! turn notifies too.

use super::app::{ChildMark, TurnEnd};
use super::attention::{self, Event, Focus, Notifier, Protocol, Sessions, Sound};
use super::settings::Attention;
use super::term::{self, Kind, Pending, PendingSession, ProgramStatus};
use std::collections::{HashMap, HashSet};

/// How a tab stands, as the runner reads it before a frame.
#[derive(Debug, Clone, Default)]
pub struct TabView {
    /// The tab (stable while it is open).
    pub key: u64,
    pub session: String,
    pub title: Option<String>,
    pub busy: bool,
    /// The permission request or question it shows, and its number.
    pub ask: Option<(Kind, u64)>,
    /// How its last turn ended, if it did since the last frame.
    pub ended: Option<TurnEnd>,
    /// Its subagents that started or ended since the last frame.
    pub children: Vec<ChildMark>,
    /// A subagent of its session still runs.
    pub running_child: bool,
}

#[derive(Debug, Default)]
struct Seen {
    busy: bool,
    ask: Option<(Kind, u64)>,
}

pub struct Outside {
    pub title_on: bool,
    pub attention: Attention,
    pub focus: Focus,
    protocol: Protocol,
    tmux: bool,
    /// The terminal reads progress reports.
    progress: bool,
    title: Option<String>,
    status: ProgramStatus,
    notifier: Notifier,
    seen: HashMap<u64, Seen>,
    /// Subagent sessions, by ID, with their titles.
    children: HashMap<String, String>,
    /// Bytes to write before the next frame.
    pub out: String,
    /// Sounds to play, with the file configured for each.
    pub sounds: Vec<(Sound, Option<String>)>,
}

struct Known<'a> {
    views: &'a [TabView],
    children: &'a HashMap<String, String>,
}

impl Sessions for Known<'_> {
    fn title(&self, session: &str) -> Option<String> {
        if let Some(t) = self.children.get(session) {
            return Some(t.clone());
        }
        let v = self.views.iter().find(|v| v.session == session)?;
        Some(
            v.title
                .clone()
                .unwrap_or_else(|| super::tabs::NEW_SESSION_TITLE.into()),
        )
    }
    fn is_child(&self, session: &str) -> bool {
        self.children.contains_key(session)
    }
    fn running_child(&self, session: &str) -> bool {
        self.views
            .iter()
            .any(|v| v.session == session && v.running_child)
    }
}

impl Outside {
    pub fn new(title_on: bool, attention: Attention, env: &dyn Fn(&str) -> Option<String>) -> Self {
        Outside {
            title_on,
            attention,
            focus: Focus::Unknown,
            protocol: Protocol::detect(env),
            tmux: env("TMUX").is_some(),
            progress: term::reads_progress(env),
            title: None,
            status: ProgramStatus::default(),
            notifier: Notifier::default(),
            seen: HashMap::new(),
            children: HashMap::new(),
            out: String::new(),
            sounds: Vec::new(),
        }
    }

    /// Bring everything up to date with the open tabs, the shown one's
    /// title giving the window's.
    pub fn observe(&mut self, mut views: Vec<TabView>, shown_title: Option<&str>) {
        let open: HashSet<u64> = views.iter().map(|v| v.key).collect();
        self.seen.retain(|k, _| open.contains(k));
        let mut events: Vec<(String, EventKind)> = Vec::new();
        for v in &mut views {
            let seen = self.seen.entry(v.key).or_default();
            if seen.ask != v.ask {
                if let Some((kind, n)) = seen.ask {
                    events.push((ask_id(v.key, n), EventKind::Resolved(kind)));
                }
                if let Some((kind, n)) = v.ask {
                    events.push((v.session.clone(), EventKind::Asked(kind, ask_id(v.key, n))));
                }
                seen.ask = v.ask;
            }
            for c in std::mem::take(&mut v.children) {
                match c {
                    ChildMark::Started { id, title } => {
                        self.children.insert(id.clone(), title);
                        events.push((id, EventKind::Started));
                    }
                    ChildMark::Ended { id, end } => events.push((id, EventKind::Ended(end))),
                }
            }
            match v.ended.take() {
                Some(end) => {
                    if !seen.busy {
                        events.push((v.session.clone(), EventKind::Started));
                    }
                    events.push((v.session.clone(), EventKind::Ended(end)));
                    if v.busy {
                        events.push((v.session.clone(), EventKind::Started));
                    }
                }
                None if v.busy && !seen.busy => {
                    events.push((v.session.clone(), EventKind::Started))
                }
                None => {}
            }
            seen.busy = v.busy;
        }
        let known = Known {
            views: &views,
            children: &self.children,
        };
        for (session, kind) in &events {
            let ev = match kind {
                EventKind::Asked(Kind::Permission, id) => Event::PermissionAsked { id, session },
                EventKind::Asked(Kind::Question, id) => Event::FormCreated {
                    id,
                    session,
                    title: None,
                },
                EventKind::Resolved(Kind::Permission) => Event::PermissionReplied(session),
                EventKind::Resolved(Kind::Question) => Event::FormResolved(session),
                EventKind::Started => Event::Started(session),
                EventKind::Ended(TurnEnd::Done) => Event::Succeeded(session),
                EventKind::Ended(TurnEnd::Interrupted) => Event::Interrupted(session),
                EventKind::Ended(TurnEnd::Failed(m)) => Event::Failed {
                    session,
                    message: m,
                    limit: false,
                },
            };
            self.notifier.on(&known, ev);
        }
        self.notifier.toasts.clear();
        for req in std::mem::take(&mut self.notifier.notifications) {
            let o = attention::decide(&req, &self.attention, self.focus, self.protocol, self.tmux);
            if let Some(n) = o.notification {
                self.out.push_str(&n);
            }
            self.sounds.extend(o.sound);
        }
        let pending: Vec<PendingSession> = views
            .iter()
            .filter_map(|v| {
                let state = match v.ask {
                    Some((kind, _)) => Pending::Blocked {
                        kind,
                        message: None,
                    },
                    None if v.busy => Pending::Working,
                    None => return None,
                };
                Some(PendingSession {
                    id: v.session.clone(),
                    title: v.title.clone(),
                    state,
                })
            })
            .collect();
        for s in self.status.update(&pending) {
            if let Some(seq) = term::progress(&s).filter(|_| self.progress) {
                self.out.push_str(seq);
            }
        }
        self.set_title(shown_title);
    }

    fn set_title(&mut self, shown: Option<&str>) {
        if !self.title_on {
            return;
        }
        let t = term::window_title(shown);
        if self.title.as_deref() != Some(t.as_str()) {
            self.out.push_str(&term::set_title(&t));
            self.title = Some(t);
        }
    }

    /// `terminal.title.toggle`: stop (clearing the title) or start
    /// keeping the window title.
    pub fn toggle_title(&mut self) -> bool {
        self.title_on = !self.title_on;
        if !self.title_on && self.title.take().is_some() {
            self.out.push_str(&term::set_title(""));
        }
        self.title_on
    }

    /// The UI is closing: clear the title and the status it set.
    pub fn close(&mut self) {
        if self.title.take().is_some() {
            self.out.push_str(&term::set_title(""));
        }
        let s = self.status.close();
        if let Some(seq) = term::progress(&s).filter(|_| self.progress) {
            self.out.push_str(seq);
        }
    }
}

enum EventKind {
    /// A permission request or question, and its ID.
    Asked(Kind, String),
    /// One was answered; the event's session is its ID.
    Resolved(Kind),
    Started,
    Ended(TurnEnd),
}

fn ask_id(tab: u64, n: u64) -> String {
    format!("{tab}-{n}")
}

/// Play a sound file, for at most ten seconds, with the platform's player.
pub fn play(file: String, volume: f64) {
    tokio::spawn(async move {
        let mut cmd = if cfg!(target_os = "macos") {
            let mut c = tokio::process::Command::new("afplay");
            c.arg("-v").arg(format!("{volume:.2}")).arg(&file);
            c
        } else {
            let mut c = tokio::process::Command::new("paplay");
            c.arg(&file);
            c
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut child) = cmd.spawn() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Outside {
        let mut o = Outside::new(
            true,
            Attention {
                notifications: true,
                sound: true,
                ..Attention::default()
            },
            &|_| None,
        );
        o.focus = Focus::Blurred;
        o
    }

    fn view(key: u64, busy: bool) -> TabView {
        TabView {
            key,
            session: format!("ses_{key}"),
            title: Some(format!("Tab {key}")),
            busy,
            ..Default::default()
        }
    }

    #[test]
    fn a_turn_that_ends_notifies_and_the_title_follows_the_shown_tab() {
        let mut o = on();
        o.observe(vec![view(1, true)], Some("Tab 1"));
        assert_eq!(o.out, "\x1b]0;CS | Tab 1\x07");
        o.out.clear();
        let mut v = view(1, false);
        v.ended = Some(TurnEnd::Done);
        o.observe(vec![v], Some("Tab 1"));
        assert_eq!(o.out, "\x1b]9;Session done\x07");
        assert_eq!(o.sounds, vec![(Sound::Done, None)]);
        o.out.clear();
        o.close();
        assert_eq!(o.out, "\x1b]0;\x07");
    }

    #[test]
    fn a_background_tabs_permission_request_notifies_once() {
        let mut o = on();
        o.title_on = false;
        let mut v = view(2, true);
        v.ask = Some((Kind::Permission, 1));
        o.observe(vec![view(1, false), v.clone()], None);
        o.observe(vec![view(1, false), v], None);
        assert_eq!(o.out, "\x1b]9;Permission needs input\x07");
        assert_eq!(o.sounds, vec![(Sound::Permission, None)]);
    }

    #[test]
    fn a_subagent_only_sounds() {
        let mut o = on();
        o.title_on = false;
        let mut v = view(1, true);
        v.children = vec![ChildMark::Started {
            id: "ses_child".into(),
            title: "Find it".into(),
        }];
        o.observe(vec![v], None);
        let mut v = view(1, true);
        v.children = vec![ChildMark::Ended {
            id: "ses_child".into(),
            end: TurnEnd::Done,
        }];
        o.observe(vec![v], None);
        assert_eq!(o.out, "");
        assert_eq!(o.sounds, vec![(Sound::SubagentDone, None)]);
    }
}
