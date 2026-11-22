//! The interactive terminal UI (`codesmith` with no subcommand).
//!
//! The session runs each turn on its own task and talks to the UI through
//! `bridge::Ev` messages; the UI owns the session between turns. Every
//! message is already in the store, so an interrupted turn is recovered by
//! reloading the session.
//!
//! A turn ends when the model answers, even while background commands it
//! started still run. While the UI is idle it waits for one to finish and
//! then resumes the session as a new turn (OpenCode wakes an idle session
//! the same way), so the model acts on the result without a prompt. A
//! prompt sent first reports the finished command itself.

mod app;
pub(crate) mod attach;
mod attention;
#[cfg(test)]
mod bench;
mod bridge;
mod browse_host;
mod browser;
mod complete;
mod diff;
mod diff_draw;
mod diff_source;
mod diff_tree;
mod diff_view;
mod dirs;
mod editor;
mod export_view;
mod external;
mod fit;
mod form;
mod group;
mod history;
pub(crate) mod keymap;
mod login_view;
mod markdown;
mod mcp_view;
mod mention;
mod models;
mod navigation;
mod outside;
mod panel;
#[cfg(test)]
mod parity;
#[cfg(test)]
mod parity_browser;
#[cfg(test)]
mod parity_diff;
#[cfg(test)]
mod parity_form;
#[cfg(test)]
mod parity_groups;
#[cfg(test)]
mod parity_keys;
#[cfg(test)]
mod parity_panel;
#[cfg(test)]
mod parity_prompt;
#[cfg(test)]
mod parity_tabs;
#[cfg(test)]
mod parity_ui;
pub(crate) mod settings;
mod stash;
mod swap;
mod tabs;
mod tabs_host;
mod term;
pub(crate) mod theme;
mod toast;
mod turn;
mod usage;
mod view;

pub(crate) use view::display_safe;

use anyhow::{anyhow, Result};
use app::{Action, App, Entry, Modal, Pick, PickItem, Picker};
use bridge::{ChanPrompter, ChanSink, Ev};
use codesmith_core::config::{self, Loaded};
use codesmith_core::permission::{Approvals, Asker};
use codesmith_core::question::Questioner;
use codesmith_core::session::{self, Options, Session};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::{self, Store};
use codesmith_core::tool::shell::Jobs;
use codesmith_core::{agent, command};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// How often an open output viewer looks at its command's output.
const SHELL_POLL: Duration = Duration::from_millis(250);

pub struct Opts {
    pub resume: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub auto: bool,
}

struct Runner {
    app: App,
    loaded: Loaded,
    cwd: PathBuf,
    tx: UnboundedSender<Ev>,
    asker: Arc<dyn Asker>,
    questioner: Option<Arc<dyn Questioner>>,
    mcp: Arc<codesmith_core::mcp::Mcp>,
    agent: Option<String>,
    model: Option<String>,
    session: Option<Session>,
    session_id: String,
    task: Option<tokio::task::JoinHandle<()>>,
    /// The UI's prompter (none with `--auto`), told when a turn is stopping.
    prompter: Option<Arc<ChanPrompter>>,
    /// The current session's "always" approvals, kept across reloads.
    approvals: Approvals,
    /// An interrupted turn is still finishing a blocking call.
    stopping: bool,
    /// `Done` messages of interrupted turns that finished anyway, to ignore.
    skip_done: usize,
    /// Waits, while the UI is idle, for one of the session's background
    /// commands to finish, then sends `Ev::Background`.
    waiter: Option<tokio::task::JoinHandle<()>>,
    /// After an interrupt, how many finished background commands are not
    /// resumed for (see [`Session::idle_wake`]).
    resume_held: Option<usize>,
    /// The current session's background commands, given to every rebuild
    /// of it (interrupt, undo, an agent or model switch), so they outlive
    /// the session object a turn's task dropped. Stopped when its tab is
    /// closed, when the TUI exits, or, with tabs off, when it moves to
    /// another session (`/new`, `/sessions`).
    jobs: Jobs,
    /// The theme selection.
    themes: theme::Themes,
    /// The theme `tui.theme.name` configures (the theme chosen in the UI is
    /// remembered against it).
    configured_theme: Option<String>,
    /// The terminal's mode, when it reported its background.
    detected: Option<theme::doc::Mode>,
    /// The theme to go back to when the theme picker is closed.
    theme_before: Option<String>,
    /// `ctrl+z` was pressed: suspend before the next frame.
    suspend: bool,
    /// `prompt.editor`: edit the prompt in `$EDITOR` before the next frame.
    edit: bool,
    /// `tui.diffs`, which the diff viewer's remembered layout is kept against.
    diffs: settings::Diffs,
    /// The open session tabs; the fields above from `app` to `jobs` are
    /// the shown tab's (see `tabs_host`).
    tabs: tabs_host::Host,
    /// The window title, notifications and program status.
    outside: outside::Outside,
    /// The prompt history file.
    history: history::Store,
    /// Model favourites, recents and variants.
    models: models::Repository,
    /// The model each agent had in each session.
    selections: models::Selections,
    /// The `!command` running, until its turn is done.
    shell_command: Option<String>,
    /// Goals written (`/goal`), for the status line.
    goals: tokio::sync::broadcast::Receiver<codesmith_core::plugin::goal::Change>,
    /// The open menu chose a session or folder elsewhere: the UI closes
    /// and starts again there.
    relaunch: Option<browse_host::Relaunch>,
    /// The folder the process was launched in, where `/new` goes back to
    /// (unless `tui.session.new_location` is `inherit`).
    launch: PathBuf,
    new_inherit: bool,
    /// The next `/new` starts here (the open menu chose this folder).
    new_here: bool,
    /// The sign-in picker's running login: its number, its task, and
    /// where redirects pasted into the picker go.
    login: Option<(
        u64,
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::Sender<String>,
    )>,
    /// The number the next login gets.
    login_seq: u64,
    /// The provider the running login signs in to.
    login_provider: Option<String>,
}

/// How long an interrupt waits for the turn's task to stop. A tool parked
/// in a blocking call only notices the abort when that call returns.
const STOP_WAIT: Duration = Duration::from_secs(10);

fn open_store() -> Option<Store> {
    Store::open(&store::default_path()).ok()
}

impl Runner {
    fn make(&self, resume: Option<String>) -> Result<Session> {
        let mut s = Session::new(
            &self.loaded,
            self.cwd.clone(),
            Options {
                agent: self.agent.clone(),
                model: self.model.clone(),
                resume,
                title: None,
                parent: None,
                id: None,
                questioner: self.questioner.clone(),
                mcp: Some(self.mcp.clone()),
            },
            self.asker.clone(),
            open_store(),
        )?;
        s.share_approvals(self.approvals.clone());
        s.adopt_jobs(self.jobs.clone());
        Ok(s)
    }

    /// Start over with no approvals, for a different session.
    fn forget_approvals(&mut self) {
        self.approvals = Approvals::default();
    }

    /// With tabs off, leaving the current session for another: stop its
    /// background commands (the TUI then shows and wakes one session only,
    /// so nothing would report them) and start the next one with none.
    fn end_jobs(&mut self) {
        self.stop_waiting();
        self.resume_held = None;
        let old = std::mem::take(&mut self.jobs);
        tokio::spawn(async move { old.stop_all().await });
    }

    /// Install a session and refresh the header (and transcript when continuing).
    fn install(&mut self, s: Session, replay: bool) {
        self.selections.record(&s.id, &s.agent.id, &s.model_spec);
        self.app.header.agent = s.agent.name.clone();
        self.app.header.model = format!("{}/{}", s.model.provider_id, s.model.model_id);
        self.app.header.session = s.id.clone();
        self.app.parent = s.parent().map(str::to_string);
        self.app.goal = codesmith_core::plugin::goal::shared()
            .get(&s.id)
            .map(|g| g.status);
        self.app.header.title = s.title.clone();
        self.app.header.context_limit = s.model.context;
        self.app.header.cost = self.loaded.config.cost(&self.app.header.model);
        if s.id != self.session_id {
            self.app.context = 0;
            self.app.usage_log = Default::default();
        }
        self.app.usage = s.usage;
        self.app.spend = s.spend();
        if replay {
            self.app.set_entries(app::replay(&s.messages));
        }
        self.learn_mentions();
        self.session_id = s.id.clone();
        self.session = Some(s);
    }

    /// Look up the titles of the sessions the transcript mentions that
    /// are not known yet.
    fn learn_mentions(&mut self) {
        let ids = self.app.unknown_mentions();
        if ids.is_empty() {
            return;
        }
        let db = open_store();
        for id in ids {
            let title = db.as_ref().and_then(|db| {
                let info = db.get(&id).ok().flatten()?;
                if db.archived(&id).ok().flatten().is_some() {
                    return None;
                }
                Some(
                    info.title
                        .unwrap_or_else(|| tabs::NEW_SESSION_TITLE.to_string()),
                )
            });
            self.app.session_labels.insert(id, title);
        }
    }

    /// Reload the current session from the store (or start fresh if it was
    /// never saved), with the selected agent and model, which a stored
    /// session keeps for `-s` and automations.
    fn reload(&mut self) -> Result<()> {
        let db = open_store();
        let stored = db
            .as_ref()
            .and_then(|db| db.get(&self.session_id).ok().flatten())
            .is_some();
        let s = self.make(stored.then(|| self.session_id.clone()))?;
        if let Some(db) = db.filter(|_| stored) {
            let _ = db.set_selection(&s.id, &s.agent.id, &s.model_spec);
        }
        self.install(s, stored);
        Ok(())
    }

