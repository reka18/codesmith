//! The platform backend for [`super::Watcher`], through the `notify` crate,
//! as OpenCode's `Watcher.nativeLayer` uses `fs.watch` and Parcel.
//!
//! One `notify` watcher serves every subscription of the process. It is
//! made on the first subscription and dropped with the last, so an idle
//! process holds no watch and no thread. Its events go through a bounded
//! queue to one dispatch thread; when the queue fills, events are dropped
//! and every subscription gets a [`Kind::Overflow`] instead.
//!
//! - A file or named entries: a non-recursive watch on the directory, so
//!   atomic saves (write a temp file, rename it over) are seen too. A
//!   missing directory ends the subscription quietly; any other failure is
//!   logged.
//! - A directory tree: on macOS and Windows one recursive watch, filtered
//!   here; elsewhere (inotify, which needs a watch per directory) a watch on
//!   each directory not ignored, added as directories appear, and at most
//!   half of `fs.inotify.max_user_watches` of them, leaving the rest to
//!   other tools. Ignored: [`super::IGNORED_DIRECTORIES`], the target's
//!   ignore list (paths or globs relative to the root), and the root's
//!   `.gitignore` and `.git/info/exclude`.

use super::{BoxFuture, Kind, Log, Native, Publish, Subscription, Target, TargetKind, Update};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};

/// Raw events queued between the platform's thread and dispatch.
const RAW_QUEUE: usize = 4096;

/// How a directory tree is watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One recursive watch on the root (FSEvents, Windows).
    Recursive,
    /// A watch per directory (inotify).
    PerDirectory,
}

impl Mode {
    pub fn platform() -> Mode {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            Mode::Recursive
        } else {
            Mode::PerDirectory
        }
    }
}

/// The `notify` backend.
pub struct Notify {
    shared: Arc<Shared>,
}

impl Default for Notify {
    fn default() -> Self {
        Notify::new()
    }
}

impl Notify {
    pub fn new() -> Notify {
        Notify::with(Mode::platform(), super::default_log())
    }

    /// With a tree mode (tests drive the per-directory mode on any
    /// platform) and an error sink.
    pub fn with(mode: Mode, log: Log) -> Notify {
        Notify {
            shared: Arc::new(Shared {
                mode,
                log,
                limit: dir_limit(),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Native watches held: (directories watched, subscriptions).
    pub fn counts(&self) -> (usize, usize) {
        let s = self.shared.state.lock().unwrap();
        (s.dirs.len(), s.routes.len())
    }
}

impl Native for Notify {
    fn subscribe(
        &self,
        target: Target,
        publish: Publish,
    ) -> BoxFuture<Option<Box<dyn Subscription>>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            // Walking a large tree takes a while: keep it off the runtime.
            tokio::task::spawn_blocking(move || Shared::add(&shared, target, publish))
                .await
                .ok()
                .flatten()
        })
    }
}

/// Directories the per-directory mode may watch.
fn dir_limit() -> usize {
    #[cfg(target_os = "linux")]
    {
        if let Some(n) = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
        {
            return (n / 2).max(1);
        }
    }
    100_000
}

struct Shared {
    mode: Mode,
    log: Log,
    limit: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    watcher: Option<RecommendedWatcher>,
    routes: HashMap<u64, Route>,
    /// Watched directory → (non-recursive, recursive) users.
    dirs: HashMap<PathBuf, (usize, usize)>,
    next: u64,
    /// The per-directory limit was reached (logged once).
    full: bool,
}

struct Route {
    /// The watched directory, canonical (events arrive canonical).
    root: PathBuf,
    /// The directory as the subscriber named it, for the paths published.
    shown: PathBuf,
    filter: Filter,
    publish: Publish,
}

enum Filter {
    Names(HashSet<OsString>),
    Tree {
        ignore: Arc<Ignore>,
        /// Directories this route watches (per-directory mode), or just
        /// the root (recursive mode).
        dirs: Vec<PathBuf>,
    },
}

struct Sub {
    id: u64,
    shared: Weak<Shared>,
    backend: &'static str,
}

impl Subscription for Sub {
    fn backend(&self) -> &str {
        self.backend
    }
}

impl Drop for Sub {
    fn drop(&mut self) {
        if let Some(s) = self.shared.upgrade() {
            s.remove(self.id);
        }
    }
}

fn backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "fsevents"
    } else if cfg!(target_os = "linux") {
        "inotify"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "kqueue"
    }
}

