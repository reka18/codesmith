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
#[cfg(test)]
mod bench;
mod bridge;
mod complete;
mod diff;
mod diff_draw;
mod diff_source;
mod diff_tree;
mod diff_view;
mod editor;
mod group;
pub(crate) mod keymap;
mod markdown;
pub(crate) mod mini;
#[cfg(test)]
mod parity;
#[cfg(test)]
mod parity_diff;
#[cfg(test)]
mod parity_groups;
#[cfg(test)]
mod parity_keys;
pub(crate) mod settings;
pub(crate) mod theme;
mod view;

pub(crate) use view::display_safe;

use anyhow::{anyhow, Result};
use app::{Action, App, Entry, Modal, Pick, PickItem, Picker};
use bridge::{ChanPrompter, ChanSink, Ev};
use codesmith_core::config::{self, Loaded};
use codesmith_core::permission::{Approvals, Asker, Fixed, Reply};
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

pub struct Opts {
    pub resume: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub auto: bool,
    /// Mini mode (`codesmith mini`), with its command-line overrides.
    pub mini: Option<MiniOpts>,
}

/// `codesmith mini`'s flags over `tui.mini`.
#[derive(Debug, Clone, Default)]
pub struct MiniOpts {
    pub replay: Option<bool>,
    pub replay_limit: Option<usize>,
}

/// Where the UI draws: the full screen, or mini mode's scrollback and footer.
enum Screen {
    Full(ratatui::DefaultTerminal),
    Mini(Box<mini::Mini<std::io::Stdout>>),
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
    /// the session object a turn's task dropped. Stopped when the TUI
    /// moves to another session (`/new`, `/sessions`) or exits.
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
    /// `app.clear` (mini mode): clear the screen before the next frame.
    clear: bool,
    /// `tui.diffs`, which the diff viewer's remembered layout is kept against.
    diffs: settings::Diffs,
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

    /// Leaving the current session for another: stop its background
    /// commands (the TUI shows and wakes one session only, so nothing
    /// would report them) and start the next one with none.
    fn end_jobs(&mut self) {
        self.stop_waiting();
        self.resume_held = None;
        let old = std::mem::take(&mut self.jobs);
        tokio::spawn(async move { old.stop_all().await });
    }

