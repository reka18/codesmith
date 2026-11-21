//! The runner's side of the open menu and the worktrees dialog (see
//! [`super::browser`]): what they load, run in the background, and what
//! opening a session or folder does. The UI works in one folder at a time;
//! one elsewhere restarts it there ([`Relaunch`]), with that folder's
//! configuration, tabs and files, once no turn is running.

use super::app::Pick;
use super::bridge::Ev;
use super::browser::{self, Browser, Context, Request, Response, SessionRow, Target};
use super::{open_store, Runner};
use codesmith_core::config;
use codesmith_core::store::Store;
use std::path::{Path, PathBuf};

/// Where the UI starts again, after closing.
#[derive(Debug, Clone, PartialEq)]
pub struct Relaunch {
    pub cwd: PathBuf,
    pub resume: Option<String>,
}

/// Whether two folders are the same one.
pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The menu's row for a stored session.
fn row(info: codesmith_core::store::Info) -> SessionRow {
    let dir = PathBuf::from(&info.directory);
    SessionRow {
        root: browser::project_of(&dir),
        id: info.id,
        title: info.title,
        dir,
        updated: info.updated,
        parent: info.parent_id,
    }
}

/// The recent top-level sessions that are not archived (at most
/// [`browser::LOAD_LIMIT`]), and the `include` ones.
pub fn recent(db: &Store, include: &[String]) -> Result<Vec<SessionRow>, String> {
    let list = db.list(None, false).map_err(|e| format!("{e:#}"))?;
    let mut out: Vec<SessionRow> = Vec::new();
    let mut roots: std::collections::HashMap<String, Option<PathBuf>> = Default::default();
    for info in list {
        if out.len() >= browser::LOAD_LIMIT {
            break;
        }
        if db.archived(&info.id).ok().flatten().is_some() {
            continue;
        }
        let root = roots
            .entry(info.directory.clone())
            .or_insert_with(|| browser::project_of(Path::new(&info.directory)))
            .clone();
        let mut r = row(info);
        r.root = root;
        out.push(r);
    }
    for id in include {
        if out.iter().any(|r| &r.id == id) {
            continue;
        }
        if let Ok(Some(info)) = db.get(id) {
            out.push(row(info));
        }
    }
    Ok(out)
}

/// Answer one request (in the background: these read the store and run
/// Git).
pub fn answer(req: Request, db: Option<&Store>) -> Response {
    let no_store = || "the session store is unavailable".to_string();
    match req {
        Request::Recent { include } => {
            Response::Recent(db.ok_or_else(no_store).and_then(|db| recent(db, &include)))
        }
        Request::Lookup(id) => {
            let found = db.and_then(|db| db.get(&id).ok().flatten()).map(row);
            Response::Lookup(id, found)
        }
        Request::Worktrees(root) => {
            let list = codesmith_core::worktree::list(&root)
                .map(|l| {
                    l.into_iter()
                        .map(|e| e.directory.canonicalize().unwrap_or(e.directory))
                        .collect()
                })
                .map_err(|e| format!("{e:#}"));
            Response::Worktrees(root, list)
        }
        Request::Create { root, name } => {
            let cfg = worktree_config(&root);
            let opts = codesmith_core::worktree::Create {
                name,
                ..Default::default()
            };
            let made = codesmith_core::worktree::create(&cfg, &root, &opts)
                .map(|(dir, _)| dir)
                .map_err(|e| format!("{e:#}"));
            Response::Created(root, made)
        }
        Request::Remove { root, dir } => {
            let cfg = worktree_config(&root);
            let done =
                codesmith_core::worktree::remove(&cfg, &root, &dir.display().to_string(), false)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"));
            Response::Removed(dir, done)
        }
    }
}

/// The project's `worktree` settings (where new ones go, their start
/// command).
fn worktree_config(root: &Path) -> codesmith_core::worktree::WorktreeConfig {
    config::load(root)
        .map(|l| l.config.worktree)
        .unwrap_or_default()
}

/// The toast title when a session could not be moved (OpenCode's).
pub const MOVE_FAILED: &str = "Failed to move session";

/// Move session `id` (and its subagents' sessions) to the folder `dir`,
/// which must exist; the folder it moved to.
pub fn move_to(db: &Store, id: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = dir.canonicalize()?;
    if !db.set_directory(id, &dir.display().to_string())? {
        anyhow::bail!("no session {id}");
    }
    Ok(dir)
}

/// What opening a target from a UI in `cwd` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Continue the session here (a tab of its own with tabs on).
    Session(String),
    /// A new session here.
    New,
    /// Start again in the target's folder.
    Relaunch(Relaunch),
    /// The session's folder is gone: choose another for it.
    Recover(String, PathBuf),
}