    /// Run a custom or MCP command: its prompt (an MCP prompt fetched
    /// first, `` !`cmd` `` output filled in) sent, or run in a child session.
    fn submit_command(&mut self, shown: String, c: command::Command, args: String) {
        if self.app.busy {
            self.app
                .notice("A turn is still running. Press Esc to interrupt it first.");
            return;
        }
        let Some(mut s) = self.session.take() else {
            self.app.error("no session");
            return;
        };
        self.app.entries.push(Entry::User(shown.clone()));
        self.app.start_turn();
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let result = s
                .run_command(&shown, &c, &args, vec![], &mut sink)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    fn submit(&mut self, shown: String, text: String, media: Vec<codesmith_ai::Media>) {
        if self.app.busy {
            self.app
                .notice("A turn is still running. Press Esc to interrupt it first.");
            return;
        }
        let Some(mut s) = self.session.take() else {
            self.app.error("no session");
            return;
        };
        self.app.entries.push(Entry::User(shown));
        self.app.start_turn();
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let result = s
                .prompt_with(&text, media, &mut sink)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    /// Run `!command` as the user (OpenCode's shell mode): shown as a shell
    /// row, recorded for the model's next step, no model call.
    fn user_shell(&mut self, command: String) {
        if self.app.busy {
            self.app
                .notice("A turn is still running. Press Esc to interrupt it first.");
            return;
        }
        let Some(mut s) = self.session.take() else {
            self.app.error("no session");
            return;
        };
        self.app.busy = true;
        self.shell_command = Some(command.clone());
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            use codesmith_core::session::Sink;
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let id = format!("user-shell-{}", session::new_id());
            sink.tool_start(&id, "shell", &serde_json::json!({"command": command}));
            let result = match s.shell(&command).await {
                Ok(ran) => {
                    let shown = codesmith_core::user_shell::parse(&ran.text)
                        .map_or(ran.output.clone(), |v| v.text);
                    sink.tool_end(&id, "shell", &shown, ran.failed());
                    Ok(())
                }
                // It could not run: the prompt comes back (see `shell_failed`).
                Err(e) => {
                    sink.tool_end(&id, "shell", &format!("{e:#}"), true);
                    Err(format!("{e:#}"))
                }
            };
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    /// Run a native command (`/review`, `/calibrate`) on a task; its report
    /// joins the session as an exchange.
    fn native(&mut self, name: &str, args: &str) {
        if self.app.busy {
            self.app
                .notice("A turn is still running. Press Esc to interrupt it first.");
            return;
        }
        let Some(mut s) = self.session.take() else {
            self.app.error("no session");
            return;
        };
        let shown = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        self.app.entries.push(Entry::User(shown.clone()));
        self.app.start_turn();
        let env = codesmith_core::review::Env::new(
            &self.loaded,
            s.cwd.clone(),
            Some(s.model_spec.clone()),
            Some(store::default_path()),
            Some(s.id.clone()),
            None,
        );
        let (tx, name, args) = (self.tx.clone(), name.to_string(), args.to_string());
        self.task = Some(tokio::spawn(async move {
            let notes = tx.clone();
            let progress = move |m: String| {
                let _ = notes.send(Ev::Notice(m));
            };
            let result =
                match codesmith_core::review::run_command(&name, &args, &env, &progress).await {
                    Ok((report, usage)) => {
                        let _ = tx.send(Ev::Text(report.clone()));
                        s.append_exchange(&shown, &report, &usage)
                            .map_err(|e| format!("{e:#}"))
                    }
                    Err(e) => Err(format!("{e:#}")),
                };
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    /// Stop the running turn and reload the session from the store. The
    /// task is awaited first: until it stops it can still record a result
    /// (a declined permission), and a session reloaded before that would
    /// see the call unanswered and answer it a second time.
    async fn interrupt(&mut self) {
        if self.stopping {
            self.app
                .notice("still stopping the interrupted turn; it reloads when it ends");
            return;
        }
        if let Some(p) = &self.prompter {
            p.set_stopping(true);
        }
        self.app.cancel_modal();
        if let Some(mut t) = self.task.take() {
            t.abort();
            match tokio::time::timeout(STOP_WAIT, &mut t).await {
                // It ended before the abort landed; its `Done` is queued.
                Ok(Ok(())) => self.skip_done += 1,
                Ok(Err(_)) => {}
                Err(_) => {
                    self.stopping = true;
                    let tx = self.tx.clone();
                    self.task = Some(tokio::spawn(async move {
                        let _ = t.await;
                        let _ = tx.send(Ev::Stopped);
                    }));
                    self.app.notice(
                        "stopping: a tool is still finishing; the session reloads when it ends",
                    );
                    return;
                }
            }
        }
        self.stopped();
    }

    /// The interrupted turn has stopped: reload its session.
    fn stopped(&mut self) {
        if let Some(p) = &self.prompter {
            p.set_stopping(false);
        }
        self.stopping = false;
        self.task = None;
        self.app.busy = false;
        self.app.ended = Some(app::TurnEnd::Interrupted);
        self.app.end_turn(true);
        self.app.drop_forming();
        self.app.notice("interrupted");
        if let Err(e) = self.reload() {
            self.app.error(format!("{e:#}"));
        }
        // The user stopped the turn: commands that already finished wait
        // for the next prompt (or another to finish) instead of resuming
        // the session at once, which after an interrupted resume would
        // restart it.
        self.resume_held = self.session.as_ref().map(|s| s.background_finished());
    }

    /// Keep the status line's background count current and, while idle,
    /// resume the session when one of its background commands has
    /// finished, or wait for one that still runs. Called before every
    /// redraw; a turn in progress reports finished commands itself.
    fn watch_background(&mut self) {
        let Some(s) = &self.session else {
            // The session is on a turn's task: keep the last count.
            self.stop_waiting();
            return;
        };
        self.app.background = s.background_running();
        if self.app.busy || self.stopping {
            self.stop_waiting();
            return;
        }
        let wake = s.idle_wake(&mut self.resume_held);
        if wake == session::IdleWake::Resume {
            self.stop_waiting();
            self.resume_background();
            return;
        }
        if self.waiter.as_ref().is_some_and(|w| !w.is_finished()) {
            return;
        }
        let (reaper, tx) = (s.reaper(), self.tx.clone());
        self.waiter = match wake {
            // A resume failed: try again once it has backed off.
            session::IdleWake::At(due) => Some(tokio::spawn(async move {
                tokio::time::sleep_until(due.into()).await;
                let _ = tx.send(Ev::Background);
            })),
            session::IdleWake::Finish(seen) => Some(tokio::spawn(async move {
                reaper.wait_more(seen).await;
                let _ = tx.send(Ev::Background);
            })),
            session::IdleWake::Resume | session::IdleWake::Idle => None,
        };
    }

    /// Start the loads the diff viewer asked for, and remember its layout
    /// when it changed.
    fn pump_diff(&mut self) {
        for req in self.app.take_diff_requests() {
            let tx = self.tx.clone();
            let root = config::project_root(&self.cwd);
            let session = self.session_id.clone();
            tokio::task::spawn_blocking(move || {
                let turn = || {
                    let db = open_store().ok_or("the session store is unavailable")?;
                    let snaps = Snapshots::for_project(&root)
                        .ok_or_else(|| format!("snapshots are not kept for {}", root.display()))?;
                    diff_source::turn_diff(&db, &session, &snaps)
                };
                let src = diff_view::Git(turn);
                let _ = tx.send(Ev::Diff(Box::new(diff_view::run(req.job, req.seq, &src))));
            });
        }
        if std::mem::take(&mut self.app.diff_prefs_changed) {
            let text = diff_view::remember_text(&self.diffs, self.app.diff_prefs);
            let _ = codesmith_core::paths::write_private(&diff_view::state_file(), text.as_bytes());
        }
    }

    fn stop_waiting(&mut self) {
        if let Some(w) = self.waiter.take() {
            w.abort();
        }
    }

    /// Resume the idle session for its finished background commands, as a
    /// turn of its own (Esc interrupts it like any other).
    fn resume_background(&mut self) {
        let Some(mut s) = self.session.take() else {
            return;
        };
        self.app.start_turn();
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let result = s
                .resume_background(&mut sink)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Ev::Done {
                result,
                session: Box::new(s),
            });
        }));
    }

    fn idle_or_say(&mut self) -> bool {
        if self.app.busy {
            self.app
                .notice("Wait for the current turn to finish, or press Esc to interrupt it.");
            return false;
        }
        true
    }

    fn slash(&mut self, name: &str, args: &str) -> Result<()> {
        let Some(name) = builtin(name, args) else {
            return self.command(name, args);
        };
        match name {
            "exit" | "quit" | "q" => self.app.quit = true,
            "help" => self.app.modal = Some(Modal::Help(0)),
            "export" => {
                self.app.modal = Some(Modal::Export(Box::new(export_view::ExportView::new(
                    &self.session_id,
                    &self.cwd,
                    self.app.show_reasoning,
                ))))
            }
            "themes" | "theme" => self.theme_command(args),
            "login" => self.open_login(),
            "clear" => self.app.set_entries(Vec::new()),
            "editor" => self.edit = true,
            "diff" => {
                let dir = self
                    .session
                    .as_ref()
                    .map(|s| s.cwd.clone())
                    .unwrap_or_else(|| self.cwd.clone());
                self.app.toggle_diff(dir);
            }
            "thinking" => {
                self.app.show_reasoning = !self.app.show_reasoning;
                self.app.notice(format!(
                    "reasoning {}",
                    if self.app.show_reasoning {
                        "shown"
                    } else {
                        "hidden"
                    }
                ));
            }
            "verbosity" => {
                let arg = args.trim();
                if arg.is_empty() {
                    self.app.verbosity = self.app.verbosity.next();
                } else {
                    match group::Verbosity::parse(&arg.to_ascii_lowercase()) {
                        Some(v) => self.app.verbosity = v,
                        None => anyhow::bail!("verbosity is low, medium or high, not {arg}"),
                    }
                }
                self.app
                    .notice(format!("verbosity: {}", self.app.verbosity.key()));
            }
            // As OpenCode's `session.new_location: "launch"`: a new session
            // starts where the UI was launched.
            "new"
                if !self.new_inherit
                    && !std::mem::take(&mut self.new_here)
                    && !browse_host::same_dir(&self.launch, &self.cwd) =>
            {
                self.relaunch(self.launch.clone(), None)
            }
            "new" if self.tabs.on() => self.new_tab(),
            "new" if self.idle_or_say() => {
                self.forget_approvals();
                self.end_jobs();
                let s = self.make(None)?;
                self.app.set_entries(Vec::new());
                self.install(s, false);
                self.app.notice("new session");
            }
            "sessions" if self.tabs.on() || self.idle_or_say() => {
                let list = open_store()
                    .map(|db| folder_sessions(&db, &self.cwd))
                    .transpose()?
                    .unwrap_or_default();
                let items = list
                    .into_iter()
                    .map(|i| PickItem {
                        current: i.id == self.session_id,
                        label: i.title.clone().unwrap_or_else(|| "(untitled)".into()),
                        detail: format!("{} · {}{}", ago(i.updated), i.id, self.tab_note(&i.id)),
                        id: i.id,
                    })
                    .collect();
                self.app.modal = Some(Modal::Picker(Picker::new(Pick::Session, "Sessions", items)));
            }
            "open" | "projects" | "project" => self.open_browser(),
            "worktrees" => self.open_worktrees(),
            "fork" if self.idle_or_say() => {
                let Some(s) = &self.session else {
                    return Ok(());
                };
                if !s.seqs().iter().any(Option::is_some) {
                    self.app.notice("nothing to fork yet");
                    return Ok(());
                }
                let mut items = vec![PickItem {
                    current: false,
                    label: "the whole session".into(),
                    detail: "through the latest message".into(),
                    id: String::new(),
                }];
                // Your prompts, newest first: the fork stops before the one picked.
                let prompts = codesmith_core::fork::user_prompts(&s.messages, s.seqs())
                    .into_iter()
                    .filter_map(|(seq, m)| Some((prompt_text(m)?, seq)));
                let mut prompts: Vec<_> = prompts.collect();
                prompts.reverse();
                items.extend(prompts.into_iter().map(|(text, seq)| PickItem {
                    current: false,
                    label: text.lines().next().unwrap_or("").chars().take(80).collect(),
                    detail: format!("before message {seq}"),
                    id: seq.to_string(),
                }));
                self.app.modal = Some(Modal::Picker(Picker::new(Pick::Fork, "Fork", items)));
            }
            "undo" | "redo" if self.idle_or_say() => {
                let db = open_store().ok_or_else(|| anyhow!("the session store is unavailable"))?;
                let root = config::project_root(&self.cwd);
                let snaps = Snapshots::for_project(&root)
                    .ok_or_else(|| anyhow!("snapshots are not kept for {}", root.display()))?;
                let changes = if name == "undo" {
                    session::undo(&db, &self.session_id, &snaps)?
                } else {
                    session::redo(&db, &self.session_id, &snaps)?
                };
                match changes {
                    Some(ch) => {
                        self.reload()?;
                        let files: Vec<String> = ch.iter().map(|c| c.path.clone()).collect();
                        self.app.notice(format!(
                            "{} the latest turn; {} file(s): {}",
                            if name == "undo" { "undid" } else { "redid" },
                            ch.len(),
                            files.join(", ")
                        ));
                    }
                    None => self.app.notice(format!("nothing to {name}")),
                }
            }
            "agent" if self.idle_or_say() => {
                if args.is_empty() {
                    let current = self.current_agent();
                    let items = agent::list(&self.loaded.config)
                        .into_iter()
                        .filter_map(|id| agent::resolve(&self.loaded.config, &id))
                        .map(|a| PickItem {
                            current: a.id == current,
                            label: a.id.clone(),
                            detail: if a.mode == "subagent" {
                                format!("(subagent) {}", a.description)
                            } else {
                                a.description.clone()
                            },
                            id: a.id,
                        })
                        .collect();
                    self.app.modal = Some(Modal::Picker(Picker::new(Pick::Agent, "Agents", items)));
                } else {
                    self.set_agent(args)?;
                }
            }
            "model" if self.idle_or_say() => {
                if args.is_empty() {
                    // The kept lists show at once; the local servers are
                    // asked again meanwhile.
                    let items = self.model_items();
                    self.app.modal = Some(Modal::Picker(Picker::new(Pick::Model, "Models", items)));
                    refresh_local_models(&self.loaded.config, self.tx.clone());
                    swap::refresh(&self.loaded.config, self.tx.clone());
                } else {
                    self.set_model(args)?;
                }
            }
            "variant" if self.idle_or_say() => self.set_variant(args)?,
            "compact" if self.idle_or_say() => {
                let Some(mut s) = self.session.take() else {
                    return Ok(());
                };
                self.app.busy = true;
                let tx = self.tx.clone();
                let cwd = s.cwd.clone();
                self.task = Some(tokio::spawn(async move {
                    let mut sink = ChanSink::new(tx.clone(), cwd);
                    let result = s.compact(&mut sink).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Ev::Done {
                        result,
                        session: Box::new(s),
                    });
                }));
            }
            _ => {}
        }
        Ok(())
    }

    /// Run a custom, native or MCP command. Their names, unlike the
    /// built-ins', are matched exactly, as in OpenCode.
    fn command(&mut self, other: &str, args: &str) -> Result<()> {
        let all = crate::all_commands_with(&self.cwd, &self.loaded, &self.mcp);
        let c = match all.get(other) {
            Some(c) => c.clone(),
            None if builtin(other, "").is_some() => {
                return Err(anyhow!("/{other} takes no arguments"));
            }
            None => return Err(anyhow!("unknown command /{other}; /help lists commands")),
        };
        if c.native {
            self.native(other, args);
            return Ok(());
        }
        let mut rebuild = false;
        // A subagent command runs in a child session and leaves this one's
        // agent and model alone.
        let child = c.runs_as_subagent(&self.loaded.config);
        if !child && c.agent.is_some() && c.agent != self.agent {
            self.agent = c.agent.clone();
            rebuild = true;
        }
        if !child && c.model.is_some() && c.model != self.model {
            self.model = c.model.clone();
            rebuild = true;
        }
        if rebuild {
            self.reload()?;
        }
        let shown = if args.is_empty() {
            format!("/{other}")
        } else {
            format!("/{other} {args}")
        };
        self.submit_command(shown, c, args.to_string());
        Ok(())
    }

    fn set_agent(&mut self, id: &str) -> Result<()> {
        let prev = self.agent.replace(id.to_string());
        // The agent's model in this session, else its configured one.
        let configured =
            codesmith_core::agent::resolve(&self.loaded.config, id).and_then(|a| a.model);
        let model = self
            .selections
            .for_agent(&self.session_id, id, configured.as_deref());
        let prev_model = match model {
            Some(m) => self.model.replace(m),
            None => self.model.clone(),
        };
        if let Err(e) = self.reload() {
            self.agent = prev;
            self.model = prev_model;
            return Err(e);
        }
        self.app.notice(format!("agent: {id}"));
        Ok(())
    }

    fn set_model(&mut self, spec: &str) -> Result<()> {
        // Refused before anything is sent when it names no model the user
        // can run; spacing, case and partial names are forgiven.
        let resolved = codesmith_core::model_spec::resolve(&self.loaded.config, spec)?;
        let spec = resolved.spec.as_str();
        let prev = self.model.replace(spec.to_string());
        if let Err(e) = self.reload() {
            self.model = prev;
            return Err(e);
        }
        if let Some(note) = &resolved.note {
            self.app.notice(note.clone());
        }
        if let Some(m) = models::Model::parse(spec) {
            self.models.add_recent(&m);
            let (_, variant) = config::split_variant(spec);
            self.models.save_variant(&m, variant.unwrap_or("default"));
        }
        self.app.notice(format!("model: {spec}"));
        Ok(())
    }

    /// Select a model variant (`/variant high`, `/variant default`), or
    /// with no name move to the next one, wrapping back to the default.
    fn set_variant(&mut self, name: &str) -> Result<()> {
        let Some(s) = &self.session else {
            return Ok(());
        };
        let (base, current) = config::split_variant(&s.model_spec);
        let base = base.to_string();
        let variants = s.model.variants.clone();
        if variants.is_empty() {
            self.app.notice(format!("{base} has no variants"));
            return Ok(());
        }
        let next = match name {
            "" => match current.and_then(|c| variants.iter().position(|v| v == c)) {
                None => variants.first().cloned(),
                Some(i) => variants.get(i + 1).cloned(),
            },
            "default" => None,
            n => Some(n.to_string()),
        };
        match next {
            Some(v) => self.set_model(&format!("{base}#{v}")),
            None => self.set_model(&base),
        }
    }

    /// The model picker's items: favourites, then recent models, then the
    /// rest.
    fn model_items(&self) -> Vec<PickItem> {
        let prefs = self.models.read();
        let mut items = model_items(&self.loaded.config, &self.app.header.model);
        // The models the servers with a `swap` setting can load, last.
        let swaps = swap::items(&self.loaded.config, &items);
        items.extend(swaps);
        let rank = |id: &str| {
            let key = id.split('#').next().unwrap_or(id);
            if prefs.favorite.iter().any(|m| m.key() == key) {
                0
            } else if prefs.recent.iter().any(|m| m.key() == key) {
                1
            } else {
                2
            }
        };
        for i in &mut items {
            match rank(&i.id) {
                0 => i.detail = format!("★ favorite · {}", i.detail),
                1 => i.detail = format!("recent · {}", i.detail),
                _ => {}
            }
        }
        // Recents in the order used; the sort is stable for the rest.
        let order = |id: &str| {
            let key = id.split('#').next().unwrap_or(id);
            prefs
                .favorite
                .iter()
                .chain(&prefs.recent)
                .position(|m| m.key() == key)
                .unwrap_or(usize::MAX)
        };
        items.sort_by_key(|i| (rank(&i.id), order(&i.id)));
        items
    }

    /// ctrl+f in the model picker: the model goes on or off the
    /// favourites, and the picker opens again on it.
    fn toggle_favorite(&mut self, id: &str) {
        if id.starts_with(swap::PREFIX) {
            self.app
                .notice("a model the server has yet to load cannot be a favorite");
            return;
        }
        let Some(m) = models::Model::parse(id) else {
            return;
        };
        let on = self.models.toggle_favorite(&m);
        let filter = match &self.app.modal {
            Some(Modal::Picker(p)) => p.filter.clone(),
            _ => String::new(),
        };
        let mut p = Picker::new(Pick::Model, "Models", self.model_items());
        p.set_filter(&filter);
        p.select(id);
        self.app.modal = Some(Modal::Picker(p));
        self.app.notice(format!(
            "{} {}",
            m.key(),
            if on {
                "added to favorites"
            } else {
                "removed from favorites"
            }
        ));
    }

    /// F2 and friends: the next or previous recent (or favourite) model.
    fn cycle_model(&mut self, favorites: bool, dir: i32) -> Result<()> {
        if !self.idle_or_say() {
            return Ok(());
        }
        let prefs = self.models.read();
        let list = if favorites {
            &prefs.favorite
        } else {
            &prefs.recent
        };
        let current = models::Model::parse(&self.app.header.model);
        let Some(next) = models::cycle(list, current.as_ref(), dir) else {
            self.app.notice(if favorites {
                "no favorite models yet: ctrl+f in the model picker adds one"
            } else {
                "no recent models yet"
            });
            return Ok(());
        };
        let key = next.key();
        let spec = match self.models.resolve_variant(&next) {
            Some(v) => format!("{key}#{v}"),
            None => key.clone(),
        };
        if spec.contains('#') && self.set_model(&spec).is_ok() {
            return Ok(());
        }
        self.set_model(&key)
    }

    fn current_agent(&self) -> String {
        self.session
            .as_ref()
            .map(|s| s.agent.id.clone())
            .or_else(|| self.agent.clone())
            .unwrap_or_default()
    }

    /// Switch to the next or previous primary agent (Tab / Shift+Tab).
    fn cycle_agent(&mut self, dir: i32) -> Result<()> {
        if !self.idle_or_say() {
            return Ok(());
        }
        let primary: Vec<String> = agent::list(&self.loaded.config)
            .into_iter()
            .filter(|id| {
                agent::resolve(&self.loaded.config, id).is_some_and(|a| agent::selectable(&a))
            })
            .collect();
        if primary.len() < 2 {
            return Ok(());
        }
        let cur = self.current_agent();
        let i = primary.iter().position(|a| *a == cur).unwrap_or(0) as i32;
        let n = primary.len() as i32;
        let next = primary[((i + dir) % n + n) as usize % primary.len()].clone();
        self.set_agent(&next)
    }

    fn picked(&mut self, kind: Pick, id: String) -> Result<()> {
        match kind {
            // Not while a turn (a resumed one may start while the picker
            // is open) holds the session and its commands.
            Pick::Session if self.tabs.on() => self.open_session_tab(id)?,
            Pick::Session if self.idle_or_say() => {
                if id != self.session_id {
                    self.forget_approvals();
                    self.end_jobs();
                }
                let s = self.make(Some(id))?;
                self.install(s, true);
            }
            Pick::Fork if self.idle_or_say() => {
                let before = id.parse::<i64>().ok();
                let mut db =
                    open_store().ok_or_else(|| anyhow!("the session store is unavailable"))?;
                let to = session::new_id();
                db.fork_at(&self.session_id, &to, before)?;
                // As in OpenCode, the prompt forked before is back in the input to edit.
                let text = before.and_then(|b| {
                    let s = self.session.as_ref()?;
                    let prompts = codesmith_core::fork::user_prompts(&s.messages, s.seqs());
                    let (_, m) = prompts.into_iter().find(|(seq, _)| *seq == b)?;
                    prompt_text(m)
                });
                if self.tabs.on() {
                    // The fork opens in a tab of its own; this one goes on.
                    self.open_session_tab(to)?;
                } else {
                    self.forget_approvals();
                    self.end_jobs();
                    let s = self.make(Some(to))?;
                    self.install(s, true);
                }
                if let Some(t) = text {
                    self.app.restore_prompt(&t);
                }
                self.app.notice("forked the session");
            }
            Pick::Agent if self.idle_or_say() => self.set_agent(&id)?,
            Pick::Theme => {
                self.theme_before = None;
                self.set_theme(&id, true);
            }
            // A model the provider's server can load: only ever started here,
            // by a person choosing it.
            Pick::Model if id.starts_with(swap::PREFIX) => match swap::parse_id(&id) {
                None => {
                    let pid = id.trim_start_matches(swap::PREFIX).trim_end_matches('/');
                    self.app.error(format!(
                        "{pid} could not list the models it can load; see the picker entry"
                    ));
                }
                Some(_) if !self.idle_or_say() => {}
                Some((pid, model)) => {
                    match swap::start(&self.loaded.config, &pid, &model, self.tx.clone()) {
                        Ok(()) => {
                            self.app.modal = Some(Modal::Swap(swap::SwapView::new(&pid, &model)))
                        }
                        Err(e) => self.app.error(e),
                    }
                }
            },
            // A provider without listed models: let the user type the model.
            Pick::Model if id.ends_with('/') => self.app.input.set(format!("/model {id}")),
            Pick::Model if self.idle_or_say() => {
                // A model without a variant gets the one chosen for it last.
                let remembered = (!id.contains('#'))
                    .then(|| models::Model::parse(&id))
                    .flatten()
                    .and_then(|m| self.models.resolve_variant(&m));
                match remembered {
                    Some(v) if self.set_model(&format!("{id}#{v}")).is_ok() => {}
                    _ => self.set_model(&id)?,
                }
            }
            Pick::DiffSource | Pick::DiffBase => self.app.diff_picked(kind, &id),
            // The app runs what the palette chose.
            Pick::Command | Pick::Session | Pick::Fork | Pick::Agent | Pick::Model => {}
        }
        Ok(())
    }

    /// Show the current theme from the next frame.
    fn apply_theme(&mut self) {
        theme::install(self.themes.palette());
        for (name, e) in std::mem::take(&mut self.themes.errors) {
            self.app
                .error(format!("theme {name} did not load, using opencode: {e}"));
        }
    }

    /// Remember the theme and mode chosen in the UI.
    fn remember_theme(&self) {
        let mode = self.themes.locked().then(|| self.themes.mode());
        theme::remember(
            self.configured_theme.as_deref(),
            self.themes.selected(),
            mode,
        );
    }

    fn set_theme(&mut self, name: &str, persist: bool) {
        if !self.themes.set(name) {
            self.app.error(format!("there is no theme {name}"));
            return;
        }
        self.apply_theme();
        if persist {
            self.remember_theme();
            self.app
                .notice(format!("theme: {}", self.themes.selected()));
        }
    }

    /// `/themes`: pick a theme; `/themes <name>` choose one; `/themes
    /// dark`, `light` or `system` set the mode.
    fn theme_command(&mut self, args: &str) {
        let arg = args.trim();
        match arg {
            "" => {
                let current = self.themes.selected().to_string();
                let items = self
                    .themes
                    .names()
                    .into_iter()
                    .map(|n| PickItem {
                        current: n == current,
                        detail: String::new(),
                        label: n.clone(),
                        id: n,
                    })
                    .collect();
                self.theme_before = Some(current);
                self.app.modal = Some(Modal::Picker(Picker::new(Pick::Theme, "Themes", items)));
            }
            "dark" | "light" => self.set_mode(theme::doc::Mode::parse(arg).unwrap()),
            "system" => {
                self.themes.unlock(self.detected);
                self.apply_theme();
                self.remember_theme();
                self.app.notice("theme mode: follows the terminal");
            }
            name => self.set_theme(name, true),
        }
    }

    fn set_mode(&mut self, mode: theme::doc::Mode) {
        if !self.themes.set_mode(mode) {
            self.app.error(format!(
                "theme {} has no {} mode",
                self.themes.selected(),
                mode.key()
            ));
            return;
        }
        self.apply_theme();
        self.remember_theme();
        self.app.notice(format!("theme mode: {}", mode.key()));
    }

    fn try_run(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) {
        if let Err(e) = f(self) {
            self.app.error(format!("{e:#}"));
        }
    }

    /// A command the app hands over: theme mode switches and suspend.
    fn run_command(&mut self, id: &str) {
        match id {
            "theme.switch_mode" => self.set_mode(self.themes.mode().other()),
            "theme.mode.lock" if self.themes.locked() => self.theme_command("system"),
            "theme.mode.lock" => {
                self.themes.lock();
                self.remember_theme();
                self.app
                    .notice(format!("theme mode: {} (pinned)", self.themes.mode().key()));
            }
            "terminal.suspend" => self.suspend = true,
            "prompt.editor" => self.edit = true,
            "model.cycle_recent" => self.try_run(|r| r.cycle_model(false, 1)),
            "model.cycle_recent_reverse" => self.try_run(|r| r.cycle_model(false, -1)),
            "model.cycle_favorite" => self.try_run(|r| r.cycle_model(true, 1)),
            "model.cycle_favorite_reverse" => self.try_run(|r| r.cycle_model(true, -1)),
            "terminal.title.toggle" => {
                let on = self.outside.toggle_title();
                self.app.notice(if on {
                    "terminal title: on"
                } else {
                    "terminal title: off"
                });
            }
            "mcp.list" if self.app.modal.is_none() => {
                let servers = self
                    .mcp
                    .statuses()
                    .iter()
                    .map(|(n, s)| mcp_view::Server::from_core(n, s))
                    .collect();
                self.app.modal = Some(Modal::Mcp(mcp_view::McpView::new(servers)));
            }
            id if id.starts_with("session.tab.") => self.tab_command(id),
            _ => {}
        }
    }

    /// Make the export the dialog asked for: the stored session as Markdown
    /// or JSON (see `codesmith_core::export`), copied through the terminal
    /// or saved to the file the dialog named. A file already there is
    /// replaced only once the dialog confirmed it.
    fn export(&mut self, out: export_view::Outcome) {
        use export_view::Outcome;
        let (options, target) = match out {
            Outcome::Copy(o) => (o, None),
            Outcome::Save {
                options,
                path,
                overwrite,
            } => (options, Some((path, overwrite))),
            _ => return,
        };
        let content = match self.export_text(options) {
            Ok(c) => c,
            Err(e) => {
                self.app.modal = None;
                self.app.error(e);
                return;
            }
        };
        let Some((path, overwrite)) = target else {
            self.app.modal = None;
            self.app.clipboard = Some(content);
            self.app.toast(toast::Toast::new(
                toast::Variant::Success,
                "Copied to clipboard",
            ));
            return;
        };
        let result = codesmith_core::export::save(&path, &content, overwrite);
        let Some(Modal::Export(v)) = self.app.modal.as_mut() else {
            return;
        };
        match result {
            Ok(()) => v.saved(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => v.exists(path),
            Err(e) => v.failed(&path, format!("Could not save: {e}")),
        }
    }

    /// The export's text, credentials scrubbed.
    fn export_text(&self, o: export_view::Options) -> std::result::Result<String, String> {
        use codesmith_core::export;
        let db = open_store().ok_or("the session store is unavailable")?;
        let e = match db.get(&self.session_id) {
            Ok(Some(_)) => db.export(&self.session_id).map_err(|e| format!("{e:#}"))?,
            _ => return Err("Nothing to export yet: the session has no messages".into()),
        };
        let secrets = export::known_secrets(&self.loaded.config);
        Ok(if !o.json {
            export::markdown(
                &e,
                export::Transcript {
                    thinking: o.thinking,
                    tools: o.tools,
                },
                &secrets,
            )
        } else {
            let v = if o.sanitize {
                export::sanitized(&e)
            } else {
                export::raw_json(&e, &secrets)
            };
            serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
        })
    }

    /// Start a remote MCP server's OAuth login on a task (the browser
    /// opens on its authorization page); the dialog hears how it ended.
    fn mcp_sign_in(&mut self, name: String) {
        use codesmith_core::mcp::{oauth, ServerConfig};
        let tx = self.tx.clone();
        let Some(ServerConfig::Remote {
            url,
            oauth: setting,
            ..
        }) = self.loaded.config.mcp.servers.get(&name).cloned()
        else {
            let _ = tx.send(Ev::McpSignedIn {
                server: name,
                result: Err("it is not a remote server".into()),
            });
            return;
        };
        if !oauth::Setting::enabled(setting.as_ref()) {
            let _ = tx.send(Ev::McpSignedIn {
                server: name,
                result: Err("OAuth is turned off for it (\"oauth\": false)".into()),
            });
            return;
        }
        let mcp = self.mcp.clone();
        tokio::spawn(async move {
            let r = oauth::login(
                &reqwest::Client::new(),
                &oauth::store_path(),
                &name,
                &url,
                setting.as_ref(),
                oauth::open_browser,
            )
            .await;
            let mut result = r.map(|_| ()).map_err(|e| format!("{e:#}"));
            // Signed in: connect it now with the new token.
            if result.is_ok() {
                result = match mcp.restart(&name).await {
                    Some(codesmith_core::mcp::Status::Connected { .. }) => Ok(()),
                    Some(codesmith_core::mcp::Status::Failed(e)) => {
                        Err(format!("signed in, but connecting failed: {e}"))
                    }
                    _ => Err("signed in, but it did not connect".into()),
                };
            }
            let _ = tx.send(Ev::McpSignedIn {
                server: name,
                result,
            });
        });
    }

    /// Open the sign-in picker (`/login`).
    fn open_login(&mut self) {
        let (choices, help) = login_view::load(
            &self.loaded.config,
            &codesmith_core::paths::auth_file(),
            &codesmith_core::providers::process_env(),
        );
        self.app.modal = Some(Modal::Login(Box::new(login_view::LoginView::new(
            choices, help,
        ))));
    }

    /// The sign-in picker's states again, after a sign-in or out.
    fn refresh_login(&mut self) {
        if let Some(Modal::Login(v)) = self.app.modal.as_mut() {
            let (choices, help) = login_view::load(
                &self.loaded.config,
                &codesmith_core::paths::auth_file(),
                &codesmith_core::providers::process_env(),
            );
            v.refresh(choices, help);
        }
    }

    /// After signing in to `provider`: when no model was chosen (on the
    /// command line, in this session, by the agent or in codesmith.json),
    /// switch to the provider's newest model and save it as the default.
    /// What changed, for the sign-in picker.
    fn adopt_login(&mut self, provider: &str) -> Option<String> {
        use codesmith_core::model_spec;
        let data = codesmith_core::paths::data_dir();
        let auto = model_spec::auto_model(&data);
        let agent_model =
            codesmith_core::agent::resolve(&self.loaded.config, &self.current_agent())
                .and_then(|a| a.model)
                .is_some();
        if model_chosen(self.model.as_deref(), auto.as_deref(), agent_model) {
            return None;
        }
        let access = model_spec::Access::current(&self.loaded.config);
        let a = model_spec::adopt_login(
            &self.loaded.config,
            provider,
            &access,
            &codesmith_core::paths::config_dir(),
            &data,
        )?;
        self.loaded.config.model = Some(a.spec.clone());
        let saved = match &a.saved {
            Ok(p) => format!("saved as the default in {}", p.display()),
            Err(e) => format!("not saved as the default: {e}"),
        };
        if self.app.busy || self.session.is_none() {
            let line = format!(
                "{} is the default for new sessions ({saved}); /model {} switches now",
                a.spec, a.spec
            );
            self.app.notice(line.clone());
            return Some(line);
        }
        // Named, as a session that ran a turn keeps its own model
        // otherwise; it matches what the login saved, so it still counts as
        // the default for a later login.
        let prev = self.model.replace(a.spec.clone());
        match self.reload() {
            Ok(()) => {
                self.app.notice(format!("model: {} ({saved})", a.spec));
                Some(format!("Now using {} ({saved}).", a.spec))
            }
            Err(e) => {
                self.model = prev;
                self.app
                    .error(format!("could not switch to {}: {e:#}", a.spec));
                None
            }
        }
    }

    /// Carry out what the sign-in picker asked for.
    fn login_action(&mut self, out: login_view::Outcome) {
        use login_view::Outcome;
        match out {
            Outcome::Login(req) => self.start_login(req),
            Outcome::Logout(id) => {
                let r = crate::cli::logout(&codesmith_core::paths::auth_file(), &id)
                    .map_err(|e| format!("{e:#}"));
                self.refresh_login();
                if let Some(Modal::Login(v)) = self.app.modal.as_mut() {
                    v.signed_out(r);
                }
            }
            // Only http(s) pages open (see `open_browser`).
            Outcome::OpenUrl(url) => codesmith_core::mcp::oauth::open_browser(&url),
            Outcome::Paste(s) => {
                if let Some((_, _, tx)) = &self.login {
                    let _ = tx.try_send(s.0);
                }
            }
            Outcome::Cancel => {
                // Dropping the login closes its callback listener.
                if let Some((_, task, _)) = self.login.take() {
                    task.abort();
                }
            }
            Outcome::Nothing | Outcome::Close => {}
        }
    }

    /// Run a sign-in from the picker on its own task: what it would print
    /// comes back as `Ev::Login`, the browser opens as from the terminal,
    /// and nothing reads the terminal.
    fn start_login(&mut self, req: crate::cli::LoginRequest) {
        use login_view::LoginEv;
        if let Some((_, task, _)) = self.login.take() {
            task.abort();
        }
        self.login_seq += 1;
        let n = self.login_seq;
        self.login_provider = Some(req.provider.clone());
        let (paste_tx, paste_rx) = tokio::sync::mpsc::channel(4);
        let tx = self.tx.clone();
        let send = move |e: LoginEv| {
            let _ = tx.send(Ev::Login(n, e));
        };
        let io = crate::cli::LoginIo {
            open: Arc::new({
                let send = send.clone();
                move |url: &str, _paste: bool| {
                    send(LoginEv::Url(url.to_string()));
                    codesmith_core::mcp::oauth::open_browser(url);
                }
            }),
            device: Arc::new({
                let send = send.clone();
                move |uri: &str, code: &str, complete: Option<&str>| {
                    send(LoginEv::Device {
                        uri: uri.to_string(),
                        code: code.to_string(),
                    });
                    codesmith_core::mcp::oauth::open_browser(complete.unwrap_or(uri));
                }
            }),
            note: Arc::new({
                let send = send.clone();
                move |m: &str| send(LoginEv::Note(m.to_string()))
            }),
            pasted: crate::cli::Pasted::Channel(paste_rx),
            terminal: false,
        };
        let (file, cwd) = (codesmith_core::paths::auth_file(), self.cwd.clone());
        let task = tokio::spawn(async move {
            let r = crate::cli::run_login(&file, &cwd, req, io)
                .await
                .map_err(|e| format!("{e:#}"));
            send(LoginEv::Done(r));
        });
        self.login = Some((n, task, paste_tx));
    }

    /// Edit the prompt in the external editor, the terminal handed to it
    /// and taken back after.
    async fn edit_prompt(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        cursor: settings::Cursor,
    ) {
        let Some(editor) = external::configured(&|k| std::env::var(k).ok()) else {
            self.app
                .error("Failed to open editor: Set $VISUAL or $EDITOR to open an external editor");
            return;
        };
        let value = self.app.input.text().to_string();
        let cwd = self.cwd.clone();
        pause_input(true);
        let result = tokio::task::spawn_blocking(move || {
            external::open(
                &value,
                Some(&editor),
                &cwd,
                &mut || {
                    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
                    ratatui::restore();
                },
                &mut || {},
            )
        })
        .await
        .unwrap_or_else(|e| Err(anyhow!("{e}")));
        let _ = ratatui::crossterm::terminal::enable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            ratatui::crossterm::terminal::EnterAlternateScreen,
            EnableBracketedPaste
        );
        if let Some(c) = cursor_style(cursor) {
            let _ = execute!(std::io::stdout(), c);
        }
        let _ = terminal.clear();
        pause_input(false);
        match result {
            Ok(Some(text)) => {
                let text = app::strip_escapes(external::normalize(&text));
                self.app.input.set(text);
            }
            Ok(None) => {}
            Err(e) => self.app.error(format!("Failed to open editor: {e:#}")),
        }
    }

    /// Bring the title, notifications and program status up to date, and
    /// write what changed.
    fn attend(&mut self) {
        if let Some(text) = self.app.clipboard.take() {
            let seq = term::copy(&text);
            self.outside
                .out
                .push_str(&if std::env::var_os("TMUX").is_some() {
                    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
                } else {
                    seq
                });
        }
        let views = self.tab_views();
        let title = self.app.header.title.clone();
        self.outside.observe(views.clone(), title.as_deref());
        for t in std::mem::take(&mut self.outside.toasts) {
            let title = views
                .iter()
                .find(|v| v.session == t.session)
                .and_then(|v| v.title.clone());
            let toast = toast::Toast {
                title: Some(t.title),
                message: t.message,
                variant: toast::Variant::Error,
                duration: toast::DURATION,
            };
            self.app.toast_for(&t.session, title, toast);
        }
        for (_, file) in std::mem::take(&mut self.outside.sounds) {
            match file {
                Some(f) => outside::play(f, self.outside.attention.volume),
                None => self.outside.out.push('\x07'),
            }
        }
        self.flush_outside();
    }

    /// Keep the prompts submitted since in the history file.
    fn keep_history(&mut self) {
        for e in std::mem::take(&mut self.app.history_new) {
            self.history.append(e);
        }
    }

    fn flush_outside(&mut self) {
        if self.outside.out.is_empty() {
            return;
        }
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(std::mem::take(&mut self.outside.out).as_bytes());
        let _ = out.flush();
    }

    /// While the shell panel or a command's output is open, list the
    /// session's background commands and read the output's new part.
    /// Returns whether the output viewer is open (it is looked at again
    /// after [`SHELL_POLL`]).
    fn sync_shells(&mut self) -> bool {
        match &mut self.app.modal {
            Some(Modal::Panel(_)) => {}
            Some(Modal::ShellOutput(v)) => {
                v.status = panel::status(self.jobs.job(&v.id).as_ref());
                v.refresh();
            }
            _ => return false,
        }
        self.app.shells = self
            .jobs
            .running_jobs()
            .into_iter()
            .filter(|j| j.notice.is_none())
            .map(|j| panel::Shell {
                id: j.id,
                command: j.command,
                file: j.file,
            })
            .collect();
        matches!(self.app.modal, Some(Modal::ShellOutput(_)))
    }

    async fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Key(k) => {
                let result = match self.app.on_key(k) {
                    Some(Action::Submit(t)) if user_command(&t).is_some() => {
                        let command = user_command(&t).unwrap_or_default().to_string();
                        self.user_shell(command);
                        Ok(())
                    }
                    Some(Action::Submit(t)) => {
                        let limits = self.loaded.config.media.image;
                        let pending = std::mem::take(&mut self.app.attachments);
                        match attach::load(&pending, &t, &limits) {
                            Ok(pasted) => {
                                let mut e = attach::expand(&t, &self.cwd, &limits);
                                e.media.extend(pasted);
                                let mut names = e.files;
                                names.extend(
                                    pending
                                        .iter()
                                        .filter(|p| t.contains(&p.label))
                                        .map(|p| p.label.clone()),
                                );
                                self.submit(t, e.text, e.media);
                                if !names.is_empty() {
                                    self.app.notice(format!("attached {}", names.join(", ")));
                                }
                                Ok(())
                            }
                            Err(err) => {
                                // Keep the prompt and its attachments to fix and resend.
                                self.app.input.set(t);
                                self.app.attachments = pending;
                                Err(anyhow::anyhow!(err))
                            }
                        }
                    }
                    Some(Action::Slash { name, args }) => self.slash(&name, &args),
                    Some(Action::Interrupt) => {
                        self.interrupt().await;
                        Ok(())
                    }
                    Some(Action::Background) => {
                        let moved = self.jobs.background_all();
                        self.app.notice(match moved.len() {
                            0 => "Nothing is running in the foreground.".to_string(),
                            1 => format!("Moved {} to the background.", moved[0].id),
                            n => format!("Moved {n} commands to the background."),
                        });
                        Ok(())
                    }
                    Some(Action::Picked(kind, id)) => self.picked(kind, id),
                    Some(Action::Preview(Pick::Theme, id)) => {
                        self.set_theme(&id, false);
                        Ok(())
                    }
                    Some(Action::Cancelled(Pick::Theme)) => {
                        if let Some(before) = self.theme_before.take() {
                            self.set_theme(&before, false);
                        }
                        Ok(())
                    }
                    Some(Action::Preview(..) | Action::Cancelled(_)) => Ok(()),
                    Some(Action::McpSignIn(name)) => {
                        self.mcp_sign_in(name);
                        Ok(())
                    }
                    Some(Action::Login(out)) => {
                        self.login_action(out);
                        Ok(())
                    }
                    Some(Action::Investigate(prompt)) => self.slash("new", "").map(|()| {
                        self.app.restore_prompt(&prompt);
                    }),
                    Some(Action::KillShell(id)) => {
                        self.jobs.cancel(&id);
                        Ok(())
                    }
                    Some(Action::OpenUrl(url)) => {
                        // Only http(s) links open (see `open_browser`).
                        codesmith_core::mcp::oauth::open_browser(&url);
                        Ok(())
                    }
                    Some(Action::Run(id)) => {
                        self.run_command(id);
                        Ok(())
                    }
                    Some(Action::Export(out)) => {
                        self.export(out);
                        Ok(())
                    }
                    Some(Action::Open(t)) => self.open_target(t),
                    Some(Action::MoveSession { session, dir }) => {
                        self.move_session(session, dir);
                        Ok(())
                    }
                    Some(Action::CycleAgent(d)) => self.cycle_agent(d),
                    Some(Action::Favorite(id)) => {
                        self.toggle_favorite(&id);
                        Ok(())
                    }
                    Some(Action::Quit) | None => Ok(()),
                };
                if let Err(e) = result {
                    self.app.error(format!("{e:#}"));
                }
            }
            Ev::Paste(s) => self.app.on_paste(&s),
            Ev::Resize => {}
            Ev::Focus(f) => {
                self.outside.focus = if f {
                    attention::Focus::Focused
                } else {
                    attention::Focus::Blurred
                }
            }
            Ev::Hangup => self.app.quit = true,
            Ev::Files(f) => {
                self.app.set_files(f);
            }
            Ev::Login(n, e) => {
                if self.login.as_ref().is_some_and(|l| l.0 == n) {
                    let done = matches!(e, login_view::LoginEv::Done(_));
                    let ok = matches!(e, login_view::LoginEv::Done(Ok(_)));
                    if done {
                        self.login = None;
                    }
                    if let Some(Modal::Login(v)) = self.app.modal.as_mut() {
                        v.event(e);
                    }
                    if done {
                        let provider = self.login_provider.take();
                        // Signing in selects the provider, unless a model
                        // was chosen.
                        let adopted = provider.filter(|_| ok).and_then(|p| self.adopt_login(&p));
                        if let (Some(line), Some(Modal::Login(v))) =
                            (adopted, self.app.modal.as_mut())
                        {
                            v.add_note(&line);
                        }
                        self.refresh_login();
                    }
                }
            }
            Ev::Swap(swap::Progress::Line(l)) => {
                if let Some(Modal::Swap(v)) = self.app.modal.as_mut() {
                    v.push(l);
                }
            }
            Ev::Swap(swap::Progress::Done { provider, result }) => {
                let outcome = match result {
                    Ok(id) => {
                        let spec = format!("{provider}/{id}");
                        refresh_local_models(&self.loaded.config, self.tx.clone());
                        match self.set_model(&spec) {
                            Ok(()) => swap::Status::Done(spec),
                            Err(e) => swap::Status::Failed(format!(
                                "the server loaded {id}, but {spec} could not be selected: {e:#}"
                            )),
                        }
                    }
                    Err(e) => swap::Status::Failed(e),
                };
                match (&outcome, self.app.modal.as_mut()) {
                    (_, Some(Modal::Swap(v))) => v.status = outcome,
                    (swap::Status::Done(spec), _) => {
                        self.app.notice(format!("the server now serves {spec}"))
                    }
                    (swap::Status::Failed(e), _) => {
                        self.app.error(format!("model swap failed: {e}"))
                    }
                    (swap::Status::Running, _) => {}
                }
            }
            Ev::LocalModels => {
                let items = self.model_items();
                if let Some(Modal::Picker(p)) = self.app.modal.as_mut() {
                    if p.kind == Pick::Model {
                        p.replace_items(items);
                    }
                }
            }
            // An interrupted turn that finished anyway: already reloaded,
            // or reloading once `Stopped` arrives.
            Ev::Done { .. } if self.skip_done > 0 || self.stopping => {
                self.skip_done = self.skip_done.saturating_sub(1);
                self.app.drop_forming();
            }
            Ev::Stopped => self.stopped(),
            // Acted on by `watch_background` before the next redraw.
            Ev::Background => {}
            Ev::Done { result, session } => {
                self.task = None;
                self.app.busy = false;
                self.app.drop_forming();
                self.app.ended = Some(match &result {
                    Ok(()) => app::TurnEnd::Done,
                    Err(e) => app::TurnEnd::Failed(e.clone()),
                });
                self.app.end_turn(false);
                match (result, self.shell_command.take()) {
                    (Err(e), Some(command)) => self.app.shell_failed(&command, &e),
                    (Err(e), None) => self.app.error(e),
                    _ => {}
                }
                self.install(*session, false);
                self.app.diff_turn_done();
            }
            Ev::Diff(r) => self.app.diff_answer(*r),
            Ev::Browse(r) => {
                let result = match self.app.browse_answer(*r) {
                    Some(Action::Open(t)) => self.open_target(t),
                    Some(Action::MoveSession { session, dir }) => {
                        self.move_session(session, dir);
                        Ok(())
                    }
                    _ => Ok(()),
                };
                if let Err(e) = result {
                    self.app.error(format!("{e:#}"));
                }
            }
            other => self.app.on_event(other),
        }
    }
}

