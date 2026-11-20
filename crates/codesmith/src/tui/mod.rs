//! The interactive terminal UI (`codesmith` with no subcommand).
//!
//! The session runs each turn on its own task and talks to the UI through
//! `bridge::Ev` messages; the UI owns the session between turns. Every
//! message is already in the store, so an interrupted turn is recovered by
//! reloading the session.

mod app;
mod attach;
#[cfg(test)]
mod bench;
mod bridge;
mod complete;
mod diff;
mod editor;
mod markdown;
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
        Ok(s)
    }

    /// Start over with no approvals, for a different session.
    fn forget_approvals(&mut self) {
        self.approvals = Approvals::default();
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
        if replay {
            self.app.entries = app::replay(&s.messages);
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

    /// Run an MCP prompt command: fetch the prompt, then send it.
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
        self.app.entries.push(Entry::User(shown));
        self.app.busy = true;
        let tx = self.tx.clone();
        let cwd = s.cwd.clone();
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink::new(tx.clone(), cwd);
            let result = match s.expand_command(&c, &args).await {
                Ok(text) => s.prompt(&text, &mut sink).await,
                Err(e) => Err(e),
            }
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
        self.app.notice("interrupted");
        if let Err(e) = self.reload() {
            self.app.error(format!("{e:#}"));
        }
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
        match name {
            "exit" | "quit" | "q" => self.app.quit = true,
            "help" => self.app.modal = Some(Modal::Help),
            "clear" => self.app.entries.clear(),
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
            "new" if self.idle_or_say() => {
                self.forget_approvals();
                let s = self.make(None)?;
                self.app.entries.clear();
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
            "new" | "sessions" | "undo" | "redo" | "agent" | "model" | "variant" | "compact" => {}
            other => {
                let all = crate::all_commands_with(&self.cwd, &self.loaded, &self.mcp);
                let c = all
                    .get(other)
                    .ok_or_else(|| anyhow!("unknown command /{other}; /help lists commands"))?
                    .clone();
                if c.native {
                    self.native(other, args);
                    return Ok(());
                }
                let mut rebuild = false;
                if c.agent.is_some() && c.agent != self.agent {
                    self.agent = c.agent.clone();
                    rebuild = true;
                }
                if c.model.is_some() && c.model != self.model {
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
                if c.mcp.is_some() {
                    self.submit_command(shown, c, args.to_string());
                } else {
                    self.submit(shown, command::expand(&c.template, args), vec![]);
                }
            }
        }
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
                agent::resolve(&self.loaded.config, id).is_some_and(|a| a.mode != "subagent")
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
            Pick::Session => {
                if id != self.session_id {
                    self.forget_approvals();
                }
                let s = self.make(Some(id))?;
                self.install(s, true);
            }
            Pick::Agent if self.idle_or_say() => self.set_agent(&id)?,
            // A provider without listed models: let the user type the model.
            Pick::Model if id.ends_with('/') => self.app.input.set(format!("/model {id}")),
            Pick::Model if self.idle_or_say() => self.set_model(&id)?,
            Pick::Agent | Pick::Model => {}
        }
        Ok(())
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
                    Some(Action::Picked(kind, id)) => self.picked(kind, id),
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
            }
            Ev::Stopped => self.stopped(),
            Ev::Done { result, session } => {
                self.task = None;
                self.app.busy = false;
                if let Err(e) = result {
                    self.app.error(e);
                }
                self.install(*session, false);
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

pub async fn run(cwd: PathBuf, opts: Opts) -> Result<()> {
    let loaded = config::load(&cwd)?;
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
    };
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
        r.submit(p.clone(), p, vec![]);
    }

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    spawn_input(tx);
    let result: Result<()> = async {
        loop {
            terminal.draw(|f| view::draw(f, &r.app))?;
            let Some(ev) = rx.recv().await else { break };
            r.handle(ev).await;
            // Apply everything already queued (streamed text arrives in bursts) before redrawing.
            while let Ok(ev) = rx.try_recv() {
                r.handle(ev).await;
            }
            if r.app.quit {
                break;
            }
        }
        Ok(())
    }
    .await;
    if let Some(mut t) = r.task.take() {
        if let Some(p) = &r.prompter {
            p.set_stopping(true);
        }
        r.app.cancel_modal();
        t.abort();
        // Let it stop writing to the store before the process exits.
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut t).await;
    }
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    if !r.session_id.is_empty() {
        println!(
            "Session {} — continue with: codesmith -s {}",
            r.session_id, r.session_id
        );
    }
    result
}