pub fn plan(t: &Target, cwd: &Path) -> Plan {
    match t {
        Target::Session { id, dir } if same_dir(dir, cwd) => Plan::Session(id.clone()),
        Target::Session { id, dir } if !dir.is_dir() => Plan::Recover(id.clone(), dir.clone()),
        Target::Session { id, dir } => Plan::Relaunch(Relaunch {
            cwd: dir.clone(),
            resume: Some(id.clone()),
        }),
        Target::Directory(dir) if same_dir(dir, cwd) => Plan::New,
        Target::Directory(dir) => Plan::Relaunch(Relaunch {
            cwd: dir.clone(),
            resume: None,
        }),
    }
}

impl Runner {
    /// What the dialogs need to know of the UI.
    fn browse_context(&self) -> Context {
        let stored = open_store()
            .and_then(|db| db.get(&self.session_id).ok().flatten())
            .is_some();
        let mut ctx = Context {
            cwd: self.cwd.clone(),
            project: browser::project_of(&self.cwd),
            session: stored.then(|| self.session_id.clone()),
            home: std::env::var_os("HOME").map(PathBuf::from),
            ..Default::default()
        };
        for (id, attention, busy) in self.tab_states() {
            if self.tabs.on() {
                ctx.tabs.insert(id.clone());
            }
            if let Some(a) = attention {
                ctx.attention.insert(id.clone(), a);
            }
            if busy {
                ctx.running.insert(id);
            }
        }
        ctx
    }

    /// `/open` (`open.menu`): recent sessions and projects.
    pub(super) fn open_browser(&mut self) {
        let known = self.app.browser_known.clone();
        self.app.browser = Some(Box::new(Browser::open(self.browse_context(), known)));
    }

    /// `/worktrees` (`session.move`): the project's worktrees.
    pub(super) fn open_worktrees(&mut self) {
        match Browser::worktrees(self.browse_context()) {
            Some(b) => self.app.browser = Some(Box::new(b)),
            None => self.toast_error(None, browser::NO_PROJECT),
        }
    }

    fn toast_error(&mut self, title: Option<&str>, message: &str) {
        let mut t = super::toast::Toast::new(super::toast::Variant::Error, message);
        t.title = title.map(str::to_string);
        self.app.toast(t);
    }

    /// Run what the dialogs asked for in the background.
    pub(super) fn pump_browser(&mut self) {
        for req in self.app.take_browse_requests() {
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let db = open_store();
                let r = answer(req, db.as_ref());
                let _ = tx.send(Ev::Browse(Box::new(r)));
            });
        }
    }

    /// Open what the menu chose: here, or by restarting in its folder.
    pub(super) fn open_target(&mut self, t: Target) -> anyhow::Result<()> {
        match plan(&t, &self.cwd) {
            Plan::Session(id) => self.picked(Pick::Session, id),
            Plan::New => {
                self.new_here = true;
                let r = self.slash("new", "");
                self.new_here = false;
                r
            }
            Plan::Relaunch(r) => {
                self.relaunch(r.cwd, r.resume);
                Ok(())
            }
            Plan::Recover(id, dir) => {
                let ctx = self.browse_context();
                match Browser::recover(ctx, id, dir) {
                    Some(b) => self.app.browser = Some(Box::new(b)),
                    None => self.toast_error(
                        Some("Session location unavailable"),
                        "Open a Git project to choose another directory for it.",
                    ),
                }
                Ok(())
            }
        }
    }

    /// Move the session to `dir` and continue it there.
    pub(super) fn move_session(&mut self, session: String, dir: PathBuf) {
        if self.any_turn_running() {
            self.app
                .notice("A turn is still running. Press Esc to interrupt it first.");
            return;
        }
        let moved = (|| -> anyhow::Result<PathBuf> {
            // The folder's configuration must load for the session to run
            // there.
            config::load(&dir)?;
            let db =
                open_store().ok_or_else(|| anyhow::anyhow!("the session store is unavailable"))?;
            move_to(&db, &session, &dir)
        })();
        match moved {
            Err(e) => self.toast_error(Some(MOVE_FAILED), &format!("{e:#}")),
            Ok(dir) if same_dir(&dir, &self.cwd) => {
                let _ = self.picked(Pick::Session, session);
            }
            Ok(dir) => self.relaunch(dir, Some(session)),
        }
    }

    fn any_turn_running(&self) -> bool {
        self.tab_states().iter().any(|(_, _, busy)| *busy) || self.app.busy
    }

    /// Close and start again in `dir` (continuing `resume`).
    pub(super) fn relaunch(&mut self, dir: PathBuf, resume: Option<String>) {
        if self.any_turn_running() {
            self.app.notice(
                "A turn is still running. Interrupt it (Esc) before opening another folder.",
            );
            return;
        }
        let checked = dir
            .canonicalize()
            .map_err(anyhow::Error::from)
            .and_then(|d| config::load(&d).map(|_| d));
        match checked {
            Ok(cwd) => {
                self.relaunch = Some(Relaunch { cwd, resume });
                self.app.quit = true;
            }
            Err(e) => self.toast_error(
                Some("Could not open the folder"),
                &format!("{}: {e:#}", dir.display()),
            ),
        }
    }
}
