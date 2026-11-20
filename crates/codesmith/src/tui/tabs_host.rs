//! Running several sessions as tabs. The runner holds the shown tab's
//! state (its app, session, turn, background commands, prompter) in its
//! own fields, as it did for its one session; every other open tab keeps
//! the same state in a [`Slot`]. Acting on a background tab (one of its
//! session's events, a wake-up for a finished command) swaps its slot in,
//! runs the runner's usual code, and swaps it back, so a background tab
//! goes on with its turn, answers its commands' results and resumes by
//! itself as the shown one does. Each tab's session talks to the UI on a
//! channel of its own, whose events come in tagged with the tab
//! ([`Ev::Tab`]).
//!
//! As in OpenCode, tabs are kept across restarts (per working directory
//! unless `tui.tabs.scope` is `global`); a restored tab loads its session
//! when it is first shown. Closing a tab stops its turn and its background
//! commands; switching away from one does not.

use super::app::{App, Modal};
use super::bridge::{ChanPrompter, Ev};
use super::tabs::{self, Attention, Item, Saved, Status, Strip, Tab, Unread};
use super::{open_store, Runner};
use codesmith_core::permission::{Approvals, Asker, Fixed, Reply};
use codesmith_core::question::Questioner;
use codesmith_core::session::Session;
use codesmith_core::store;
use codesmith_core::tool::shell::Jobs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;

/// A tab's own part of the runner, while another tab is shown. The
/// fields are the runner's of the same names.
pub(super) struct Slot {
    app: App,
    tx: UnboundedSender<Ev>,
    asker: Arc<dyn Asker>,
    questioner: Option<Arc<dyn Questioner>>,
    prompter: Option<Arc<ChanPrompter>>,
    agent: Option<String>,
    model: Option<String>,
    session: Option<Session>,
    session_id: String,
    task: Option<JoinHandle<()>>,
    approvals: Approvals,
    stopping: bool,
    skip_done: usize,
    waiter: Option<JoinHandle<()>>,
    resume_held: Option<usize>,
    jobs: Jobs,
}

/// Where a tab's state is.
enum State {
    /// In the runner: the tab shown.
    Shown,
    Live(Box<Slot>),
    /// Restored or reopened, not shown since: its session loads when it is.
    Dormant,
}

struct Entry {
    /// Tags the events of the tab's session.
    id: u64,
    tab: Tab,
    unread: Option<Unread>,
    /// When it was last shown (ms), kept across restarts.
    viewed: Option<i64>,
    /// Its session is in the store (it has a message, or was loaded).
    saved: bool,
    state: State,
}

/// A new tab's channel and prompters.
pub(super) struct Parts {
    pub tx: UnboundedSender<Ev>,
    pub asker: Arc<dyn Asker>,
    pub questioner: Option<Arc<dyn Questioner>>,
    pub prompter: Option<Arc<ChanPrompter>>,
}

pub(super) struct Host {
    pub settings: tabs::Settings,
    entries: Vec<Entry>,
    /// The tab in the runner.
    shown: usize,
    history: tabs::History,
    closed: Vec<tabs::Closed>,
    next_id: u64,
    /// The UI's channel, which tab channels forward to.
    main: UnboundedSender<Ev>,
    /// `--auto`: permissions are granted, questions are not asked.
    auto: bool,
    cwd: PathBuf,
    /// The tabs last written to the state file.
    written: Option<Vec<Saved>>,
    /// The column's line under each title.
    detail: String,
}

impl Host {
    /// Tabs for a UI on `main`. In mini mode, or with tabs off, there is
    /// one session at a time, as before tabs.
    pub fn new(
        main: UnboundedSender<Ev>,
        auto: bool,
        cwd: PathBuf,
        settings: tabs::Settings,
    ) -> Host {
        let root = codesmith_core::config::project_root(&cwd);
        Host {
            detail: tabs::project_detail(&root),
            settings,
            entries: Vec::new(),
            shown: 0,
            history: tabs::History::default(),
            closed: Vec::new(),
            next_id: 0,
            main,
            auto,
            cwd,
            written: None,
        }
    }