impl Shared {
    fn add(this: &Arc<Shared>, target: Target, publish: Publish) -> Option<Box<dyn Subscription>> {
        let (dir, names) = match target.kind {
            TargetKind::File => (
                target.path.parent()?.to_path_buf(),
                Some(target.path.file_name()?.to_os_string())
                    .into_iter()
                    .collect(),
            ),
            TargetKind::Entries => (
                target.path.clone(),
                target.names.iter().map(OsString::from).collect(),
            ),
            TargetKind::Directory => (target.path.clone(), HashSet::new()),
        };
        match std::fs::metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            // Missing (or not a directory): end the stream, nothing to log.
            Ok(_) => return None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                (this.log)(&format!("failed to watch directory {}: {e}", dir.display()));
                return None;
            }
        }
        let root = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let mut filter = if target.kind == TargetKind::Directory {
            let ignore = Arc::new(Ignore::new(&root, &target.ignore));
            let dirs = match this.mode {
                Mode::Recursive => vec![root.clone()],
                Mode::PerDirectory => tree_dirs(&root, &ignore),
            };
            Filter::Tree { ignore, dirs }
        } else {
            Filter::Names(names)
        };
        let mut s = this.state.lock().unwrap();
        if s.watcher.is_none() {
            match make_watcher(Arc::downgrade(this)) {
                Ok(w) => s.watcher = Some(w),
                Err(e) => {
                    drop(s);
                    (this.log)(&format!("watcher backend failed to start: {e}"));
                    return None;
                }
            }
        }
        let id = s.next;
        s.next += 1;
        let failed = match &mut filter {
            Filter::Names(_) => this.watch_dir(&mut s, &root, false).err(),
            Filter::Tree { dirs, .. } => {
                let recursive = this.mode == Mode::Recursive;
                let mut kept = Vec::with_capacity(dirs.len());
                let mut failed = None;
                for d in dirs.iter() {
                    match this.watch_dir(&mut s, d, recursive) {
                        Ok(()) => kept.push(d.clone()),
                        // The root must be watched; past the limit the rest are skipped.
                        Err(e) if d == &root => {
                            failed = Some(e);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                if failed.is_some() {
                    for d in &kept {
                        this.unwatch_dir(&mut s, d, recursive);
                    }
                }
                *dirs = kept;
                failed
            }
        };
        if let Some(e) = failed {
            if s.routes.is_empty() && s.dirs.is_empty() {
                s.watcher = None;
            }
            drop(s);
            (this.log)(&format!("failed to subscribe to {}: {e}", root.display()));
            return None;
        }
        s.routes.insert(
            id,
            Route {
                root,
                shown: dir,
                filter,
                publish,
            },
        );
        Some(Box::new(Sub {
            id,
            shared: Arc::downgrade(this),
            backend: backend(),
        }))
    }

    fn remove(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        let Some(route) = s.routes.remove(&id) else {
            return;
        };
        match &route.filter {
            Filter::Names(_) => self.unwatch_dir(&mut s, &route.root, false),
            Filter::Tree { dirs, .. } => {
                let recursive = self.mode == Mode::Recursive;
                for d in dirs {
                    self.unwatch_dir(&mut s, d, recursive);
                }
            }
        }
        if s.routes.is_empty() {
            // Nothing left: stop the platform watcher and its threads.
            s.watcher = None;
            s.dirs.clear();
            s.full = false;
        }
    }

    fn mode_of((plain, rec): (usize, usize)) -> Option<RecursiveMode> {
        if rec > 0 {
            Some(RecursiveMode::Recursive)
        } else if plain > 0 {
            Some(RecursiveMode::NonRecursive)
        } else {
            None
        }
    }

    fn watch_dir(&self, s: &mut State, dir: &Path, recursive: bool) -> notify::Result<()> {
        if !recursive && !s.dirs.contains_key(dir) && s.dirs.len() >= self.limit {
            if !s.full {
                s.full = true;
                (self.log)(&format!(
                    "watching at most {} directories; {} and later ones are not watched",
                    self.limit,
                    dir.display()
                ));
            }
            return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
        }
        let before = s.dirs.get(dir).copied().unwrap_or_default();
        let after = if recursive {
            (before.0, before.1 + 1)
        } else {
            (before.0 + 1, before.1)
        };
        let (old, new) = (Self::mode_of(before), Self::mode_of(after));
        if old != new {
            let w = s.watcher.as_mut().expect("watcher");
            if old.is_some() {
                let _ = w.unwatch(dir);
            }
            if let Err(e) = w.watch(dir, new.unwrap()) {
                if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) && !s.full {
                    s.full = true;
                    (self.log)(&format!(
                        "the inotify watch limit (fs.inotify.max_user_watches) is reached; {} and later directories are not watched",
                        dir.display()
                    ));
                }
                if let Some(m) = old {
                    let _ = s.watcher.as_mut().unwrap().watch(dir, m);
                }
                return Err(e);
            }
        }
        s.dirs.insert(dir.to_path_buf(), after);
        Ok(())
    }

    fn unwatch_dir(&self, s: &mut State, dir: &Path, recursive: bool) {
        let Some(&before) = s.dirs.get(dir) else {
            return;
        };
        let after = if recursive {
            (before.0, before.1.saturating_sub(1))
        } else {
            (before.0.saturating_sub(1), before.1)
        };
        let (old, new) = (Self::mode_of(before), Self::mode_of(after));
        if old != new {
            if let Some(w) = s.watcher.as_mut() {
                let _ = w.unwatch(dir);
                if let Some(m) = new {
                    let _ = w.watch(dir, m);
                }
            }
        }
        if new.is_none() {
            s.dirs.remove(dir);
        } else {
            s.dirs.insert(dir.to_path_buf(), after);
        }
    }

    /// Route one raw event to the subscriptions it concerns.
    fn dispatch(&self, event: notify::Event) {
        if event.need_rescan() {
            return self.overflow();
        }
        let Some(kind) = kind_of(&event.kind) else {
            return;
        };
        let mut sends: Vec<(Publish, Update)> = Vec::new();
        let mut new_dirs: Vec<(u64, PathBuf)> = Vec::new();
        {
            let s = self.state.lock().unwrap();
            for path in &event.paths {
                let kind = kind.unwrap_or_else(|| {
                    if path.exists() {
                        Kind::Create
                    } else {
                        Kind::Delete
                    }
                });
                for (id, r) in &s.routes {
                    match &r.filter {
                        Filter::Names(names) => {
                            if path.parent() == Some(&r.root)
                                && path.file_name().is_some_and(|n| names.contains(n))
                            {
                                let name = path.file_name().unwrap();
                                sends.push((
                                    r.publish.clone(),
                                    Update::new(r.shown.join(name), kind),
                                ));
                            }
                        }
                        Filter::Tree { ignore, .. } => {
                            let Ok(rel) = path.strip_prefix(&r.root) else {
                                continue;
                            };
                            if rel.as_os_str().is_empty() {
                                continue;
                            }
                            let is_dir = kind != Kind::Delete && path.is_dir();
                            if ignore.ignored(rel, is_dir) {
                                continue;
                            }
                            if is_dir && kind == Kind::Create && self.mode == Mode::PerDirectory {
                                new_dirs.push((*id, path.clone()));
                            }
                            sends.push((r.publish.clone(), Update::new(r.shown.join(rel), kind)));
                        }
                    }
                }
            }
        }
        for (p, u) in sends {
            p.send(u);
        }
        for (id, dir) in new_dirs {
            self.grow(id, &dir);
        }
    }

    /// A directory appeared under a per-directory tree: watch it and what
    /// is already inside it.
    fn grow(&self, id: u64, dir: &Path) {
        let ignore = {
            let s = self.state.lock().unwrap();
            match s.routes.get(&id).map(|r| &r.filter) {
                Some(Filter::Tree { ignore, .. }) => ignore.clone(),
                _ => return,
            }
        };
        let found = tree_dirs(dir, &ignore);
        let mut s = self.state.lock().unwrap();
        let mut added = Vec::new();
        for d in found {
            let known = match s.routes.get(&id).map(|r| &r.filter) {
                Some(Filter::Tree { dirs, .. }) => dirs.contains(&d),
                _ => return,
            };
            if known {
                continue;
            }
            if self.watch_dir(&mut s, &d, false).is_ok() {
                added.push(d);
            }
        }
        if let Some(Route {
            filter: Filter::Tree { dirs, .. },
            ..
        }) = s.routes.get_mut(&id)
        {
            dirs.extend(added);
        }
    }

    fn overflow(&self) {
        let s = self.state.lock().unwrap();
        let sends: Vec<_> = s
            .routes
            .values()
            .map(|r| {
                (
                    r.publish.clone(),
                    Update::new(r.shown.clone(), Kind::Overflow),
                )
            })
            .collect();
        drop(s);
        for (p, u) in sends {
            p.send(u);
        }
    }
}

