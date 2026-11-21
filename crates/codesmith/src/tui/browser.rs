//! The open menu and the worktrees dialog, as OpenCode's `DialogOpen`
//! (`open.menu`, `/open`) and `DialogWorkspaces` (`session.move`,
//! `/worktrees`).
//!
//! The open menu lists recent sessions from every folder and the projects
//! they belong to: a project is a Git repository's main worktree (found from
//! the session's folder on disk, no Git run), and the UI's own folder is
//! always one. → on a Git project lists its worktrees, where ctrl+n makes a
//! new one. The worktrees dialog lists the current project's worktrees to
//! open one, move the session there, make a new one, remove one or
//! refresh the list; it also recovers a session whose folder is gone.
//!
//! The UI runs in one folder at a time: opening a session or a folder
//! elsewhere restarts it there (the runner's part); here is only the
//! dialog's state, what it asks the runner to load ([`Request`]) and the
//! answers ([`Response`]).

use super::keymap::Keymap;
use super::tabs::Attention;
use super::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

/// Recent sessions shown with an empty filter (more when that many wait
/// for their user).
pub const RECENT_LIMIT: usize = 8;
/// The error when the worktrees dialog opens outside Git (OpenCode's).
pub const NO_PROJECT: &str = "Unable to determine current project";
/// Sessions read from the store for the menu.
pub const LOAD_LIMIT: usize = 50;

/// A session the menu can open.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub title: Option<String>,
    pub dir: PathBuf,
    /// The Git project's main worktree, when the folder is in one.
    pub root: Option<PathBuf>,
    /// Last activity (ms).
    pub updated: i64,
    pub parent: Option<String>,
}

/// What the dialog opens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Session {
        id: String,
        dir: PathBuf,
    },
    /// A folder: a new session there (OpenCode's Home at that location).
    Directory(PathBuf),
}

/// What the dialog needs loaded or done.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// The recent sessions, with their projects, and these sessions (the
    /// ones waiting for their user) however old.
    Recent { include: Vec<String> },
    /// A session by its exact ID, typed into the filter.
    Lookup(String),
    /// The worktrees of the project at this root.
    Worktrees(PathBuf),
    /// A new worktree of the project at `root`.
    Create { root: PathBuf, name: Option<String> },
    /// Remove the linked worktree `dir` of the project at `root`.
    Remove { root: PathBuf, dir: PathBuf },
}

/// An answer to a [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Recent(Result<Vec<SessionRow>, String>),
    Lookup(String, Option<SessionRow>),
    /// The project's worktrees, its main one first.
    Worktrees(PathBuf, Result<Vec<PathBuf>, String>),
    Created(PathBuf, Result<PathBuf, String>),
    Removed(PathBuf, Result<(), String>),
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Nothing,
    Close,
    Open(Target),
    /// Move the session to the folder (and open it there).
    Move {
        session: String,
        dir: PathBuf,
    },
    /// An error toast: title and message.
    Failed(String, String),
}

/// What the UI knows when the dialog opens.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// The UI's folder.
    pub cwd: PathBuf,
    /// The Git project the UI's folder is in (its main worktree, from
    /// [`project_of`]), if any.
    pub project: Option<PathBuf>,
    /// The current session, when it is stored (none on a new session:
    /// OpenCode's Home).
    pub session: Option<String>,
    /// Sessions open in tabs.
    pub tabs: HashSet<String>,
    /// Sessions waiting for their user (an open tab's permission or
    /// question).
    pub attention: HashMap<String, Attention>,
    /// Sessions with a running turn.
    pub running: HashSet<String>,
    /// For `~` in paths.
    pub home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// The open menu.
    Open,
    /// The worktrees dialog.
    Worktrees,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Load {
    Loading,
    Done,
    Failed,
}

/// The project list's place, to come back to from a project's worktrees.
#[derive(Debug, Clone)]
struct Place {
    filter: String,
    sel: Option<Target>,
}

/// The new worktree's name being typed.
#[derive(Debug, Clone, Default)]
struct Naming {
    text: String,
    busy: bool,
    back: Option<Place>,
}

/// A row's mark before its title.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gutter {
    None,
    /// The UI's folder (or project).
    Current,
    Permission,
    Question,
    Running,
    /// Open in a tab.
    Tab,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub section: &'static str,
    pub title: String,
    pub footer: String,
    pub gutter: Gutter,
    pub target: Target,
    search: String,
}

/// Rows laid out: the width, the minute and the rows.
type LaidOut = (usize, i64, std::sync::Arc<Vec<Row>>);