/// How long ago a unix timestamp (seconds or milliseconds) was.
fn ago(t: i64) -> String {
    let secs = if t > 10_000_000_000 { t / 1000 } else { t };
    let d = (chrono::Utc::now().timestamp() - secs).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

/// How long the model picker waits for the local servers it asks.
const LOCAL_MODELS_WAIT: Duration = Duration::from_secs(3);

/// Ask the local servers that may be asked unprompted for their models (as
/// `codesmith models` does), in the background and briefly, then send
/// `Ev::LocalModels`. One refresh runs at a time.
fn refresh_local_models(cfg: &config::Config, tx: UnboundedSender<Ev>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let found = tokio::time::timeout(
            LOCAL_MODELS_WAIT,
            codesmith_core::providers::local::discover_local(
                &cfg,
                &codesmith_core::providers::live::file(),
            ),
        )
        .await;
        RUNNING.store(false, Ordering::SeqCst);
        if found.is_ok_and(|ids| !ids.is_empty()) {
            let _ = tx.send(Ev::LocalModels);
        }
    });
}

/// Models for the picker: every configured model, the models the local
/// servers listed when last asked, then the built-in providers without
/// listed models (choosing one prefills `/model p/`).
fn model_items(cfg: &config::Config, current: &str) -> Vec<PickItem> {
    let mut items: Vec<PickItem> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (pid, p) in &cfg.providers {
        let pname = p.name.clone().unwrap_or_else(|| pid.clone());
        for (mid, m) in &p.models {
            let spec = format!("{pid}/{mid}");
            seen.insert(pid.clone());
            items.push(PickItem {
                current: spec == current,
                detail: match (&m.name, m.limit.context) {
                    (Some(n), Some(c)) => format!("{n} · {pname} · {}K context", c / 1000),
                    (Some(n), None) => format!("{n} · {pname}"),
                    (None, Some(c)) => format!("{pname} · {}K context", c / 1000),
                    (None, None) => pname.clone(),
                },
                label: spec.clone(),
                id: spec,
            });
        }
    }
    for pid in codesmith_core::providers::local::providers(cfg) {
        let Some(p) = cfg.provider(&pid) else {
            continue;
        };
        let pname = p.name.clone().unwrap_or_else(|| pid.clone());
        for mid in cfg.list_models(&pid, &[]) {
            let spec = format!("{pid}/{mid}");
            if items.iter().any(|i| i.id == spec) || codesmith_core::providers::hidden(&pid, &mid) {
                continue;
            }
            seen.insert(pid.clone());
            let context = cfg.resolve_model(&spec).ok().and_then(|m| m.context);
            items.push(PickItem {
                current: spec == current,
                detail: match context {
                    Some(c) => format!("{pname} · local · {}K context", c / 1000),
                    None => format!("{pname} · local"),
                },
                label: spec.clone(),
                id: spec,
            });
        }
    }
    if !current.is_empty() && !items.iter().any(|i| i.current) {
        items.insert(
            0,
            PickItem {
                id: current.to_string(),
                label: current.to_string(),
                detail: "current".into(),
                current: true,
            },
        );
    }
    for (id, name, ..) in config::BUILTIN {
        if !seen.contains(*id) {
            items.push(PickItem {
                id: format!("{id}/"),
                label: format!("{id}/…"),
                detail: format!("{name} · type a model id"),
                current: false,
            });
        }
    }
    items
}