/// `Some(None)`: decide create or delete by whether the path exists.
fn kind_of(kind: &EventKind) -> Option<Option<Kind>> {
    match kind {
        EventKind::Access(_) => None,
        EventKind::Create(_) => Some(Some(Kind::Create)),
        EventKind::Remove(_) => Some(Some(Kind::Delete)),
        EventKind::Modify(ModifyKind::Name(_)) => Some(None),
        EventKind::Modify(_) => Some(Some(Kind::Update)),
        EventKind::Any | EventKind::Other => Some(Some(Kind::Update)),
    }
}

fn make_watcher(shared: Weak<Shared>) -> notify::Result<RecommendedWatcher> {
    let (tx, rx) = mpsc::sync_channel::<notify::Result<notify::Event>>(RAW_QUEUE);
    let lost = Arc::new(AtomicBool::new(false));
    let flag = lost.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if tx.try_send(res).is_err() {
            flag.store(true, Ordering::Relaxed);
        }
    })?;
    std::thread::Builder::new()
        .name("codesmith-watch".into())
        .spawn(move || {
            // Ends when the watcher (and with it the sender) is dropped.
            for res in rx {
                let Some(s) = shared.upgrade() else { return };
                if lost.swap(false, Ordering::Relaxed) {
                    s.overflow();
                }
                match res {
                    Ok(event) => s.dispatch(event),
                    Err(e) => (s.log)(&format!("watcher callback failed: {e}")),
                }
            }
        })
        .map_err(notify::Error::io)?;
    Ok(watcher)
}