    /// Install a session and refresh the header (and transcript when continuing).
    fn install(&mut self, s: Session, replay: bool) {
        self.app.header.agent = s.agent.name.clone();
        self.app.header.model = format!("{}/{}", s.model.provider_id, s.model.model_id);
        self.app.header.session = s.id.clone();
        self.app.header.title = s.title.clone();
        self.app.header.context_limit = s.model.context;
        self.app.header.cost = self.loaded.config.cost(&self.app.header.model);
        if s.id != self.session_id {
            self.app.context = 0;
        }
        self.app.usage = s.usage;
        self.app.spend = s.spend();
        if replay {
            self.app.set_entries(app::replay(&s.messages));
        }
        self.session_id = s.id.clone();
        self.session = Some(s);
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
        self.app.busy = true;
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
        self.app.busy = true;
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
        self.app.busy = true;
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
        self.app.busy = true;
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
            "themes" | "theme" => self.theme_command(args),
            "clear" => self.app.set_entries(Vec::new()),
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
            "verbosity" if self.app.mini.is_some() && mini_verbosity(args).is_some() => {
                let m = self.app.mini.as_mut().expect("mini mode");
                let level = match mini_verbosity(args) {
                    Some(Ok(l)) => Some(l),
                    Some(Err(dir)) => m.step(dir),
                    None => None,
                };
                let shown = match level {
                    Some(l) => {
                        *m = m.with_level(l);
                        self.app.show_reasoning = m.thinking;
                        "set"
                    }
                    None => "already",
                };
                let label = mini::settings::label(m.level());
                self.app.notice(format!("mini verbosity {shown}: {label}"));
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
            "new" if self.idle_or_say() => {
                self.forget_approvals();
                self.end_jobs();
                let s = self.make(None)?;
                self.app.set_entries(Vec::new());
                self.install(s, false);
                self.app.notice("new session");
            }
            "sessions" if self.idle_or_say() => {
                let list = open_store()
                    .map(|db| db.list(Some(&self.cwd.display().to_string()), false))
                    .transpose()?
                    .unwrap_or_default();
                let items = list
                    .into_iter()
                    .map(|i| PickItem {
                        current: i.id == self.session_id,
                        label: i.title.clone().unwrap_or_else(|| "(untitled)".into()),
                        detail: format!("{} · {}", ago(i.updated), i.id),
                        id: i.id,
                    })
                    .collect();
                self.app.modal = Some(Modal::Picker(Picker::new(Pick::Session, "Sessions", items)));
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
                    let items = model_items(&self.loaded.config, &self.app.header.model);
                    self.app.modal = Some(Modal::Picker(Picker::new(Pick::Model, "Models", items)));
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
        if let Err(e) = self.reload() {
            self.agent = prev;
            return Err(e);
        }
        self.app.notice(format!("agent: {id}"));
        Ok(())
    }

    fn set_model(&mut self, spec: &str) -> Result<()> {
        let prev = self.model.replace(spec.to_string());
        if let Err(e) = self.reload() {
            self.model = prev;
            return Err(e);
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
            Pick::Session if self.idle_or_say() => {
                if id != self.session_id {
                    self.forget_approvals();
                    self.end_jobs();
                }
                let s = self.make(Some(id))?;
                self.install(s, true);
            }
            Pick::Agent if self.idle_or_say() => self.set_agent(&id)?,
            Pick::Theme => {
                self.theme_before = None;
                self.set_theme(&id, true);
            }
            // A provider without listed models: let the user type the model.
            Pick::Model if id.ends_with('/') => self.app.input.set(format!("/model {id}")),
            Pick::Model if self.idle_or_say() => self.set_model(&id)?,
            Pick::DiffSource | Pick::DiffBase => self.app.diff_picked(kind, &id),
            Pick::Session | Pick::Agent | Pick::Model => {}
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
            "app.clear" => self.clear = true,
            _ => {}
        }
    }

    async fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Key(k) => {
                let result = match self.app.on_key(k) {
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
                    Some(Action::Run(id)) => {
                        self.run_command(id);
                        Ok(())
                    }
                    Some(Action::CycleAgent(d)) => self.cycle_agent(d),
                    Some(Action::Quit) | None => Ok(()),
                };
                if let Err(e) = result {
                    self.app.error(format!("{e:#}"));
                }
            }
            Ev::Paste(s) => self.app.on_paste(&s),
            Ev::Resize => {}
            Ev::Files(f) => {
                self.app.set_files(f);
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
                if let Err(e) = result {
                    self.app.error(e);
                }
                self.install(*session, false);
                self.app.diff_turn_done();
            }
            Ev::Diff(r) => self.app.diff_answer(*r),
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

/// Models for the picker: every configured model, then the built-in
/// providers without listed models (choosing one prefills `/model p/`).
fn model_items(cfg: &config::Config, current: &str) -> Vec<PickItem> {
    let mut items = Vec::new();
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

fn spawn_input(tx: UnboundedSender<Ev>) {
    std::thread::spawn(move || loop {
        let ev = match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => Ev::Key(k),
            Ok(Event::Paste(s)) => Ev::Paste(s),
            Ok(Event::Resize(..)) => Ev::Resize,
            Ok(_) => continue,
            Err(_) => break,
        };
        if tx.send(ev).is_err() {
            break;
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
    let loaded = config::load(&cwd)?;
    let (tui, tui_warnings) = settings::load(&loaded.config.tui);
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
    let prompter = Arc::new(ChanPrompter::new(tx.clone()));
    let asker: Arc<dyn Asker> = if opts.auto {
        Arc::new(Fixed(Reply::Once))
    } else {
        prompter.clone()
    };
    let questioner: Option<Arc<dyn Questioner>> = if opts.auto {
        None
    } else {
        Some(prompter.clone())
    };
    codesmith_core::automation::spawn_scheduler(&loaded);
    let mcp = crate::connect_mcp(&loaded, false).await;
    let mcp_failures: Vec<String> = mcp
        .statuses
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
        tx: tx.clone(),
        asker,
        questioner,
        mcp,
        agent: opts.agent,
        model: opts.model,
        session: None,
        session_id: String::new(),
        task: None,
        prompter: (!opts.auto).then_some(prompter),
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
        clear: false,
        diffs: settings::Diffs::default(),
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
    r.app.cwd = r.cwd.clone();
    {
        let (tx, root) = (tx.clone(), r.cwd.clone());
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Ev::Files(complete::project_files(&root, 20_000)));
        });
    }
    let first = r.make(opts.resume.clone())?;
    r.install(first, opts.resume.is_some());
    // Mini mode's entry banner says as much.
    if r.app.entries.is_empty() && opts.mini.is_none() {
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
        r.submit(p.clone(), p, vec![]);
    }

    let mini_opts = opts.mini.clone();
    if let Some(m) = &mini_opts {
        let mut s = tui.mini.clone();
        s.replay = m.replay.unwrap_or(s.replay);
        s.replay_limit = m.replay_limit.unwrap_or(s.replay_limit);
        r.app.show_reasoning = s.thinking;
        r.app.mini = Some(s);
    }
    let mut screen = match &mini_opts {
        None => Screen::Full(ratatui::init()),
        Some(_) => Screen::Mini(Box::new(mini_screen(&r.cwd)?)),
    };
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    if let Some(c) = cursor_style(tui.cursor) {
        let _ = execute!(std::io::stdout(), c);
    }
    // The terminal's colors give the mode (unless one is pinned) and the
    // `system` theme. Asked before input is read, so the reply is not
    // taken for keys.
    if let Some(colors) = theme::system::query(Duration::from_millis(200)) {
        r.detected = theme::system::terminal_mode(&colors);
        r.themes.set_palette(colors);
    }
    r.apply_theme();
    spawn_input(tx);
    let result: Result<()> = async {
        loop {
            r.watch_background();
            let tick = match &mut screen {
                Screen::Full(t) => {
                    t.draw(|f| view::draw(f, &r.app))?;
                    None
                }
                Screen::Mini(m) => {
                    m.draw(&mut r.app)?;
                    m.tick(&r.app)
                }
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
            r.handle(ev).await;
            // Apply everything already queued (streamed text arrives in bursts) before redrawing.
            while let Ok(ev) = rx.try_recv() {
                r.handle(ev).await;
            }
            r.pump_diff();
            if r.app.quit {
                break;
            }
            if std::mem::take(&mut r.suspend) {
                match &mut screen {
                    Screen::Full(t) => suspend(t, tui.cursor),
                    Screen::Mini(m) => m.suspend(),
                }
            }
            if std::mem::take(&mut r.clear) {
                if let Screen::Mini(m) = &mut screen {
                    m.clear();
                }
            }
        }
        Ok(())
    }
    .await;
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
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    let banner = match screen {
        Screen::Full(_) => {
            ratatui::restore();
            false
        }
        // The main screen stays as it is, the transcript on it.
        Screen::Mini(mut m) => {
            let _ = m.close(&mut r.app);
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
            r.app.mini.as_ref().is_some_and(|s| s.splash)
        }
    };
    if !r.session_id.is_empty() && !banner {
        println!(
            "Session {} — continue with: codesmith -s {}",
            r.session_id, r.session_id
        );
    }
    result
}

/// Mini mode's `/verbosity quiet|default|everything` (a preset) or
/// `/verbosity up|down` (a step, as a slider): `Err` holds the step.
fn mini_verbosity(args: &str) -> Option<std::result::Result<mini::settings::Level, i32>> {
    let a = args.trim().to_ascii_lowercase();
    match a.as_str() {
        "up" => Some(Err(1)),
        "down" => Some(Err(-1)),
        _ => mini::settings::Level::parse(&a).map(Ok),
    }
}

/// Mini mode's screen, from where the cursor is (what the shell printed
/// stays). Asked before input is read, so the reply is not taken for keys.
fn mini_screen(cwd: &std::path::Path) -> Result<mini::Mini<std::io::Stdout>> {
    use ratatui::crossterm::{cursor, terminal};
    terminal::enable_raw_mode()?;
    let (cols, rows) = terminal::size()?;
    // Without a terminal on stdin (a prompt piped in), the reply would not
    // be read: start at the bottom row.
    let asked = std::io::IsTerminal::is_terminal(&std::io::stdin())
        .then(cursor::position)
        .and_then(|r| r.ok());
    let row = match asked {
        Some((0, y)) => y,
        Some((_, y)) => y + 1,
        None => rows.saturating_sub(1),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let detail = mini::splash::directory_label(cwd, home.as_deref());
    Ok(mini::Mini::new(std::io::stdout(), cols, rows, row, detail))
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
    ("undo", false),
    ("redo", false),
    ("compact", false),
    ("agent", true),
    ("model", true),
    ("variant", true),
    ("diff", false),
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
