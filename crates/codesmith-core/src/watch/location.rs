//! The project's branch metadata (OpenCode's `LocationWatcher`): Git's
//! `HEAD` in the resolved Git directory (through a symlinked or worktree
//! `.git`), or Mercurial's `.hg/branch`, each published as
//! `filesystem.changed`. The ignore policy turns it off when it names
//! `.git` (or `.hg`, or the resolved directory). It starts in the
//! background once `activation` is done (config loaded), reconciles on
//! every policy change without subscribing twice, and stops with its
//! handle.

use super::policy::{Policy, Registration};
use super::{BoxFuture, Change, Changed, Events, WatchInput, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};

/// The version control a location uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vcs {
    Git,
    /// Mercurial, with its `.hg` store.
    Hg {
        store: PathBuf,
    },
}

impl Vcs {
    /// What `directory` uses: a `.git` (directory or file) or a `.hg`.
    pub fn detect(directory: &Path) -> Option<Vcs> {
        if directory.join(".git").exists() {
            Some(Vcs::Git)
        } else if directory.join(".hg").is_dir() {
            Some(Vcs::Hg {
                store: directory.join(".hg"),
            })
        } else {
            None
        }
    }
}

/// The file to watch and the names the policy can turn it off by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub aliases: Vec<String>,
}

/// Find the target (may be slow: it reads the file system).
pub type Discover = Arc<dyn Fn() -> BoxFuture<Option<Target>> + Send + Sync>;

/// The default discovery for `directory`.
pub fn discover(directory: &Path, vcs: Option<Vcs>) -> Discover {
    let directory = directory.to_path_buf();
    Arc::new(move || {
        let (directory, vcs) = (directory.clone(), vcs.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || target(&directory, vcs.as_ref()?))
                .await
                .ok()
                .flatten()
        })
    })
}

/// The branch file for `vcs` in `directory`.
pub fn target(directory: &Path, vcs: &Vcs) -> Option<Target> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match vcs {
        Vcs::Git => {
            let resolved = git_dir(directory)?;
            let vcs = real(&resolved);
            let mut aliases = vec![".git".to_string(), vcs.display().to_string()];
            aliases.push(resolved.display().to_string());
            Some(Target {
                path: vcs.join("HEAD"),
                aliases,
            })
        }
        Vcs::Hg { store } => {
            let vcs = real(store);
            Some(Target {
                path: vcs.join("branch"),
                aliases: vec![".hg".into(), vcs.display().to_string()],
            })
        }
    }
}

/// The Git directory of `directory`: `.git` itself, or where a `.git` file
/// (a worktree or submodule) points.
fn git_dir(directory: &Path) -> Option<PathBuf> {
    let dot = directory.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let to = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let to = Path::new(to);
    Some(if to.is_absolute() {
        to.to_path_buf()
    } else {
        directory.join(to)
    })
}

struct Active {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

struct Inner {
    directory: PathBuf,
    watcher: Watcher,
    policy: Policy,
    events: Events,
    discover: Discover,
    target: OnceCell<Option<Target>>,
    lock: AsyncMutex<(bool, Option<Active>)>,
    /// The active watch's task, for stopping it from `Drop`.
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Inner {
    async fn reconcile(&self) {
        let mut g = self.lock.lock().await;
        if g.0 {
            return;
        }
        let target = self.target.get_or_init(|| (self.discover)()).await;
        // Read after discovery: a change made meanwhile must count.
        let ignore = self.policy.current();
        let next = target
            .as_ref()
            .filter(|t| !t.aliases.iter().any(|a| ignore.contains(a)))
            .map(|t| t.path.clone());
        if g.1.as_ref().map(|a| &a.path) == next.as_ref() {
            return;
        }
        if let Some(a) = g.1.take() {
            a.task.abort();
        }
        let Some(path) = next else { return };
        let mut updates = self.watcher.subscribe(WatchInput::File(path.clone()), None);
        let (events, location) = (self.events.clone(), self.directory.clone());
        let task = tokio::spawn(async move {
            while let Some(u) = updates.recv().await {
                events.publish(Changed {
                    location: location.clone(),
                    file: u.path,
                    event: Change::from(u.kind),
                });
            }
        });
        *self.abort.lock().unwrap() = Some(task.abort_handle());
        g.1 = Some(Active { path, task });
    }

    async fn stop(&self) {
        let mut g = self.lock.lock().await;
        g.0 = true;
        if let Some(a) = g.1.take() {
            a.task.abort();
        }
    }
}

/// A running location watcher; dropping it stops it.
pub struct LocationWatcher {
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
    _observer: Registration,
}

impl LocationWatcher {
    /// Start watching `directory`'s branch metadata in the background, once
    /// `activation` is done.
    pub fn start(
        directory: &Path,
        watcher: Watcher,
        policy: Policy,
        events: Events,
        discover: Discover,
        activation: Option<BoxFuture<()>>,
    ) -> LocationWatcher {
        let inner = Arc::new(Inner {
            directory: directory.to_path_buf(),
            watcher,
            policy: policy.clone(),
            events,
            discover,
            target: OnceCell::new(),
            lock: AsyncMutex::new((false, None)),
            abort: Mutex::default(),
        });
        let wake = Arc::new(Notify::new());
        let pending = Arc::new(Mutex::new(false));
        let observer = {
            let (wake, pending) = (wake.clone(), pending.clone());
            policy.observe(move |_| {
                *pending.lock().unwrap() = true;
                wake.notify_one();
            })
        };
        let task = {
            let inner = inner.clone();
            tokio::spawn(async move {
                if let Some(a) = activation {
                    a.await;
                }
                *pending.lock().unwrap() = false;
                inner.reconcile().await;
                loop {
                    wake.notified().await;
                    if std::mem::take(&mut *pending.lock().unwrap()) {
                        inner.reconcile().await;
                    }
                }
            })
        };
        LocationWatcher {
            inner,
            task,
            _observer: observer,
        }
    }

    /// Stop now and wait until the watch is released.
    pub async fn stop(&self) {
        self.task.abort();
        self.inner.stop().await;
    }

    /// The file watched now, if any.
    pub async fn watching(&self) -> Option<PathBuf> {
        self.inner
            .lock
            .lock()
            .await
            .1
            .as_ref()
            .map(|a| a.path.clone())
    }
}

impl Drop for LocationWatcher {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(a) = self.inner.abort.lock().unwrap().take() {
            a.abort();
        }
    }
}