    pub fn on(&self) -> bool {
        self.settings.enabled
    }

    /// A channel for tab `id`, forwarding to the UI's tagged with it, and
    /// prompters on it. The forwarder ends with the tab's last sender.
    pub fn parts(&mut self) -> (u64, Parts) {
        let id = self.next_id;
        self.next_id += 1;
        let (tx, mut rx) = unbounded_channel::<Ev>();
        let main = self.main.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if main.send(Ev::Tab(id, Box::new(ev))).is_err() {
                    break;
                }
            }
        });
        let prompter = (!self.auto).then(|| Arc::new(ChanPrompter::new(tx.clone())));
        let asker: Arc<dyn Asker> = match &prompter {
            Some(p) => p.clone(),
            None => Arc::new(Fixed(Reply::Once)),
        };
        let questioner: Option<Arc<dyn Questioner>> =
            prompter.as_ref().map(|p| p.clone() as Arc<dyn Questioner>);
        (
            id,
            Parts {
                tx,
                asker,
                questioner,
                prompter,
            },
        )
    }

    fn slot(&mut self, agent: Option<String>, model: Option<String>) -> (u64, Box<Slot>) {
        let (id, p) = self.parts();
        let slot = Slot {
            app: App::new(),
            tx: p.tx,
            asker: p.asker,
            questioner: p.questioner,
            prompter: p.prompter,
            agent,
            model,
            session: None,
            session_id: String::new(),
            task: None,
            approvals: Approvals::default(),
            stopping: false,
            skip_done: 0,
            waiter: None,
            resume_held: None,
            jobs: Jobs::default(),
        };
        (id, Box::new(slot))
    }

    fn list(&self) -> Vec<Tab> {
        self.entries.iter().map(|e| e.tab.clone()).collect()
    }

    fn index_of(&self, session: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.tab.session == session)
    }

    /// The tabs to keep: those whose session is stored.
    fn to_save(&self) -> Vec<Saved> {
        self.entries
            .iter()
            .filter(|e| e.saved && !e.tab.session.is_empty())
            .map(|e| Saved {
                tab: e.tab.clone(),
                viewed: e.viewed,
            })
            .collect()
    }

    /// Write the open tabs when they changed.
    fn persist(&mut self) {
        if !self.on() {
            return;
        }
        let now = self.to_save();
        if self.written.as_ref() == Some(&now) {
            return;
        }
        let path = tabs::state_file();
        let old = std::fs::read_to_string(&path).ok();
        let text = tabs::with_saved(old.as_deref(), self.settings.scope, &self.cwd, &now);
        let _ = codesmith_core::paths::write_private(&path, text.as_bytes());
        self.written = Some(now);
    }

    /// Whether a tab other than the shown one is waiting for its user,
    /// or unread.
    fn marked(&self, i: usize) -> bool {
        let e = &self.entries[i];
        e.unread.is_some() || matches!(&e.state, State::Live(s) if attention(&s.app).is_some())
    }
}

/// What a tab's open dialog asks of its user.
pub(super) fn attention(app: &App) -> Option<Attention> {
    match app.modal {
        Some(Modal::Permission { .. }) => Some(Attention::Permission),
        Some(Modal::Question { .. }) => Some(Attention::Question),
        _ => None,
    }
}

/// How long a closed tab's turn gets to stop.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// Stop a closed tab's turn and background commands.
fn teardown(mut s: Box<Slot>) {
    if let Some(p) = &s.prompter {
        p.set_stopping(true);
    }
    s.app.cancel_modal();
    if let Some(w) = s.waiter.take() {
        w.abort();
    }
    let task = s.task.take();
    let jobs = std::mem::take(&mut s.jobs);
    drop(s);
    tokio::spawn(async move {
        if let Some(mut t) = task {
            t.abort();
            let _ = tokio::time::timeout(CLOSE_WAIT, &mut t).await;
        }
        jobs.stop_all().await;
    });
}

