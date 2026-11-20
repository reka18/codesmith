//! File watching, ported from OpenCode's `filesystem/watcher.ts` and the
//! services built on it.
//!
//! - [`Watcher`]: shared, reference-counted subscriptions to a file, to
//!   named entries of a directory, or to a directory tree with an ignore
//!   list. Equivalent subscriptions share one native watch, which is
//!   released exactly once, when the last consumer goes or the watcher is
//!   shut down. Nothing native starts until a subscription is first read,
//!   so creating a watcher costs nothing at startup.
//! - [`native::Notify`]: the platform backend (FSEvents on macOS, inotify on
//!   Linux, ReadDirectoryChangesW on Windows) through the `notify` crate.
//! - [`file_watch::FileWatch`]: files a client asked to watch (the local
//!   server's `/fs/watch`), expiring after 24 hours, at most 256 per
//!   location, each change published as `filesystem.changed`.
//! - [`location::LocationWatcher`]: the project's branch metadata
//!   (`.git/HEAD`, `.hg/branch`), unless the `watcher.ignore` policy names
//!   it.
//! - [`policy::Policy`]: that ignore policy, built from config.
//!
//! Language servers use a directory subscription to hear about files
//! changed outside the agent (see [`crate::lsp`]).
//!
//! `CODESMITH_DISABLE_FILEWATCHER` turns all of it off: subscriptions end at
//! once and `FileWatch` refuses watches.

pub mod file_watch;
pub mod location;
pub mod native;
pub mod policy;

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, watch as tokio_watch};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Updates buffered per subscription before a slow consumer starts losing
/// them (it then gets one [`Kind::Overflow`]).
pub const QUEUE: usize = 1024;

/// Directories never watched inside a watched tree (OpenCode's
/// `FSUtil.ignoredDirectories`).
pub const IGNORED_DIRECTORIES: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

/// Set to turn file watching off (OpenCode: `OPENCODE_DISABLE_FILEWATCHER`).
pub const DISABLE_VAR: &str = "CODESMITH_DISABLE_FILEWATCHER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Create,
    Update,
    Delete,
    /// The queue was full and updates were lost; rescan what matters.
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub path: PathBuf,
    pub kind: Kind,
}

impl Update {
    pub fn new(path: impl Into<PathBuf>, kind: Kind) -> Update {
        Update {
            path: path.into(),
            kind,
        }
    }
}

/// What to watch (OpenCode's `Watcher.WatchInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchInput {
    /// One file (or directory) by path; it may not exist yet.
    File(PathBuf),
    /// The named immediate entries of a directory.
    Entries { path: PathBuf, names: Vec<String> },
    /// A whole tree, minus `ignore`: directory names, or globs relative to
    /// the root.
    Directory { path: PathBuf, ignore: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    File,
    Entries,
    Directory,
}

/// A normalized [`WatchInput`]: equal targets share one native watch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub kind: TargetKind,
    pub path: PathBuf,
    /// Sorted and deduplicated (entries only).
    pub names: Vec<String>,
    /// Sorted and deduplicated (directories only).
    pub ignore: Vec<String>,
}

impl Target {
    pub fn new(input: &WatchInput) -> Target {
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        };
        let (kind, path, names, ignore) = match input {
            WatchInput::File(p) => (TargetKind::File, p, vec![], vec![]),
            WatchInput::Entries { path, names } => {
                (TargetKind::Entries, path, sorted(names), vec![])
            }
            WatchInput::Directory { path, ignore } => {
                (TargetKind::Directory, path, vec![], sorted(ignore))
            }
        };
        Target {
            kind,
            path: absolute(path),
            names,
            ignore,
        }
    }

    /// Back to the input it came from (without empty ignore lists' noise).
    pub fn input(&self) -> WatchInput {
        match self.kind {
            TargetKind::File => WatchInput::File(self.path.clone()),
            TargetKind::Entries => WatchInput::Entries {
                path: self.path.clone(),
                names: self.names.clone(),
            },
            TargetKind::Directory => WatchInput::Directory {
                path: self.path.clone(),
                ignore: self.ignore.clone(),
            },
        }
    }
}

/// `path` made absolute and lexically normalized (like `path.resolve`).
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Where a native watch sends what it sees. Cheap to clone; a publish after
/// the watch was released goes nowhere.
#[derive(Clone)]
pub struct Publish(PublishTo);