/// Close as on exit when the terminal hangs up.
fn spawn_hangup(tx: UnboundedSender<Ev>) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut s) = signal(SignalKind::hangup()) else {
            return;
        };
        while s.recv().await.is_some() {
            if tx.send(Ev::Hangup).is_err() {
                break;
            }
        }
    });
    #[cfg(not(unix))]
    drop(tx);
}

/// The input thread stops reading while this is set (an external editor
/// has the terminal), and says so in [`INPUT_IDLE`].
static INPUT_PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INPUT_IDLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stop (or start again) reading the terminal's input. Pausing waits, for
/// at most half a second, until the reader is idle.
fn pause_input(pause: bool) {
    use std::sync::atomic::Ordering;
    INPUT_PAUSED.store(pause, Ordering::SeqCst);
    if !pause {
        return;
    }
    let until = std::time::Instant::now() + Duration::from_millis(500);
    while !INPUT_IDLE.load(Ordering::SeqCst) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Where the input thread sends: the running UI's channel (the thread
/// outlives a UI that starts again in another folder).
static INPUT_TX: std::sync::Mutex<Option<UnboundedSender<Ev>>> = std::sync::Mutex::new(None);
static INPUT_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the input thread runs (a UI ran before in this process).
fn input_started() -> bool {
    INPUT_STARTED.load(std::sync::atomic::Ordering::SeqCst)
}

fn spawn_input(tx: UnboundedSender<Ev>) {
    use std::sync::atomic::Ordering;
    *INPUT_TX.lock().unwrap() = Some(tx);
    if INPUT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || loop {
        if INPUT_PAUSED.load(Ordering::SeqCst) {
            INPUT_IDLE.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        INPUT_IDLE.store(false, Ordering::SeqCst);
        // Polled, so a pause is seen within one wait.
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break,
        }
        if INPUT_PAUSED.load(Ordering::SeqCst) {
            continue;
        }
        let ev = match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => Ev::Key(k),
            Ok(Event::Paste(s)) => Ev::Paste(s),
            Ok(Event::Resize(..)) => Ev::Resize,
            Ok(Event::FocusGained) => Ev::Focus(true),
            Ok(Event::FocusLost) => Ev::Focus(false),
            Ok(_) => continue,
            Err(_) => break,
        };
        let tx = INPUT_TX.lock().unwrap().clone();
        // Between two UIs (or after the last) a key goes nowhere.
        if let Some(tx) = tx {
            let _ = tx.send(ev);
        }
    });
}