impl Runner {
    /// Swap the shown tab's state with `s`'s (the shared parts of the app
    /// stay with the shown one).
    fn exchange(&mut self, s: &mut Slot) {
        self.app.hand_over(&mut s.app);
        std::mem::swap(&mut self.app, &mut s.app);
        std::mem::swap(&mut self.tx, &mut s.tx);
        std::mem::swap(&mut self.asker, &mut s.asker);
        std::mem::swap(&mut self.questioner, &mut s.questioner);
        std::mem::swap(&mut self.prompter, &mut s.prompter);
        std::mem::swap(&mut self.agent, &mut s.agent);
        std::mem::swap(&mut self.model, &mut s.model);
        std::mem::swap(&mut self.session, &mut s.session);
        std::mem::swap(&mut self.session_id, &mut s.session_id);
        std::mem::swap(&mut self.task, &mut s.task);
        std::mem::swap(&mut self.approvals, &mut s.approvals);
        std::mem::swap(&mut self.stopping, &mut s.stopping);
        std::mem::swap(&mut self.skip_done, &mut s.skip_done);
        std::mem::swap(&mut self.waiter, &mut s.waiter);
        std::mem::swap(&mut self.resume_held, &mut s.resume_held);
        std::mem::swap(&mut self.jobs, &mut s.jobs);
    }

    /// Open the tabs: the ones kept from last time (when tabs are on) and
    /// the launch session's, tab `id`, which `resumed` says is stored.
    pub(super) fn start_tabs(&mut self, id: u64, resumed: bool) {
        let shown = Entry {
            id,
            tab: Tab {
                session: self.session_id.clone(),
                title: self.app.header.title.clone(),
            },
            unread: None,
            viewed: Some(store::now()),
            saved: resumed,
            state: State::Shown,
        };
        if !self.tabs.on() {
            self.tabs.entries = vec![shown];
            return;
        }
        let path = tabs::state_file();
        let text = std::fs::read_to_string(&path).ok();
        let kept = tabs::saved(text.as_deref(), self.tabs.settings.scope, &self.cwd);
        let db = open_store();
        let mut entries = Vec::new();
        for s in kept {
            // A session deleted since is dropped.
            let Some(info) = db
                .as_ref()
                .and_then(|db| db.get(&s.tab.session).ok().flatten())
            else {
                continue;
            };
            let unread = tabs::unread_since(info.updated, s.viewed);
            let id = self.tabs.next_id;
            self.tabs.next_id += 1;
            entries.push(Entry {
                id,
                tab: Tab {
                    session: s.tab.session,
                    title: info.title.or(s.tab.title),
                },
                unread,
                viewed: s.viewed,
                saved: true,
                state: State::Dormant,
            });
        }
        let at = match entries
            .iter()
            .position(|e| e.tab.session == shown.tab.session)
        {
            Some(i) => {
                entries[i] = Entry {
                    tab: Tab {
                        title: shown.tab.title.clone().or(entries[i].tab.title.take()),
                        ..shown.tab.clone()
                    },
                    ..shown
                };
                i
            }
            None => {
                entries.push(shown);
                entries.len() - 1
            }
        };
        self.tabs.entries = entries;
        self.tabs.shown = at;
        self.tabs.history.record(&self.session_id);
        self.sync_tabs();
    }