#[derive(Clone)]
enum PublishTo {
    Channel(Arc<Mutex<Option<broadcast::Sender<Update>>>>),
    Fn(Arc<dyn Fn(Update) + Send + Sync>),
}

impl Publish {
    pub fn send(&self, update: Update) {
        match &self.0 {
            PublishTo::Channel(tx) => {
                if let Some(tx) = &*tx.lock().unwrap() {
                    let _ = tx.send(update);
                }
            }
            PublishTo::Fn(f) => f(update),
        }
    }

    fn channel(tx: broadcast::Sender<Update>) -> Publish {
        Publish(PublishTo::Channel(Arc::new(Mutex::new(Some(tx)))))
    }

    fn close(&self) {
        if let PublishTo::Channel(tx) = &self.0 {
            tx.lock().unwrap().take();
        }
    }

    /// A publish that calls `f` (for driving a [`Native`] directly).
    pub fn from_fn(f: impl Fn(Update) + Send + Sync + 'static) -> Publish {
        Publish(PublishTo::Fn(Arc::new(f)))
    }
}

/// A live native watch; dropping it stops the watch.
pub trait Subscription: Send {
    /// The backend's name, for logs ("fsevents", "inotify", ...).
    fn backend(&self) -> &str {
        "native"
    }
}

/// The platform side of [`Watcher`] (OpenCode's `Watcher.Native`).
/// `subscribe` resolves to `None` when the target cannot be watched (a
/// missing directory, an unsupported platform): consumers' streams then
/// end. Dropping the future abandons the acquisition.
pub trait Native: Send + Sync + 'static {
    fn subscribe(
        &self,
        target: Target,
        publish: Publish,
    ) -> BoxFuture<Option<Box<dyn Subscription>>>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Acquired {
    Pending,
    Active,
    Dead,
}

struct Entry {
    id: u64,
    refs: usize,
    tx: broadcast::Sender<Update>,
    publish: Publish,
    state: tokio_watch::Receiver<Acquired>,
    task: tokio::task::JoinHandle<()>,
    sub: Arc<Mutex<Option<Box<dyn Subscription>>>>,
}

impl Entry {
    /// Stop the acquisition or the native watch, and end the streams.
    fn release(self) {
        self.task.abort();
        self.publish.close();
        let sub = self.sub.lock().unwrap().take();
        drop(sub);
    }
}

struct Inner {
    native: Option<Arc<dyn Native>>,
    entries: Mutex<HashMap<Target, Entry>>,
    next: AtomicU64,
    stopped: std::sync::atomic::AtomicBool,
}

/// Shared native subscriptions (OpenCode's `Watcher.Service`). Clones share
/// one set of watches.
#[derive(Clone)]
pub struct Watcher {
    inner: Arc<Inner>,
}

impl Watcher {
    pub fn new(native: Arc<dyn Native>) -> Watcher {
        Watcher::make(Some(native))
    }

    /// A watcher whose streams end at once (watching turned off).
    pub fn disabled() -> Watcher {
        Watcher::make(None)
    }