/// The cursor shape `tui.cursor` asks for.
fn cursor_style(c: settings::Cursor) -> Option<ratatui::crossterm::cursor::SetCursorStyle> {
    use ratatui::crossterm::cursor::SetCursorStyle as S;
    use settings::CursorStyle;
    Some(match (c.style, c.blinking) {
        (CursorStyle::Default, _) => return None,
        (CursorStyle::Block, true) => S::BlinkingBlock,
        (CursorStyle::Block, false) => S::SteadyBlock,
        (CursorStyle::Underline, true) => S::BlinkingUnderScore,
        (CursorStyle::Underline, false) => S::SteadyUnderScore,
        (CursorStyle::Line, true) => S::BlinkingBar,
        (CursorStyle::Line, false) => S::SteadyBar,
    })
}

/// Stop to the shell (`ctrl+z`), and take the terminal back on `fg`.
#[cfg(unix)]
fn suspend(terminal: &mut ratatui::DefaultTerminal, cursor: settings::Cursor) {
    use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // SAFETY: raising a signal at ourselves; the default action stops the
    // process until it is continued.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    let _ = enable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste
    );
    if let Some(c) = cursor_style(cursor) {
        let _ = execute!(std::io::stdout(), c);
    }
    let _ = terminal.clear();
}