    /// Route an event: a tab's to its tab (shown or not), the rest to the
    /// shown one. A closed tab's are dropped (a permission request with
    /// its reply, which refuses it).
    pub(super) async fn dispatch(&mut self, ev: Ev) {
        let Ev::Tab(id, ev) = ev else {
            return self.handle(ev).await;
        };
        let Some(i) = self.tabs.entries.iter().position(|e| e.id == id) else {
            return;
        };
        if i == self.tabs.shown {
            return self.handle(*ev).await;
        }
        let State::Live(mut slot) =
            std::mem::replace(&mut self.tabs.entries[i].state, State::Dormant)
        else {
            return;
        };
        let failed = matches!(&*ev, Ev::Done { result: Err(_), .. });
        let asks = match &*ev {
            Ev::Permission(..) => Some(Attention::Permission),
            Ev::Question(..) => Some(Attention::Question),
            _ => None,
        };
        self.exchange(&mut slot);
        let was_busy = self.app.busy;
        self.handle(*ev).await;
        let ended = was_busy && !self.app.busy;
        let title = self.app.header.title.clone();
        self.exchange(&mut slot);
        let e = &mut self.tabs.entries[i];
        e.state = State::Live(slot);
        if ended {
            e.unread = Some(if failed {
                Unread::Error
            } else {
                Unread::Activity
            });
        }
        if let Some(a) = asks {
            let name = super::display_safe(
                title
                    .as_deref()
                    .or(e.tab.title.as_deref())
                    .unwrap_or(tabs::NEW_SESSION_TITLE),
            );
            let what = match a {
                Attention::Permission => "needs your permission",
                Attention::Question => "has a question for you",
            };
            let key = self.app.keymap.hint("session.tab.next_unread");
            self.app.notice(if key.is_empty() {
                format!("Tab \"{name}\" {what}.")
            } else {
                format!("Tab \"{name}\" {what}: {key} goes to it.")
            });
        }
    }

    /// How every open tab stands, for the title, notifications and
    /// program status (what ended or started since is taken).
    pub(super) fn tab_views(&mut self) -> Vec<super::outside::TabView> {
        let view = |key: u64, app: &mut App, session: &str, stopping: bool| {
            let ask = match attention(app) {
                Some(Attention::Permission) => Some((super::term::Kind::Permission, app.asks)),
                Some(Attention::Question) => Some((super::term::Kind::Question, app.asks)),
                None => None,
            };
            super::outside::TabView {
                key,
                session: session.to_string(),
                title: app.header.title.clone(),
                busy: app.busy || stopping,
                ask,
                ended: app.ended.take(),
                children: std::mem::take(&mut app.child_log),
                running_child: app.running_child(),
            }
        };
        let mut out = Vec::new();
        for (i, e) in self.tabs.entries.iter_mut().enumerate() {
            match &mut e.state {
                State::Shown => {}
                State::Live(s) => out.push(view(e.id, &mut s.app, &s.session_id, s.stopping)),
                State::Dormant => continue,
            }
            if i == self.tabs.shown {
                out.push(view(e.id, &mut self.app, &self.session_id, self.stopping));
            }
        }
        out
    }

    /// Before each frame: every open session watches its background
    /// commands (and resumes when one finished), shown or not.
    pub(super) fn watch_all(&mut self) {
        self.watch_background();
        for i in 0..self.tabs.entries.len() {
            if i == self.tabs.shown {
                continue;
            }
            let State::Live(mut slot) =
                std::mem::replace(&mut self.tabs.entries[i].state, State::Dormant)
            else {
                continue;
            };
            self.exchange(&mut slot);
            self.watch_background();
            self.exchange(&mut slot);
            self.tabs.entries[i].state = State::Live(slot);
        }
    }

