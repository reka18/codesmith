//! The interactive terminal UI (`codesmith` with no subcommand).
//!
//! The session runs each turn on its own task and talks to the UI through
//! `bridge::Ev` messages; the UI owns the session between turns. Every
//! message is already in the store, so an interrupted turn is recovered by
//! reloading the session.

mod app;
mod bridge;
mod view;

use anyhow::{anyhow, Result};
use app::{Action, App, Entry, Modal};
use bridge::{ChanPrompter, ChanSink, Ev};
use codesmith_core::config::{self, Loaded};
use codesmith_core::permission::{Asker, Fixed, Reply};
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
}

fn open_store() -> Option<Store> {
    Store::open(&store::default_path()).ok()
}

impl Runner {
    fn make(&self, resume: Option<String>) -> Result<Session> {
        Session::new(
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
        )
    }

    /// Install a session and refresh the header (and transcript when continuing).
    fn install(&mut self, s: Session, replay: bool) {
        self.app.header.agent = s.agent.name.clone();
        self.app.header.model = format!("{}/{}", s.model.provider_id, s.model.model_id);
        self.app.header.session = s.id.clone();
        self.app.header.title = s.title.clone();
        self.app.usage = s.usage;
        if replay {
            self.app.entries = app::replay(&s.messages);
        }
        self.session_id = s.id.clone();
        self.session = Some(s);
    }

    /// Reload the current session from the store (or start fresh if it was never saved).
    fn reload(&mut self) -> Result<()> {
        let stored = open_store()
            .and_then(|db| db.get(&self.session_id).ok().flatten())
            .is_some();
        let s = self.make(stored.then(|| self.session_id.clone()))?;
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
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink(tx.clone());
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

    fn submit(&mut self, shown: String, text: String) {
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
        self.task = Some(tokio::spawn(async move {
            let mut sink = ChanSink(tx.clone());
            let result = s
                .prompt(&text, &mut sink)
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

    fn interrupt(&mut self) {
        self.app.cancel_modal();
        if let Some(t) = self.task.take() {
            t.abort();
        }
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
                let sel = list
                    .iter()
                    .position(|i| i.id == self.session_id)
                    .unwrap_or(0);
                self.app.modal = Some(Modal::Sessions { list, sel });
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
                    let ids = agent::list(&self.loaded.config);
                    self.app.notice(format!(
                        "agents: {} (current: {})",
                        ids.join(", "),
                        self.app.header.agent
                    ));
                } else {
                    let prev = self.agent.replace(args.to_string());
                    if let Err(e) = self.reload() {
                        self.agent = prev;
                        return Err(e);
                    }
                    self.app.notice(format!("agent: {args}"));
                }
            }
            "model" if self.idle_or_say() => {
                if args.is_empty() {
                    self.app.notice(format!("model: {}", self.app.header.model));
                } else {
                    let prev = self.model.replace(args.to_string());
                    if let Err(e) = self.reload() {
                        self.model = prev;
                        return Err(e);
                    }
                    self.app.notice(format!("model: {args}"));
                }
            }
            "compact" if self.idle_or_say() => {
                let Some(mut s) = self.session.take() else {
                    return Ok(());
                };
                self.app.busy = true;
                let tx = self.tx.clone();
                self.task = Some(tokio::spawn(async move {
                    let mut sink = ChanSink(tx.clone());
                    let result = s.compact(&mut sink).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Ev::Done {
                        result,
                        session: Box::new(s),
                    });
                }));
            }
            "new" | "sessions" | "undo" | "redo" | "agent" | "model" | "compact" => {}
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
                    self.submit(shown, command::expand(&c.template, args));
                }
            }
        }
        Ok(())
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Key(k) => {
                let result = match self.app.on_key(k) {
                    Some(Action::Submit(t)) => {
                        self.submit(t.clone(), t);
                        Ok(())
                    }
                    Some(Action::Slash { name, args }) => self.slash(&name, &args),
                    Some(Action::Interrupt) => {
                        self.interrupt();
                        Ok(())
                    }
                    Some(Action::Resume(id)) => self.make(Some(id)).map(|s| self.install(s, true)),
                    Some(Action::Quit) | None => Ok(()),
                };
                if let Err(e) = result {
                    self.app.error(format!("{e:#}"));
                }
            }
            Ev::Paste(s) => self.app.on_paste(&s),
            Ev::Resize => {}
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
    let prompter = Arc::new(ChanPrompter(tx.clone()));
    let asker: Arc<dyn Asker> = if opts.auto {
        Arc::new(Fixed(Reply::Once))
    } else {
        prompter.clone()
    };
    let questioner: Option<Arc<dyn Questioner>> = if opts.auto { None } else { Some(prompter) };
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
    };
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
        r.submit(p.clone(), p);
    }

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    spawn_input(tx);
    let result: Result<()> = async {
        loop {
            terminal.draw(|f| view::draw(f, &r.app))?;
            let Some(ev) = rx.recv().await else { break };
            r.handle(ev);
            // Apply everything already queued (streamed text arrives in bursts) before redrawing.
            while let Ok(ev) = rx.try_recv() {
                r.handle(ev);
            }
            if r.app.quit {
                break;
            }
        }
        Ok(())
    }
    .await;
    if let Some(t) = r.task.take() {
        r.app.cancel_modal();
        t.abort();
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