#[cfg(not(unix))]
fn suspend(_: &mut ratatui::DefaultTerminal, _: settings::Cursor) {}

pub async fn run(cwd: PathBuf, opts: Opts) -> Result<()> {
    let launch = cwd.clone();
    let (mut cwd, mut opts) = (cwd, opts);
    loop {
        let Some(next) = run_in(cwd, &launch, &opts).await? else {
            return Ok(());
        };
        // The open menu chose a session or folder elsewhere: start again
        // there, as if launched in it.
        std::env::set_current_dir(&next.cwd)?;
        cwd = next.cwd;
        opts.resume = next.resume;
        opts.prompt = None;
    }
}

/// Run the UI in `cwd` until it closes; where to start it again, when the
/// open menu chose another folder.
async fn run_in(
    cwd: PathBuf,
    launch: &std::path::Path,
    opts: &Opts,
) -> Result<Option<browse_host::Relaunch>> {
    let opts = Opts {
        resume: opts.resume.clone(),
        agent: opts.agent.clone(),
        model: opts.model.clone(),
        prompt: opts.prompt.clone(),
        auto: opts.auto,
    };
    // Started again in another folder: its project may need an answer.
    crate::cli::trust_prompt(&cwd)?;
    let loaded = config::load(&cwd)?;
    let (tui, mut tui_warnings) = settings::load(&loaded.config.tui);
    tui_warnings.extend(loaded.config.trust.notice());
    let (custom, theme_errors) = theme::discover(&theme::theme_dirs(
        &codesmith_core::paths::config_dir(),
        &cwd,
        &loaded.root,
    ));
    let (remembered, remembered_mode) = theme::remembered(tui.theme.as_deref());
    let themes = theme::Themes::new(
        remembered.as_deref().or(tui.theme.as_deref()),
        remembered_mode.or(tui.mode),
        None,
        custom,
    );
    let (tx, mut rx): (UnboundedSender<Ev>, UnboundedReceiver<Ev>) = unbounded_channel();
    let mut tabs = tabs_host::Host::new(tx.clone(), opts.auto, cwd.clone(), tui.tabs);
    // Each tab's session talks on a channel of its own.
    let (first_tab, parts) = tabs.parts();
    {
        // Once per process: the scheduler is the store's, not the folder's.
        use std::sync::atomic::{AtomicBool, Ordering};
        static SCHEDULED: AtomicBool = AtomicBool::new(false);
        if !SCHEDULED.swap(true, Ordering::SeqCst) {
            codesmith_core::automation::spawn_scheduler(&loaded);
        }
    }
    let mcp = crate::connect_mcp(&loaded, false).await;
    let mcp_failures: Vec<String> = mcp
        .statuses()
        .iter()
        .filter_map(|(n, s)| match s {
            codesmith_core::mcp::Status::Failed(e) => {
                Some(format!("MCP server {n} failed to start: {e}"))
            }
            codesmith_core::mcp::Status::NeedsAuth => Some(format!(
                "MCP server {n} needs authorization: run `codesmith mcp auth {n}`"
            )),
            _ => None,
        })
        .collect();
    let mut r = Runner {
        app: App::new(),
        loaded,
        cwd,
        tx: parts.tx,
        asker: parts.asker,
        questioner: parts.questioner,
        mcp,
        agent: opts.agent,
        model: opts.model,
        session: None,
        session_id: String::new(),
        task: None,
        prompter: parts.prompter,
        approvals: Approvals::default(),
        stopping: false,
        skip_done: 0,
        waiter: None,
        resume_held: None,
        jobs: Jobs::default(),
        themes,
        configured_theme: tui.theme.clone(),
        detected: None,
        theme_before: None,
        suspend: false,
        edit: false,
        diffs: settings::Diffs::default(),
        tabs,
        history: history::Store::default(),
        models: models::Repository::new(models::Repository::default_file()),
        selections: Default::default(),
        shell_command: None,
        goals: codesmith_core::plugin::goal::shared().subscribe(),
        relaunch: None,
        launch: launch.to_path_buf(),
        new_inherit: tui.new_inherit,
        new_here: false,
        login: None,
        login_provider: None,
        login_seq: 0,
        outside: outside::Outside::new(tui.terminal_title, tui.attention.clone(), &|k| {
            std::env::var(k).ok()
        }),
    };
    let keymap = keymap::Keymap::new(&tui.keybinds, tui.leader_timeout, cfg!(unix));
    for w in tui_warnings
        .iter()
        .chain(&keymap.warnings)
        .chain(&theme_errors)
    {
        r.app.error(w.clone());
    }
    r.app.keymap = Arc::new(keymap);
    r.app.show_reasoning = tui.thinking.unwrap_or(false);
    r.app.reasoning_source = tui.markdown == settings::Markdown::Source;
    r.app.verbosity = tui.verbosity;
    r.app.grouping = tui.grouping;
    r.app.tps = tui.tps;
    r.app.paste_full = tui.paste_full;
    r.app.turn_tokens = tui.turn_tokens;
    r.diffs = tui.diffs;
    r.app.diff_source = tui.diffs.source;
    r.app.diff_prefs = diff_view::remembered(
        &tui.diffs,
        std::fs::read_to_string(diff_view::state_file())
            .ok()
            .as_deref(),
    );
    let cmds: Vec<(String, String)> = crate::all_commands_with(&r.cwd, &r.loaded, &r.mcp)
        .into_iter()
        .map(|(n, c)| (n, c.description.unwrap_or_default()))
        .collect();
    r.app.set_commands(&cmds);
    let skills: Vec<(String, String)> = codesmith_core::skill::for_folder(&r.cwd, &r.loaded)
        .into_iter()
        .map(|s| (s.id, s.description))
        .collect();
    r.app
        .set_mentions(&agent::subagents(&r.loaded.config), &skills);
    r.app.cwd = r.cwd.clone();
    r.history = history::Store::load(history::file());
    r.app.stash = stash::Stash::load(stash::Stash::file());
    r.app.frecency = stash::Frecency::load(stash::Frecency::file());
    for e in &r.history.entries {
        r.app.history.push(history::text(e));
        r.app.history_files.push(history::files(e));
    }
    {
        let (tx, root) = (tx.clone(), r.cwd.clone());
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Ev::Files(complete::project_files(&root, 20_000)));
        });
    }
    let first = r.make(opts.resume.clone())?;
    r.install(first, opts.resume.is_some());
    r.start_tabs(first_tab, opts.resume.is_some());
    if r.app.entries.is_empty() {
        r.app.notice(format!(
            "CodeSmith {} · {} · /help for commands",
            env!("CARGO_PKG_VERSION"),
            r.cwd.display()
        ));
    }
    for f in mcp_failures {
        r.app.notice(f);
    }
    if let Some(p) = opts.prompt.filter(|p| !p.trim().is_empty()) {
        match user_command(&p) {
            Some(c) => r.user_shell(c.to_string()),
            None => r.submit(p.clone(), p, vec![]),
        }
    }

    let mut terminal = ratatui::init();
    let _ = execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        ratatui::crossterm::event::EnableFocusChange
    );
    if let Some(c) = cursor_style(tui.cursor) {
        let _ = execute!(std::io::stdout(), c);
    }
    // The terminal's colors give the mode (unless one is pinned) and the
    // `system` theme. Asked before input is read, so the reply is not
    // taken for keys.
    let reading = input_started();
    if reading {
        pause_input(true);
    }
    let colors = theme::system::query(Duration::from_millis(200));
    if reading {
        pause_input(false);
    }
    if let Some(colors) = colors {
        r.detected = theme::system::terminal_mode(&colors);
        r.themes.set_palette(colors);
    }
    r.apply_theme();
    spawn_hangup(tx.clone());
    spawn_input(tx);
    let result: Result<()> = async {
        loop {
            r.watch_all();
            r.sync_tabs();
            r.attend();
            r.keep_history();
            while let Ok(c) = r.goals.try_recv() {
                if c.session_id == r.app.header.session {
                    r.app.goal = c.goal.map(|g| g.status);
                }
            }
            let following = r.sync_shells();
            let toast = r.app.toasts.tick(std::time::Instant::now());
            terminal.draw(|f| view::draw(f, &r.app))?;
            let tick = match (following.then_some(SHELL_POLL), toast) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let ev = match tick {
                // The spinner moves while nothing else comes in.
                Some(d) => match tokio::time::timeout(d, rx.recv()).await {
                    Ok(ev) => ev,
                    Err(_) => continue,
                },
                None => rx.recv().await,
            };
            let Some(ev) = ev else { break };
            r.dispatch(ev).await;
            // Apply everything already queued (streamed text arrives in bursts) before redrawing.
            while let Ok(ev) = rx.try_recv() {
                r.dispatch(ev).await;
            }
            r.pump_diff();
            r.pump_browser();
            if r.app.quit {
                break;
            }
            if std::mem::take(&mut r.suspend) {
                suspend(&mut terminal, tui.cursor);
            }
            if std::mem::take(&mut r.edit) {
                r.edit_prompt(&mut terminal, tui.cursor).await;
            }
        }
        Ok(())
    }
    .await;
    r.close_tabs().await;
    r.stop_waiting();
    if let Some(mut t) = r.task.take() {
        if let Some(p) = &r.prompter {
            p.set_stopping(true);
        }
        r.app.cancel_modal();
        t.abort();
        // Let it stop writing to the store before the process exits.
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut t).await;
    }
    r.outside.close();
    r.flush_outside();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        ratatui::crossterm::event::DisableFocusChange,
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    ratatui::restore();
    if let Some(next) = r.relaunch.take() {
        result?;
        return Ok(Some(next));
    }
    // What continues the session, once it is in the store.
    let stored = (!r.session_id.is_empty())
        .then(open_store)
        .flatten()
        .and_then(|db| db.get(&r.session_id).ok().flatten());
    if let Some(info) = stored {
        let title = info
            .title
            .unwrap_or_else(|| tabs::NEW_SESSION_TITLE.to_string());
        if let Some(text) = term::epilogue(Some((&title, &info.id)), "codesmith", None) {
            println!("{text}");
        }
    }
    result.map(|()| None)
}