pub struct Browser {
    pub mode: Mode,
    ctx: Context,
    /// The project the UI's folder is in (its main worktree), or the
    /// folder itself outside Git.
    current: PathBuf,
    current_git: bool,
    sessions: Vec<SessionRow>,
    recent: Load,
    /// A session found by the ID typed.
    matched: Option<SessionRow>,
    looking: Option<String>,
    /// The project whose worktrees are listed.
    project: Option<PathBuf>,
    /// The project list to go back to.
    back: Option<Place>,
    worktrees: HashMap<PathBuf, Vec<PathBuf>>,
    wt_loading: Option<PathBuf>,
    wt_error: Option<String>,
    pub filter: String,
    sel: Option<Target>,
    naming: Option<Naming>,
    /// A worktree to remove once its key is pressed again.
    confirm: Option<PathBuf>,
    removing: Option<PathBuf>,
    /// A session whose folder is gone, moved to the folder chosen.
    recover: Option<(String, PathBuf)>,
    requests: Vec<Request>,
    /// The rows last laid out, for the width and minute they were laid out
    /// at: frames redraw them without filtering again. Any change clears
    /// it.
    laid_out: std::sync::Mutex<Option<LaidOut>>,
}

/// `s` on one line without control or invisible characters (titles are
/// the model's text).
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|&c| !(c.is_control() || super::view::invisible(c)))
        .collect()
}

/// The Git project `dir` is in: its main worktree, found on disk (a linked
/// worktree's `.git` file names its main repository's). Only a repository
/// this user controls counts (as for the project root): a `.git` another
/// user planted in a shared ancestor makes no project.
pub fn project_of(dir: &Path) -> Option<PathBuf> {
    let trusted = codesmith_core::tool::trusted_repository;
    for d in dir.ancestors() {
        let git = d.join(".git");
        if (git.is_dir() || git.is_file()) && !trusted(d) {
            return None;
        }
        if git.is_dir() {
            return Some(d.to_path_buf());
        }
        if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                d.join(gitdir)
            };
            let common = match std::fs::read_to_string(gitdir.join("commondir")) {
                Ok(c) => {
                    let c = PathBuf::from(c.trim());
                    if c.is_absolute() {
                        c
                    } else {
                        gitdir.join(c)
                    }
                }
                Err(_) => gitdir,
            };
            let common = common.canonicalize().unwrap_or(common);
            // A bare repository's worktrees have no main checkout.
            return match common.file_name().and_then(|n| n.to_str()) {
                Some(".git") => common
                    .parent()
                    .filter(|m| trusted(m))
                    .map(Path::to_path_buf),
                _ => Some(d.to_path_buf()),
            };
        }
    }
    None
}

/// `path` with the home folder as `~`.
pub fn abbreviate(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `s` cut from the left to `width` columns (`…/end`).
fn truncate_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: Vec<char> = Vec::new();
    let mut w = 1;
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out.into_iter().rev().collect()
}

/// `s` cut to `width` columns (`start…`).
fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

fn base(p: &Path) -> String {
    p.file_name()
        .map(|n| clean(&n.to_string_lossy()))
        .unwrap_or_else(|| p.display().to_string())
}

/// OpenCode's `timeAgo`: `now`, `5m`, `3h`, `2d`, `4mo`, `1y`.
pub fn time_ago(ms: i64, now: i64) -> String {
    let minutes = (now - ms).max(0) / 60_000;
    if minutes < 1 {
        return "now".into();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo");
    }
    format!("{}y", days / 365)
}

/// Whether `s` is a whole session ID: OpenCode's (`ses_` and 26 letters
/// or digits, as imported sessions keep) or CodeSmith's (`ses_` and 17 hex
/// digits: the time in milliseconds and 24 random bits).
fn session_id_like(s: &str) -> bool {
    s.strip_prefix("ses_").is_some_and(|rest| {
        (rest.len() == 26 && rest.chars().all(|c| c.is_ascii_alphanumeric()))
            || (rest.len() == 17 && rest.chars().all(|c| c.is_ascii_hexdigit()))
    })
}

impl Browser {
    /// The open menu. `known` are the sessions the UI already has (the
    /// last list, open tabs), shown at once while the list refreshes.
    pub fn open(ctx: Context, known: Vec<SessionRow>) -> Browser {
        let mut b = Browser::new(Mode::Open, ctx);
        b.sessions = known;
        let mut include: Vec<String> = b.ctx.attention.keys().cloned().collect();
        include.sort();
        b.requests.push(Request::Recent { include });
        b
    }

    /// The worktrees dialog of the UI's project. `None` outside Git
    /// (OpenCode: "Unable to determine current project").
    pub fn worktrees(ctx: Context) -> Option<Browser> {
        let mut b = Browser::new(Mode::Worktrees, ctx);
        if !b.current_git {
            return None;
        }
        b.show_worktrees(b.current.clone());
        Some(b)
    }

    /// The worktrees dialog for a session whose folder `dir` is gone:
    /// choosing a folder moves the session there.
    pub fn recover(ctx: Context, session: String, dir: PathBuf) -> Option<Browser> {
        let mut b = Browser::worktrees(ctx)?;
        b.recover = Some((session, dir));
        Some(b)
    }