/// The directories of a tree worth watching, root first.
fn tree_dirs(root: &Path, ignore: &Arc<Ignore>) -> Vec<PathBuf> {
    let base = ignore.root.clone();
    let filter = ignore.clone();
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if !is_dir {
                return false;
            }
            match e.path().strip_prefix(&base) {
                Ok(rel) if !rel.as_os_str().is_empty() => !filter.ignored(rel, true),
                _ => true,
            }
        })
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .map(|e| e.into_path())
        .collect()
}

/// What a tree subscription leaves out.
pub struct Ignore {
    root: PathBuf,
    /// Root-relative path prefixes.
    paths: Vec<PathBuf>,
    globs: GlobSet,
    git: Option<Gitignore>,
}

impl Ignore {
    /// `entries` are paths or globs relative to `root`, as Parcel takes
    /// them; the root's `.gitignore` and `.git/info/exclude` apply too.
    pub fn new(root: &Path, entries: &[String]) -> Ignore {
        let mut paths = Vec::new();
        let mut globs = GlobSetBuilder::new();
        for e in entries {
            if e.contains(['*', '?', '[', '{']) {
                if let Ok(g) = Glob::new(e) {
                    globs.add(g);
                }
            } else {
                paths.push(PathBuf::from(e.trim_end_matches('/')));
            }
        }
        let mut git = GitignoreBuilder::new(root);
        let mut any = false;
        for f in [root.join(".gitignore"), root.join(".git/info/exclude")] {
            if f.is_file() {
                any |= git.add(f).is_none();
            }
        }
        Ignore {
            root: root.to_path_buf(),
            paths,
            globs: globs.build().unwrap_or_else(|_| GlobSet::empty()),
            git: any.then(|| git.build().ok()).flatten(),
        }
    }

    /// Whether `rel` (relative to the root) is left out.
    pub fn ignored(&self, rel: &Path, is_dir: bool) -> bool {
        if rel.components().any(|c| {
            super::IGNORED_DIRECTORIES
                .iter()
                .any(|n| c.as_os_str() == *n)
        }) {
            return true;
        }
        if self.paths.iter().any(|p| rel.starts_with(p)) {
            return true;
        }
        if !self.globs.is_empty()
            && rel
                .ancestors()
                .any(|a| !a.as_os_str().is_empty() && self.globs.is_match(a))
        {
            return true;
        }
        self.git
            .as_ref()
            .is_some_and(|g| g.matched_path_or_any_parents(rel, is_dir).is_ignore())
    }
}