/// The sessions `/sessions` lists: the top-level ones of the UI's folder,
/// which is the active session's (one elsewhere restarts the UI there).
/// Archived sessions stay out, as in OpenCode's session list.
fn folder_sessions(db: &Store, cwd: &std::path::Path) -> Result<Vec<store::Info>> {
    db.list_archived(Some(&cwd.display().to_string()), false, Some(false))
}

/// The command of a `!command` prompt (OpenCode's shell mode).
fn user_command(text: &str) -> Option<&str> {
    let c = text.trim_start().strip_prefix('!')?.trim();
    (!c.is_empty()).then_some(c)
}

/// The text of a prompt, if it has any.
fn prompt_text(m: &codesmith_ai::Message) -> Option<String> {
    let text: Vec<&str> = m
        .parts
        .iter()
        .filter_map(|p| match p {
            codesmith_ai::Part::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// The UI's own commands, and whether each takes an argument.
const BUILTINS: &[(&str, bool)] = &[
    ("exit", false),
    ("quit", false),
    ("q", false),
    ("help", false),
    ("clear", false),
    ("thinking", false),
    ("verbosity", true),
    ("themes", true),
    ("theme", true),
    ("new", false),
    ("sessions", false),
    ("open", false),
    ("projects", false),
    ("project", false),
    ("worktrees", false),
    ("fork", false),
    ("undo", false),
    ("redo", false),
    ("compact", false),
    ("export", false),
    ("login", false),
    ("agent", true),
    ("model", true),
    ("variant", true),
    ("diff", false),
    ("editor", false),
];

/// The built-in command `/name args` invokes, if any. As in OpenCode
/// (`mini/prompt.shared`), a built-in's name ignores case, and one that
/// takes no argument is only itself without one: `/Quit` quits, `/quit now`
/// does not.
pub(crate) fn builtin(name: &str, args: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|(n, takes)| n.eq_ignore_ascii_case(name) && (*takes || args.is_empty()))
        .map(|(n, _)| *n)
}

/// Whether the session's model was chosen (on the command line, in this
/// session, or by its agent) rather than left to the default; a model a
/// login chose (`auto`) was not.
fn model_chosen(session: Option<&str>, auto: Option<&str>, agent_model: bool) -> bool {
    agent_model || session.is_some_and(|m| auto != Some(m))
}

#[cfg(test)]
mod login_switch_tests {
    use super::model_chosen;

    #[test]
    fn only_a_model_left_to_the_default_follows_a_login() {
        assert!(!model_chosen(None, None, false));
        assert!(!model_chosen(None, Some("claude-agent/opus"), false));
        // The one an earlier login picked is still the default.
        assert!(!model_chosen(
            Some("claude-agent/opus"),
            Some("claude-agent/opus"),
            false
        ));
        // `-m`, /model, the model picker.
        assert!(model_chosen(Some("openai/gpt-5"), None, false));
        assert!(model_chosen(
            Some("openai/gpt-5"),
            Some("claude-agent/opus"),
            false
        ));
        // The agent's own model.
        assert!(model_chosen(None, None, true));
    }
}