    fn new(mode: Mode, ctx: Context) -> Browser {
        let git = ctx.project.clone();
        Browser {
            mode,
            current: git.clone().unwrap_or_else(|| ctx.cwd.clone()),
            current_git: git.is_some(),
            ctx,
            sessions: Vec::new(),
            recent: Load::Loading,
            matched: None,
            looking: None,
            project: None,
            back: None,
            worktrees: HashMap::new(),
            wt_loading: None,
            wt_error: None,
            filter: String::new(),
            sel: None,
            naming: None,
            confirm: None,
            removing: None,
            recover: None,
            requests: Vec::new(),
            laid_out: Default::default(),
        }
    }

    /// What the runner should load or do now.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// The sessions listed, for the next time the menu opens.
    pub fn known(&self) -> Vec<SessionRow> {
        self.sessions.clone()
    }

    pub fn answer(&mut self, r: Response) -> Outcome {
        self.changed();
        match r {
            Response::Recent(Ok(list)) => {
                // A session only the UI knew about (a tab's, not stored
                // yet) stays.
                let mut list = list;
                for s in &self.sessions {
                    if !list.iter().any(|l| l.id == s.id) && self.ctx.tabs.contains(&s.id) {
                        list.push(s.clone());
                    }
                }
                self.sessions = list;
                self.recent = Load::Done;
            }
            Response::Recent(Err(_)) => self.recent = Load::Failed,
            Response::Lookup(id, found) => {
                if self.looking.as_deref() == Some(id.as_str()) {
                    self.looking = None;
                    self.matched = found;
                }
            }
            Response::Worktrees(root, result) => {
                if self.wt_loading.as_deref() == Some(root.as_path()) {
                    self.wt_loading = None;
                }
                match result {
                    Ok(list) => {
                        self.wt_error = None;
                        self.worktrees.insert(root, list);
                    }
                    Err(e) => {
                        if self.project.as_deref() == Some(root.as_path()) {
                            return Outcome::Failed("Loading worktrees failed".into(), e);
                        }
                    }
                }
            }
            Response::Created(root, result) => {
                let Some(n) = &mut self.naming else {
                    return Outcome::Nothing;
                };
                n.busy = false;
                return match result {
                    Ok(dir) => {
                        let _ = root;
                        match self.recover.clone() {
                            Some((session, _)) => Outcome::Move { session, dir },
                            None => Outcome::Open(Target::Directory(dir)),
                        }
                    }
                    // OpenCode's two dialogs word it differently.
                    Err(e) => Outcome::Failed(
                        match self.mode {
                            Mode::Open => "Creating worktree failed",
                            Mode::Worktrees => "Creating workspace failed",
                        }
                        .into(),
                        e,
                    ),
                };
            }
            Response::Removed(dir, result) => {
                if self.removing.as_ref() == Some(&dir) {
                    self.removing = None;
                }
                match result {
                    Ok(()) => {
                        if let Some(root) = self.project.clone() {
                            if let Some(l) = self.worktrees.get_mut(&root) {
                                l.retain(|d| *d != dir);
                            }
                            self.load_worktrees(root);
                        }
                    }
                    Err(e) => return Outcome::Failed("Failed to delete worktree".into(), e),
                }
            }
        }
        Outcome::Nothing
    }

    fn show_worktrees(&mut self, root: PathBuf) {
        self.project = Some(root.clone());
        self.filter.clear();
        self.sel = Some(Target::Directory(root.clone()));
        self.confirm = None;
        self.load_worktrees(root);
    }

    fn load_worktrees(&mut self, root: PathBuf) {
        self.wt_loading = Some(root.clone());
        self.requests.push(Request::Worktrees(root));
    }

    fn back_to_projects(&mut self) {
        let Some(place) = self.back.take() else {
            return;
        };
        self.project = None;
        self.filter = place.filter;
        self.sel = place.sel;
        self.confirm = None;
    }

    fn set_filter(&mut self, f: String) {
        self.filter = f;
        self.sel = None;
        self.confirm = None;
        let id = self.filter.trim().to_string();
        if self.mode == Mode::Open && self.project.is_none() && session_id_like(&id) {
            if self.matched.as_ref().map(|m| &m.id) != Some(&id) {
                self.matched = None;
                self.looking = Some(id.clone());
                self.requests.push(Request::Lookup(id));
            }
        } else {
            self.looking = None;
        }
    }

    /// Whether the list is still being read (its footer says so).
    pub fn loading(&self) -> bool {
        match &self.project {
            Some(root) => self.wt_loading.as_ref() == Some(root),
            None => self.recent == Load::Loading,
        }
    }

    // ----- rows ------------------------------------------------------

    fn project_name(&self, root: &Path) -> String {
        base(root)
    }

    /// The sessions the menu lists, attention first, then newest.
    fn session_pool(&self) -> Vec<&SessionRow> {
        let mut seen = HashSet::new();
        let matched = self.matched.as_ref();
        let mut list: Vec<&SessionRow> = self
            .sessions
            .iter()
            .chain(matched)
            .filter(|s| {
                let is_match = matched.is_some_and(|m| m.id == s.id);
                (s.parent.is_none() || is_match) && seen.insert(s.id.clone())
            })
            .collect();
        list.sort_by(|a, b| {
            let (x, y) = (self.attention(&a.id), self.attention(&b.id));
            let rank = |at: Option<Attention>| match at {
                Some(Attention::Question) => 0,
                Some(Attention::Permission) => 1,
                None => 2,
            };
            rank(x)
                .cmp(&rank(y))
                .then_with(|| b.updated.cmp(&a.updated))
        });
        list
    }