    /// Bring the shown tab's entry up to date, keep the tab list, and set
    /// the strip for the next frame.
    pub(super) fn sync_tabs(&mut self) {
        let Some(e) = self.tabs.entries.get_mut(self.tabs.shown) else {
            return;
        };
        if e.tab.session != self.session_id {
            e.tab.session.clone_from(&self.session_id);
        }
        if self.app.header.title.is_some() && e.tab.title != self.app.header.title {
            e.tab.title.clone_from(&self.app.header.title);
        }
        if !e.saved
            && self
                .session
                .as_ref()
                .is_some_and(|s| !s.messages.is_empty())
        {
            e.saved = true;
        }
        e.unread = None;
        if !self.tabs.on() || self.app.mini.is_some() {
            self.app.tabs = None;
            return;
        }
        self.tabs.persist();
        let fresh = self.fresh();
        let items = self
            .tabs
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let (app, stopping, new) = match &e.state {
                    State::Shown => (Some(&self.app), self.stopping, fresh),
                    State::Live(s) => (Some(&s.app), s.stopping, !e.saved),
                    State::Dormant => (None, false, false),
                };
                let title = app
                    .and_then(|a| a.header.title.as_deref())
                    .or(e.tab.title.as_deref())
                    .unwrap_or(tabs::NEW_SESSION_TITLE);
                Item {
                    title: super::display_safe(title),
                    status: Status {
                        busy: app.is_some_and(|a| a.busy) || stopping,
                        attention: app.and_then(attention),
                        unread: if i == self.tabs.shown { None } else { e.unread },
                    },
                    new,
                }
            })
            .collect();
        let start = self.app.tabs.as_ref().map_or(0, |s| s.start.get());
        self.app.tabs = Some(Strip {
            items,
            active: Some(self.tabs.shown),
            numbers: self.tabs.settings.indicators == tabs::Indicators::Numbers,
            vertical: self.tabs.settings.layout == tabs::Orientation::Vertical,
            detail: self.tabs.detail.clone(),
            start: start.into(),
        });
    }

    /// The shown tab is a new session nothing was done in yet.
    fn fresh(&self) -> bool {
        let e = &self.tabs.entries[self.tabs.shown];
        !e.saved
            && !self.app.busy
            && !self.stopping
            && self.app.background == 0
            && self.session.as_ref().is_some_and(|s| s.messages.is_empty())
    }

    /// Show tab `to`, loading its session if it has not been shown since
    /// it was restored. `prune` drops the tab left when it is a new
    /// session with nothing done or typed in it, as leaving OpenCode's new
    /// session page does.
    fn select(&mut self, to: usize, prune: bool) {
        let from = self.tabs.shown;
        if to == from || to >= self.tabs.entries.len() {
            return;
        }
        let drop_from = prune && self.fresh() && self.app.input.is_empty();
        let now = store::now();
        self.tabs.entries[from].viewed = Some(now);
        let dormant = matches!(self.tabs.entries[to].state, State::Dormant);
        let mut slot = match std::mem::replace(&mut self.tabs.entries[to].state, State::Shown) {
            State::Live(s) => s,
            _ => {
                let (id, s) = self.tabs.slot(None, None);
                self.tabs.entries[to].id = id;
                s
            }
        };
        self.exchange(&mut slot);
        self.tabs.entries[from].state = State::Live(slot);
        self.tabs.shown = to;
        let e = &mut self.tabs.entries[to];
        e.unread = None;
        e.viewed = Some(now);
        let session = e.tab.session.clone();
        self.tabs.history.record(&session);
        if drop_from {
            self.drop_tab(from);
        }
        if dormant {
            match self.make(Some(session.clone())) {
                Ok(s) => self.install(s, true),
                Err(err) => {
                    // Gone from the store: close its tab, as OpenCode does.
                    let i = self.tabs.shown;
                    self.close_at(i, false);
                    self.app
                        .error(format!("could not open session {session}: {err:#}"));
                }
            }
        }
    }

    /// Remove tab `i` (not the shown one), stopping what it runs.
    fn drop_tab(&mut self, i: usize) {
        if i == self.tabs.shown {
            return;
        }
        let e = self.tabs.entries.remove(i);
        if i < self.tabs.shown {
            self.tabs.shown -= 1;
        }
        if let State::Live(s) = e.state {
            teardown(s);
        }
    }

    /// A tab command (`session.tab.*`).
    pub(super) fn tab_command(&mut self, id: &str) {
        if !self.tabs.on() || self.app.mini.is_some() {
            return;
        }
        let current = self.tabs.entries[self.tabs.shown].tab.session.clone();
        let list = self.tabs.list();
        let to = |r: &Runner, t: Option<&Tab>| t.and_then(|t| r.tabs.index_of(&t.session));
        match id {
            "session.tab.next" | "session.tab.previous" => {
                let dir = if id.ends_with("next") { 1 } else { -1 };
                if let Some(i) = to(self, tabs::cycle(&list, Some(&current), dir, |_| true)) {
                    self.select(i, true);
                }
            }
            "session.tab.next_unread" | "session.tab.previous_unread" => {
                let dir = if id.ends_with("next_unread") { 1 } else { -1 };
                let marked: Vec<bool> = (0..list.len()).map(|i| self.tabs.marked(i)).collect();
                let hit = tabs::cycle(&list, Some(&current), dir, |t| {
                    list.iter()
                        .position(|x| x.session == t.session)
                        .is_some_and(|i| marked[i])
                });
                match to(self, hit) {
                    Some(i) if i != self.tabs.shown => self.select(i, true),
                    _ => self
                        .app
                        .notice("No other tab is unread or waiting for you."),
                }
            }
            "session.tab.close" => self.close_shown(),
            "session.tab.reopen" => self.reopen_tab(),
            "session.tab.move.left" | "session.tab.move.right" => {
                let dir = if id.ends_with("left") { -1 } else { 1 };
                let mut l = list;
                if tabs::move_tab(&mut l, &current, self.tabs.shown as isize + dir) {
                    let from = self.tabs.shown;
                    let to = l.iter().position(|t| t.session == current).unwrap_or(from);
                    let e = self.tabs.entries.remove(from);
                    self.tabs.entries.insert(to, e);
                    self.tabs.shown = to;
                }
            }
            _ => {
                if let Some(n) = id
                    .strip_prefix("session.tab.select.")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    if n >= 1 && n <= self.tabs.entries.len() {
                        self.select(n - 1, true);
                    }
                }
            }
        }
    }

    /// Close the shown tab (`session.tab.close`): its turn and background
    /// commands stop, and the tab selected before it (else its right, else
    /// its left neighbour) is shown. The last tab gives way to a new
    /// session; a new session with nothing in it is the last thing open.
    fn close_shown(&mut self) {
        let i = self.tabs.shown;
        let fresh = self.fresh();
        if fresh && self.tabs.entries.len() == 1 {
            return;
        }
        if !fresh {
            let tab = self.tabs.entries[i].tab.clone();
            tabs::record_closed(&mut self.tabs.closed, tab, i);
        }
        self.close_at(i, true);
    }

    /// Close tab `i`, the shown one; `history` picks the next tab by the
    /// selection history first.
    fn close_at(&mut self, i: usize, history: bool) {
        let target = self.tabs.entries[i].tab.session.clone();
        let mut rest = self.tabs.list();
        let (_, neighbour) = tabs::close(&mut rest, &target, Some(i));
        let back = history
            .then(|| {
                self.tabs.history.record(&target);
                self.tabs.history.step(&rest, Some(&target), -1)
            })
            .flatten();
        let next = back.or(neighbour).and_then(|s| self.tabs.index_of(&s));
        match next {
            Some(j) => self.select(j, false),
            None => {
                // The last tab: a new session takes its place.
                let (id, slot) = self.tabs.slot(self.agent.clone(), self.model.clone());
                self.tabs.entries.push(Entry {
                    id,
                    tab: Tab::new(""),
                    unread: None,
                    viewed: None,
                    saved: false,
                    state: State::Live(slot),
                });
                let j = self.tabs.entries.len() - 1;
                self.select(j, false);
                self.new_session_here();
            }
        }
        self.drop_tab(i);
    }

    /// Start a new session in the shown (just made) tab.
    fn new_session_here(&mut self) {
        match self.make(None) {
            Ok(s) => {
                self.install(s, false);
                self.app.notice("new session");
            }
            Err(e) => self.app.error(format!("{e:#}")),
        }
        self.tabs.entries[self.tabs.shown]
            .tab
            .session
            .clone_from(&self.session_id);
    }

    /// `/new` with tabs on: a new session in a tab of its own, the current
    /// one going on in the background.
    pub(super) fn new_tab(&mut self) {
        if self.fresh() {
            self.app.notice("new session");
            return;
        }
        let agent = Some(self.current_agent()).filter(|a| !a.is_empty());
        let model = self
            .session
            .as_ref()
            .map(|s| s.model_spec.clone())
            .or_else(|| self.model.clone());
        let (id, slot) = self.tabs.slot(agent, model);
        self.tabs.entries.push(Entry {
            id,
            tab: Tab::new(""),
            unread: None,
            viewed: None,
            saved: false,
            state: State::Live(slot),
        });
        let j = self.tabs.entries.len() - 1;
        self.select(j, true);
        self.new_session_here();
    }

    /// A session picked in `/sessions` with tabs on: its tab is shown,
    /// opened if need be. It takes the place of a new session nothing was
    /// done in.
    pub(super) fn open_session_tab(&mut self, id: String) -> anyhow::Result<()> {
        let mut list = self.tabs.list();
        if !tabs::open(&mut list, Tab::new(&id)) {
            if let Some(j) = self.tabs.index_of(&id) {
                self.select(j, true);
            }
            return Ok(());
        }
        if self.fresh() {
            self.forget_approvals();
            self.agent = None;
            self.model = None;
            let s = self.make(Some(id.clone()))?;
            self.install(s, true);
            let e = &mut self.tabs.entries[self.tabs.shown];
            e.tab = Tab::new(&id);
            e.saved = true;
            self.tabs.history.record(&id);
            return Ok(());
        }
        let n = self.tabs.next_id;
        self.tabs.next_id += 1;
        self.tabs.entries.push(Entry {
            id: n,
            tab: Tab::new(&id),
            unread: None,
            viewed: None,
            saved: true,
            state: State::Dormant,
        });
        let j = self.tabs.entries.len() - 1;
        self.select(j, true);
        Ok(())
    }

    /// Reopen the last closed tab at its place (`session.tab.reopen`).
    fn reopen_tab(&mut self) {
        let list = self.tabs.list();
        let Some((next, id)) = tabs::reopen(&mut self.tabs.closed, &list, None) else {
            self.app.notice("No closed tab to reopen.");
            return;
        };
        let at = next
            .iter()
            .position(|t| t.session == id)
            .unwrap_or(list.len());
        let n = self.tabs.next_id;
        self.tabs.next_id += 1;
        self.tabs.entries.insert(
            at,
            Entry {
                id: n,
                tab: next[at].clone(),
                unread: None,
                viewed: None,
                saved: true,
                state: State::Dormant,
            },
        );
        if at <= self.tabs.shown {
            self.tabs.shown += 1;
        }
        self.select(at, true);
    }

    /// A note for the session picker: the session is open in a tab.
    pub(super) fn tab_note(&self, session: &str) -> &'static str {
        if self.tabs.on() && self.app.mini.is_none() && self.tabs.index_of(session).is_some() {
            " · open in a tab"
        } else {
            ""
        }
    }

    /// The UI is closing: remember the open tabs and stop the turns
    /// running in the background ones (the shown one's is the caller's).
    pub(super) async fn close_tabs(&mut self) {
        if let Some(e) = self.tabs.entries.get_mut(self.tabs.shown) {
            e.viewed = Some(store::now());
        }
        self.sync_tabs();
        for e in &mut self.tabs.entries {
            let State::Live(s) = std::mem::replace(&mut e.state, State::Dormant) else {
                continue;
            };
            let mut s = s;
            if let Some(p) = &s.prompter {
                p.set_stopping(true);
            }
            s.app.cancel_modal();
            if let Some(w) = s.waiter.take() {
                w.abort();
            }
            if let Some(mut t) = s.task.take() {
                t.abort();
                let _ = tokio::time::timeout(CLOSE_WAIT, &mut t).await;
            }
        }
    }
}