    fn make(native: Option<Arc<dyn Native>>) -> Watcher {
        Watcher {
            inner: Arc::new(Inner {
                native,
                entries: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                stopped: Default::default(),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.native.is_some()
    }

    /// Subscribe to `input`. Nothing starts until the stream is first read;
    /// then `on_ready` runs once the native watch is acquired and this
    /// consumer is listening (not at all when the watch is unavailable), and
    /// updates published meanwhile are kept for it.
    pub fn subscribe(&self, input: WatchInput, on_ready: Option<BoxFuture<()>>) -> Updates {
        Updates {
            watcher: self.clone(),
            state: Stream::Unstarted {
                target: Target::new(&input),
                on_ready,
            },
        }
    }

    /// Release every native watch now; streams end. Later subscriptions end
    /// at once.
    pub fn shutdown(&self) {
        self.inner
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let all: Vec<Entry> = self
            .inner
            .entries
            .lock()
            .unwrap()
            .drain()
            .map(|(_, e)| e)
            .collect();
        for e in all {
            e.release();
        }
    }

    /// How many native watches are held.
    pub fn active(&self) -> usize {
        self.inner.entries.lock().unwrap().len()
    }

    fn lease(
        &self,
        target: &Target,
    ) -> Option<(
        u64,
        broadcast::Receiver<Update>,
        tokio_watch::Receiver<Acquired>,
    )> {
        let native = self.inner.native.clone()?;
        if self.inner.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let mut entries = self.inner.entries.lock().unwrap();
        if let Some(e) = entries.get_mut(target) {
            e.refs += 1;
            return Some((e.id, e.tx.subscribe(), e.state.clone()));
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = broadcast::channel(QUEUE);
        let publish = Publish::channel(tx.clone());
        let (state_tx, state) = tokio_watch::channel(Acquired::Pending);
        let sub: Arc<Mutex<Option<Box<dyn Subscription>>>> = Arc::default();
        let task = {
            let (target, publish, sub) = (target.clone(), publish.clone(), sub.clone());
            tokio::spawn(async move {
                let got = native.subscribe(target, publish.clone()).await;
                let alive = got.is_some();
                *sub.lock().unwrap() = got;
                if !alive {
                    // Unsupported or missing: end the streams instead of hanging them.
                    publish.close();
                }
                let _ = state_tx.send(if alive {
                    Acquired::Active
                } else {
                    Acquired::Dead
                });
            })
        };
        entries.insert(
            target.clone(),
            Entry {
                id,
                refs: 1,
                tx,
                publish,
                state: state.clone(),
                task,
                sub,
            },
        );
        Some((id, rx, state))
    }

    fn unlease(&self, target: &Target, id: u64) {
        let mut entries = self.inner.entries.lock().unwrap();
        let Some(e) = entries.get_mut(target).filter(|e| e.id == id) else {
            return;
        };
        e.refs -= 1;
        if e.refs > 0 {
            return;
        }
        let e = entries.remove(target).unwrap();
        drop(entries);
        e.release();
    }
}

enum Stream {
    Unstarted {
        target: Target,
        on_ready: Option<BoxFuture<()>>,
    },
    Started {
        target: Target,
        id: u64,
        rx: broadcast::Receiver<Update>,
    },
    Ended,
}

/// A subscription's updates. Dropping it releases its share of the watch.
pub struct Updates {
    watcher: Watcher,
    state: Stream,
}

impl Updates {
    /// The next update, or `None` once the watch ended (unavailable,
    /// released by shutdown, or turned off).
    pub async fn recv(&mut self) -> Option<Update> {
        if let Stream::Unstarted { .. } = self.state {
            let Stream::Unstarted { target, on_ready } =
                std::mem::replace(&mut self.state, Stream::Ended)
            else {
                unreachable!()
            };
            let (id, rx, mut state) = self.watcher.lease(&target)?;
            // Held from here, so dropping this future mid-acquisition releases it.
            self.state = Stream::Started { target, id, rx };
            let acquired = state
                .wait_for(|s| *s != Acquired::Pending)
                .await
                .map(|s| *s)
                .ok();
            if acquired != Some(Acquired::Active) {
                self.end();
                return None;
            }
            if let Some(ready) = on_ready {
                ready.await;
            }
        }
        let Stream::Started { target, rx, .. } = &mut self.state else {
            return None;
        };
        match rx.recv().await {
            Ok(u) => Some(u),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                Some(Update::new(target.path.clone(), Kind::Overflow))
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.end();
                None
            }
        }
    }

    fn end(&mut self) {
        if let Stream::Started { target, id, .. } =
            std::mem::replace(&mut self.state, Stream::Ended)
        {
            self.watcher.unlease(&target, id);
        }
    }
}

impl Drop for Updates {
    fn drop(&mut self) {
        self.end();
    }
}

/// The process's watcher on the platform backend, made on first use, or a
/// disabled one when [`DISABLE_VAR`] is set.
pub fn global() -> Watcher {
    static GLOBAL: OnceLock<Watcher> = OnceLock::new();
    GLOBAL
        .get_or_init(|| {
            if std::env::var_os(DISABLE_VAR).is_some_and(|v| truthy(&v.to_string_lossy())) {
                Watcher::disabled()
            } else {
                Watcher::new(Arc::new(native::Notify::new()))
            }
        })
        .clone()
}

fn truthy(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Watcher errors, most recent last (at most 32). Nothing is printed, as
/// that would garble the terminal UI.
pub fn recent_errors() -> Vec<String> {
    errors().lock().unwrap().clone()
}

fn errors() -> &'static Mutex<Vec<String>> {
    static ERRORS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    ERRORS.get_or_init(Default::default)
}

/// A sink for a backend's errors.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// The default sink: [`recent_errors`].
pub fn default_log() -> Log {
    Arc::new(|msg: &str| {
        let mut e = errors().lock().unwrap();
        if e.len() >= 32 {
            e.remove(0);
        }
        e.push(msg.to_string());
    })
}

/// A backend for tests: records each native subscription and delivers
/// [`testing::TestNative::emit`]ted updates to the active ones whose target
/// covers the path, as OpenCode's `Watcher.testLayer` does.
pub mod testing {
    use super::*;

    type Active = Vec<(u64, Target, Publish)>;

    #[derive(Default)]
    pub struct TestNative {
        subscriptions: Mutex<Vec<WatchInput>>,
        active: Arc<Mutex<Active>>,
        next: AtomicU64,
    }

    struct Sub(u64, Arc<Mutex<Active>>);

    impl Subscription for Sub {}

    impl Drop for Sub {
        fn drop(&mut self) {
            self.1.lock().unwrap().retain(|(id, ..)| *id != self.0);
        }
    }

    impl TestNative {
        pub fn new() -> Arc<TestNative> {
            Arc::default()
        }

        /// Every native subscribe so far, in order.
        pub fn subscriptions(&self) -> Vec<WatchInput> {
            self.subscriptions.lock().unwrap().clone()
        }

        pub fn active(&self) -> usize {
            self.active.lock().unwrap().len()
        }

        pub fn emit(&self, update: Update) {
            let path = absolute(&update.path);
            for (_, t, publish) in self.active.lock().unwrap().iter() {
                let hit = match t.kind {
                    TargetKind::File => path == t.path,
                    TargetKind::Entries => {
                        path.parent() == Some(&t.path)
                            && path
                                .file_name()
                                .is_some_and(|n| t.names.iter().any(|x| n == x.as_str()))
                    }
                    TargetKind::Directory => {
                        path.starts_with(&t.path)
                            && !t.ignore.iter().any(|i| path.starts_with(t.path.join(i)))
                    }
                };
                if hit {
                    publish.send(update.clone());
                }
            }
        }
    }

    impl Native for TestNative {
        fn subscribe(
            &self,
            target: Target,
            publish: Publish,
        ) -> BoxFuture<Option<Box<dyn Subscription>>> {
            self.subscriptions.lock().unwrap().push(target.input());
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.active.lock().unwrap().push((id, target, publish));
            let active = self.active.clone();
            Box::pin(async move { Some(Box::new(Sub(id, active)) as Box<dyn Subscription>) })
        }
    }
}

/// What happened to a file, as `filesystem.changed` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Add,
    Change,
    Unlink,
}

impl From<Kind> for Change {
    fn from(k: Kind) -> Change {
        match k {
            Kind::Create => Change::Add,
            Kind::Delete => Change::Unlink,
            Kind::Update | Kind::Overflow => Change::Change,
        }
    }
}

/// OpenCode's `filesystem.changed` event, published on a location.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Changed {
    /// The project directory it belongs to.
    pub location: PathBuf,
    pub file: PathBuf,
    pub event: Change,
}

/// Where `filesystem.changed` events go: a bounded broadcast (a slow
/// listener loses the oldest).
#[derive(Clone)]
pub struct Events(broadcast::Sender<Changed>);

impl Default for Events {
    fn default() -> Self {
        Events(broadcast::channel(QUEUE).0)
    }
}

impl Events {
    pub fn new() -> Events {
        Events::default()
    }

    pub fn publish(&self, event: Changed) {
        let _ = self.0.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Changed> {
        self.0.subscribe()
    }
}

/// The `watcher` config key (OpenCode's `watcher`).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct WatcherConfig {
    /// Left unwatched: paths or globs relative to the project (`dist`,
    /// `**/*.gen.ts`). Naming `.git` (or `.hg`) also stops the branch
    /// watch.
    pub ignore: Vec<String>,
}