    fn attention(&self, id: &str) -> Option<Attention> {
        self.ctx.attention.get(id).copied()
    }

    fn session_rows(&self, now: i64) -> Vec<Row> {
        let pool = self.session_pool();
        let current = self.ctx.session.as_deref();
        let shown: Vec<&SessionRow> = if self.filter.trim().is_empty() {
            let candidates: Vec<&SessionRow> = pool
                .into_iter()
                .filter(|s| {
                    !self.ctx.tabs.contains(&s.id)
                        || (self.attention(&s.id).is_some() && Some(s.id.as_str()) != current)
                })
                .collect();
            let waiting = candidates
                .iter()
                .filter(|s| self.attention(&s.id).is_some())
                .count();
            candidates
                .into_iter()
                .take(RECENT_LIMIT.max(waiting))
                .collect()
        } else {
            pool
        };
        shown
            .into_iter()
            .map(|s| {
                let dir_name = base(&s.dir);
                let label = match &s.root {
                    Some(root) => {
                        let name = self.project_name(root);
                        if s.dir != *root && name.to_lowercase() != dir_name.to_lowercase() {
                            format!("{name} · {dir_name}")
                        } else {
                            name
                        }
                    }
                    None => dir_name,
                };
                let label = truncate(&label, 30);
                let gutter = match self.attention(&s.id) {
                    Some(Attention::Permission) => Gutter::Permission,
                    Some(Attention::Question) => Gutter::Question,
                    None if self.ctx.running.contains(&s.id) => Gutter::Running,
                    None if self.ctx.tabs.contains(&s.id) => Gutter::Tab,
                    None => Gutter::None,
                };
                let title = s
                    .title
                    .as_deref()
                    .map(clean)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| super::tabs::NEW_SESSION_TITLE.to_string());
                Row {
                    section: "Sessions",
                    search: format!("{title} {} {}", s.id, s.dir.display()),
                    title,
                    footer: format!("{label} · {}", time_ago(s.updated, now)),
                    gutter,
                    target: Target::Session {
                        id: s.id.clone(),
                        dir: s.dir.clone(),
                    },
                }
            })
            .collect()
    }

    /// The projects: those of the sessions, newest first, and the UI's.
    fn projects(&self) -> Vec<(PathBuf, bool)> {
        let mut by_time: Vec<(&PathBuf, i64)> = Vec::new();
        for s in &self.sessions {
            let Some(root) = &s.root else { continue };
            match by_time.iter_mut().find(|(r, _)| *r == root) {
                Some((_, t)) => *t = (*t).max(s.updated),
                None => by_time.push((root, s.updated)),
            }
        }
        by_time.sort_by_key(|t| std::cmp::Reverse(t.1));
        let mut out: Vec<(PathBuf, bool)> = by_time
            .into_iter()
            .map(|(r, _)| (r.clone(), true))
            .collect();
        if !out.iter().any(|(r, _)| *r == self.current) {
            out.push((self.current.clone(), self.current_git));
        }
        out
    }

    fn project_rows(&self, width: usize) -> Vec<Row> {
        self.projects()
            .into_iter()
            .map(|(root, git)| {
                let title = self.project_name(&root);
                let path = clean(&abbreviate(&root, self.ctx.home.as_deref()));
                let room = width
                    .saturating_sub(title.width() + 6)
                    .saturating_sub(if git { 2 } else { 0 });
                Row {
                    section: "Projects",
                    search: format!("{path} {title}"),
                    footer: format!(
                        "{}{}",
                        truncate_left(&path, room),
                        if git { " →" } else { "" }
                    ),
                    gutter: if root == self.current {
                        Gutter::Current
                    } else {
                        Gutter::None
                    },
                    title,
                    target: Target::Directory(root),
                }
            })
            .collect()
    }

    /// The worktree folders of the project at `root`: its main one, the
    /// UI's, then the others.
    fn worktree_dirs(&self, root: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![root.to_path_buf()];
        let loading = self.wt_loading.as_deref() == Some(root);
        if !(loading && self.mode == Mode::Open) {
            for d in self.worktrees.get(root).into_iter().flatten() {
                if !dirs.contains(d) {
                    dirs.push(d.clone());
                }
            }
        }
        let cwd = &self.ctx.cwd;
        let here = |d: &PathBuf| cwd.starts_with(d);
        dirs.sort_by_key(|d| {
            if self.mode == Mode::Worktrees {
                // The worktrees dialog: the UI's first, then the main one.
                if here(d) {
                    0
                } else if d == root {
                    1
                } else {
                    2
                }
            } else if d == root {
                0
            } else if here(d) {
                1
            } else {
                2
            }
        });
        dirs
    }

    fn worktree_rows(&self, root: &Path, width: usize) -> Vec<Row> {
        let dirs = self.worktree_dirs(root);
        if self.mode == Mode::Worktrees {
            return dirs
                .into_iter()
                .map(|d| {
                    let title = clean(&abbreviate(&d, self.ctx.home.as_deref()));
                    let here = self.ctx.cwd.starts_with(&d);
                    Row {
                        section: if here { "Current" } else { "Other" },
                        search: title.clone(),
                        title: truncate_left(&title, width.saturating_sub(4)),
                        footer: String::new(),
                        gutter: Gutter::None,
                        target: Target::Directory(d),
                    }
                })
                .collect();
        }
        let titles: Vec<String> = dirs
            .iter()
            .map(|d| {
                if d == root {
                    self.project_name(root)
                } else {
                    base(d)
                }
            })
            .collect();
        let widest = titles.iter().map(|t| t.width()).max().unwrap_or(0);
        dirs.into_iter()
            .zip(titles)
            .map(|(d, title)| {
                let path = clean(&abbreviate(&d, self.ctx.home.as_deref()));
                Row {
                    section: "",
                    search: format!("{title} {path}"),
                    footer: truncate_left(&path, width.saturating_sub(widest + 6)),
                    gutter: if self.ctx.cwd == d {
                        Gutter::Current
                    } else {
                        Gutter::None
                    },
                    title,
                    target: Target::Directory(d),
                }
            })
            .collect()
    }

    /// The rows shown, filtered, section by section.
    pub fn rows(&self, width: usize, now: i64) -> std::sync::Arc<Vec<Row>> {
        let minute = now / 60_000;
        let mut cache = self.laid_out.lock().unwrap();
        if let Some((w, m, rows)) = cache.as_ref() {
            if *w == width && *m == minute {
                return rows.clone();
            }
        }
        let rows = std::sync::Arc::new(self.lay_out(width, now));
        *cache = Some((width, minute, rows.clone()));
        rows
    }

    /// A change: the rows are laid out again.
    fn changed(&mut self) {
        *self.laid_out.get_mut().unwrap() = None;
    }

    fn lay_out(&self, width: usize, now: i64) -> Vec<Row> {
        let sections: Vec<Vec<Row>> = match &self.project {
            Some(root) => {
                let rows = self.worktree_rows(root, width);
                if self.mode == Mode::Worktrees {
                    let (cur, other): (Vec<Row>, Vec<Row>) =
                        rows.into_iter().partition(|r| r.section == "Current");
                    vec![cur, other]
                } else {
                    vec![rows]
                }
            }
            None => vec![self.session_rows(now), self.project_rows(width)],
        };
        let q = self.filter.trim();
        sections
            .into_iter()
            .flat_map(|rows| {
                if q.is_empty() {
                    return rows;
                }
                let keep =
                    super::complete::filter(&rows, q, |r| r.search.as_str(), false, usize::MAX);
                keep.into_iter().map(|i| rows[i].clone()).collect()
            })
            .collect()
    }

    fn sel_index(&self, rows: &[Row]) -> usize {
        self.sel
            .as_ref()
            .and_then(|t| rows.iter().position(|r| &r.target == t))
            .unwrap_or(0)
    }

    /// The row selected, among `rows`.
    #[cfg(test)]
    pub fn selected<'a>(&self, rows: &'a [Row]) -> Option<&'a Row> {
        rows.get(self.sel_index(rows))
    }

    // ----- keys ------------------------------------------------------

    pub fn key(&mut self, k: &KeyEvent, km: &Keymap) -> Outcome {
        let out = self.key_inner(k, km);
        self.changed();
        out
    }

    fn key_inner(&mut self, k: &KeyEvent, km: &Keymap) -> Outcome {
        let st = super::keymap::stroke(k);
        let hit = |id: &'static str| {
            st.as_ref()
                .is_some_and(|s| !km.lookup(&[id], std::slice::from_ref(s)).exact.is_empty())
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let typed = match k.code {
            KeyCode::Char(c) if !ctrl && !alt => Some(c),
            _ => None,
        };
        if let Some(n) = &mut self.naming {
            if n.busy {
                return Outcome::Nothing;
            }
            match k.code {
                KeyCode::Esc => {
                    let back = self.naming.take().and_then(|n| n.back);
                    if let Some(p) = back {
                        self.filter = p.filter;
                        self.sel = p.sel;
                    }
                }
                KeyCode::Enter => {
                    let name = n.text.trim().to_string();
                    n.busy = true;
                    if let Some(root) = self.project.clone() {
                        self.requests.push(Request::Create {
                            root,
                            name: (!name.is_empty()).then_some(name),
                        });
                    }
                }
                KeyCode::Backspace => {
                    n.text.pop();
                }
                _ => {
                    if let Some(c) = typed {
                        n.text.push(c);
                    }
                }
            }
            return Outcome::Nothing;
        }
        let rows = self.rows(usize::MAX / 4, codesmith_core::store::now());
        let at = self.sel_index(&rows);
        let in_worktrees = self.project.is_some();
        // The dialog's own bindings come before the list's (ctrl+n).
        if self.mode == Mode::Open && in_worktrees {
            if ctrl && k.code == KeyCode::Char('n') {
                self.start_naming();
                return Outcome::Nothing;
            }
            if k.code == KeyCode::Left && k.modifiers.is_empty() {
                self.back_to_projects();
                return Outcome::Nothing;
            }
        }
        if self.mode == Mode::Open
            && !in_worktrees
            && k.code == KeyCode::Right
            && k.modifiers.is_empty()
        {
            if let Some(Row {
                target: Target::Directory(root),
                section: "Projects",
                ..
            }) = rows.get(at)
            {
                if self.projects().iter().any(|(r, git)| r == root && *git) {
                    let root = root.clone();
                    self.back = Some(Place {
                        filter: self.filter.clone(),
                        sel: self.sel.clone(),
                    });
                    self.show_worktrees(root);
                }
            }
            return Outcome::Nothing;
        }
        if self.mode == Mode::Worktrees {
            if self.removing.is_some() {
                return Outcome::Nothing;
            }
            if hit("dialog.move_session.new") {
                self.start_naming();
                return Outcome::Nothing;
            }
            if hit("dialog.move_session.refresh") {
                if let Some(root) = self.project.clone() {
                    self.load_worktrees(root);
                }
                return Outcome::Nothing;
            }
            if hit("dialog.move_session.delete") {
                let Some(Target::Directory(d)) = rows.get(at).map(|r| r.target.clone()) else {
                    return Outcome::Nothing;
                };
                if Some(&d) == self.project.as_ref() {
                    return Outcome::Nothing;
                }
                if self.ctx.cwd.starts_with(&d) {
                    return Outcome::Failed(
                        "Failed to delete worktree".into(),
                        "the UI is working in it; open another folder first".into(),
                    );
                }
                if self.confirm.as_ref() != Some(&d) {
                    self.confirm = Some(d);
                    return Outcome::Nothing;
                }
                self.confirm = None;
                self.removing = Some(d.clone());
                if let Some(root) = self.project.clone() {
                    self.requests.push(Request::Remove { root, dir: d });
                }
                return Outcome::Nothing;
            }
            if hit("dialog.move_session.move") {
                let Some(session) = self.ctx.session.clone().filter(|_| self.recover.is_none())
                else {
                    return Outcome::Nothing;
                };
                return match rows.get(at).map(|r| r.target.clone()) {
                    Some(Target::Directory(dir)) => Outcome::Move { session, dir },
                    _ => Outcome::Nothing,
                };
            }
        }
        // ⌥↑ and ⌥↓ jump between sections.
        if alt && matches!(k.code, KeyCode::Up | KeyCode::Down) {
            self.jump_section(&rows, at, k.code == KeyCode::Down);
            return Outcome::Nothing;
        }
        let last = rows.len().saturating_sub(1);
        let mv = |to: usize| rows.get(to).map(|r| r.target.clone());
        let before = self.sel.clone();
        if hit("dialog.select.prev") {
            self.sel = mv(at.saturating_sub(1));
        } else if hit("dialog.select.next") {
            self.sel = mv((at + 1).min(last));
        } else if hit("dialog.select.page_up") {
            self.sel = mv(at.saturating_sub(10));
        } else if hit("dialog.select.page_down") {
            self.sel = mv((at + 10).min(last));
        } else if hit("dialog.select.home") {
            self.sel = mv(0);
        } else if hit("dialog.select.end") {
            self.sel = mv(last);
        } else if hit("dialog.select.submit") {
            let Some(row) = rows.get(at) else {
                return Outcome::Nothing;
            };
            return match (&self.recover, &row.target) {
                (Some((session, _)), Target::Directory(dir)) => Outcome::Move {
                    session: session.clone(),
                    dir: dir.clone(),
                },
                _ => Outcome::Open(row.target.clone()),
            };
        } else {
            match k.code {
                KeyCode::Esc if self.mode == Mode::Open && in_worktrees => {
                    self.back_to_projects();
                }
                KeyCode::Esc => return Outcome::Close,
                KeyCode::Backspace => {
                    let mut f = self.filter.clone();
                    f.pop();
                    self.set_filter(f);
                }
                _ => {
                    if let Some(c) = typed {
                        let f = format!("{}{c}", self.filter);
                        self.set_filter(f);
                    }
                }
            }
            return Outcome::Nothing;
        }
        if self.sel != before {
            self.confirm = None;
        }
        Outcome::Nothing
    }

    /// Ctrl+C: clear the filter (or the name), else close.
    pub fn interrupt(&mut self) -> Outcome {
        self.changed();
        if let Some(n) = &mut self.naming {
            if !n.text.is_empty() {
                n.text.clear();
                return Outcome::Nothing;
            }
        }
        if !self.filter.is_empty() {
            self.set_filter(String::new());
            return Outcome::Nothing;
        }
        Outcome::Close
    }

    pub fn paste(&mut self, s: &str) {
        self.changed();
        let line = clean(s.lines().next().unwrap_or(""));
        match &mut self.naming {
            Some(n) if !n.busy => n.text.push_str(&line),
            Some(_) => {}
            None => {
                let f = format!("{}{line}", self.filter);
                self.set_filter(f);
            }
        }
    }

    fn start_naming(&mut self) {
        if self.project.is_none() {
            return;
        }
        self.naming = Some(Naming {
            back: Some(Place {
                filter: self.filter.clone(),
                sel: self.sel.clone(),
            }),
            ..Default::default()
        });
    }

    fn jump_section(&mut self, rows: &[Row], at: usize, down: bool) {
        let mut starts: Vec<usize> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            if i == 0 || rows[i - 1].section != r.section {
                starts.push(i);
            }
        }
        if starts.len() < 2 {
            return;
        }
        let cur = starts.iter().rposition(|&s| s <= at).unwrap_or(0);
        let next = if down {
            (cur + 1) % starts.len()
        } else {
            (cur + starts.len() - 1) % starts.len()
        };
        self.sel = Some(rows[starts[next]].target.clone());
    }

    // ----- drawing ---------------------------------------------------

    pub fn title(&self) -> String {
        if let Some(n) = &self.naming {
            let _ = n;
            return match self.mode {
                Mode::Open => format!(
                    "{} / New worktree",
                    self.project
                        .as_deref()
                        .map(|p| self.project_name(p))
                        .unwrap_or_else(|| "Project".into())
                ),
                Mode::Worktrees => "Name worktree".into(),
            };
        }
        match (&self.project, self.mode) {
            (_, Mode::Worktrees) => "Worktrees".into(),
            (Some(root), Mode::Open) => format!("{} / Worktrees", self.project_name(root)),
            (None, Mode::Open) => "Open".into(),
        }
    }

    /// The dialog's lines, `width` columns wide, the list at most `rows`
    /// rows tall.
    pub fn lines(&self, width: usize, rows: usize, km: &Keymap, now: i64) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let mut lines = Vec::new();
        if let Some(n) = &self.naming {
            lines.push(Line::from(vec![
                Span::styled("> ", fg(p.primary)),
                Span::styled(n.text.clone(), fg(p.dialog_text)),
                Span::styled(
                    if n.text.is_empty() {
                        "Worktree name (optional)"
                    } else {
                        ""
                    },
                    fg(p.dialog_muted),
                ),
            ]));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                if n.busy {
                    "Creating worktree…"
                } else {
                    "Leave blank for a random name."
                },
                fg(p.dialog_muted),
            )));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "enter create · esc back",
                fg(p.dialog_muted),
            )));
            return lines;
        }
        if let Some((_, dir)) = &self.recover {
            lines.push(Line::from(Span::styled(
                "Session location unavailable",
                fg(p.warning).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                truncate_left(
                    &clean(&abbreviate(dir, self.ctx.home.as_deref())),
                    width.saturating_sub(1),
                ),
                fg(p.dialog_muted),
            )));
            lines.push(Line::from(Span::styled(
                "Choose another directory to continue this session.",
                fg(p.dialog_text),
            )));
            lines.push(Line::default());
        }
        let placeholder = match (&self.project, self.mode) {
            (None, Mode::Open) => "Search sessions and projects…",
            _ => "Search worktrees…",
        };
        lines.push(Line::from(vec![
            Span::styled("> ", fg(p.primary)),
            Span::styled(clean(&self.filter), fg(p.dialog_text)),
            Span::styled(
                if self.filter.is_empty() {
                    placeholder
                } else {
                    ""
                },
                fg(p.dialog_muted),
            ),
        ]));
        lines.push(Line::default());
        let all = self.rows(width, now);
        if all.is_empty() {
            let text = self.empty_text(km);
            lines.push(Line::from(Span::styled(text, fg(p.dialog_muted))));
        }
        // Section headers take rows too: lay out the list, then window it.
        let at = self.sel_index(&all);
        let confirm_key = km.hint("dialog.move_session.delete");
        let mut list: Vec<(Option<usize>, Line<'static>)> = Vec::new();
        for (i, r) in all.iter().enumerate() {
            if !r.section.is_empty() && (i == 0 || all[i - 1].section != r.section) {
                if i > 0 {
                    list.push((None, Line::default()));
                }
                list.push((
                    None,
                    Line::from(Span::styled(
                        r.section,
                        fg(p.accent).add_modifier(Modifier::BOLD),
                    )),
                ));
            }
            list.push((Some(i), self.row_line(r, i == at, width, &confirm_key)));
        }
        let pos = list.iter().position(|(i, _)| *i == Some(at)).unwrap_or(0);
        let rows = rows.max(1);
        let mut top = (pos + 1).saturating_sub(rows);
        // Show the selected row's section header when there is room.
        if top > 0 && pos > 0 && list[pos - 1].0.is_none() && top == pos {
            top -= 1;
        }
        lines.extend(list.into_iter().skip(top).take(rows).map(|(_, l)| l));
        lines.push(Line::default());
        if let Some(status) = self.status() {
            let color = if status.starts_with("Could not") {
                p.error
            } else {
                p.dialog_muted
            };
            lines.push(Line::from(Span::styled(status, fg(color))));
        }
        lines.push(Line::from(Span::styled(self.hints(km), fg(p.dialog_muted))));
        lines
    }

    fn row_line(&self, r: &Row, sel: bool, width: usize, confirm_key: &str) -> Line<'static> {
        let p = theme::current();
        let base = if sel {
            Style::default().fg(p.selected_fg).bg(p.selected_bg)
        } else {
            Style::default().fg(p.dialog_text)
        };
        let confirming =
            matches!(&r.target, Target::Directory(d) if self.confirm.as_ref() == Some(d));
        let removing =
            matches!(&r.target, Target::Directory(d) if self.removing.as_ref() == Some(d));
        let (mark, mark_style) = match r.gutter {
            Gutter::None => ("  ", base),
            Gutter::Current => ("● ", if sel { base } else { base.fg(p.success) }),
            Gutter::Permission => ("! ", if sel { base } else { base.fg(p.warning) }),
            Gutter::Question => ("? ", if sel { base } else { base.fg(p.warning) }),
            Gutter::Running => ("⋯ ", if sel { base } else { base.fg(p.dialog_muted) }),
            Gutter::Tab => ("▪ ", if sel { base } else { base.fg(p.accent) }),
        };
        let title = if removing {
            format!("Deleting {}", r.title)
        } else if confirming {
            format!("Press {confirm_key} again to confirm")
        } else {
            r.title.clone()
        };
        let title_style = if removing || confirming {
            base.fg(p.error).add_modifier(Modifier::BOLD)
        } else {
            base.add_modifier(Modifier::BOLD)
        };
        let room = width.saturating_sub(2);
        let title = truncate(
            &title,
            room.saturating_sub(r.footer.width() + 2).max(room / 2),
        );
        let gap = room.saturating_sub(title.width() + r.footer.width()).max(2);
        Line::from(vec![
            Span::styled(mark, mark_style),
            Span::styled(title, title_style),
            Span::styled(" ".repeat(gap), base),
            Span::styled(
                r.footer.clone(),
                if sel {
                    base
                } else {
                    Style::default().fg(p.dialog_muted)
                },
            ),
        ])
    }

    fn empty_text(&self, km: &Keymap) -> String {
        if self.project.is_some() {
            return if self.loading() {
                "Loading worktrees…".into()
            } else if self.filter.trim().is_empty() {
                "No worktrees available".into()
            } else {
                "No matching worktrees".into()
            };
        }
        if self.recent == Load::Loading || self.looking.is_some() {
            return "Searching sessions and projects…".into();
        }
        if self.filter.trim().is_empty() {
            return "No recent sessions or projects".into();
        }
        match km.hint("session.list") {
            h if h.is_empty() => "No matches".into(),
            h => format!("No matches · search all sessions with {h}"),
        }
    }

    fn status(&self) -> Option<String> {
        if self.project.is_some() {
            return self.loading().then(|| "Loading worktrees…".into());
        }
        match self.recent {
            Load::Loading => Some("Refreshing sessions and projects…".into()),
            Load::Failed => Some("Could not refresh sessions and projects.".into()),
            Load::Done => None,
        }
    }

    fn hints(&self, km: &Keymap) -> String {
        let mut parts: Vec<String> = Vec::new();
        let add = |parts: &mut Vec<String>, k: String, what: &str| {
            if !k.is_empty() {
                parts.push(format!("{what} {k}"));
            }
        };
        match (self.mode, self.project.is_some()) {
            (Mode::Open, false) => {
                if self.projects().iter().any(|(_, git)| *git) {
                    parts.push("worktrees →".into());
                }
                parts.push("sections alt+↑/↓".into());
            }
            (Mode::Open, true) => {
                parts.push("new worktree ctrl+n".into());
                parts.push("back ←".into());
            }
            (Mode::Worktrees, _) => {
                if self.ctx.session.is_some() && self.recover.is_none() {
                    add(&mut parts, km.hint("dialog.move_session.move"), "move");
                }
                add(&mut parts, km.hint("dialog.move_session.new"), "new");
                if self.confirm.is_some() {
                    add(
                        &mut parts,
                        km.hint("dialog.move_session.delete"),
                        "again to confirm, delete",
                    );
                } else {
                    add(&mut parts, km.hint("dialog.move_session.delete"), "delete");
                }
                add(
                    &mut parts,
                    km.hint("dialog.move_session.refresh"),
                    "refresh",
                );
            }
        }
        parts.push("esc close".into());
        parts.join(" · ")
    }
}
