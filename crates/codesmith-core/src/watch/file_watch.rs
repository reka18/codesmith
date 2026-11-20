//! Files a client asked to watch (OpenCode's `FileWatch`), for the local
//! server's `/fs/watch`. A watch lasts 24 hours unless renewed by watching
//! the file again; a location holds at most 256, and a new one past that
//! evicts the one closest to expiring. A watch whose native side ended or
//! failed (its directory vanished, say) is forgotten, so the next request
//! starts a new one. Each change is published as `filesystem.changed` on
//! the watch's location: `change`, or `unlink` when the file is gone.

use super::{Change, Changed, Events, WatchInput, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::time::Instant;

pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const MAX_PER_LOCATION: usize = 256;

/// Watching is turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disabled;

impl std::fmt::Display for Disabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("File watching is disabled")
    }
}

impl std::error::Error for Disabled {}

struct Entry {
    id: u64,
    expires: Instant,
    task: tokio::task::JoinHandle<()>,
}

type Locations = HashMap<PathBuf, HashMap<PathBuf, Entry>>;

struct Inner {
    watcher: Watcher,
    events: Events,
    locations: Mutex<Locations>,
    next: AtomicU64,
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(s) = self.sweeper.lock().unwrap().take() {
            s.abort();
        }
        for files in self.locations.lock().unwrap().values() {
            for e in files.values() {
                e.task.abort();
            }
        }
    }
}

/// Clones share the watches; they stop when the last clone goes.
#[derive(Clone)]
pub struct FileWatch {
    inner: Option<Arc<Inner>>,
}

impl FileWatch {
    pub fn new(watcher: Watcher, events: Events) -> FileWatch {
        FileWatch {
            inner: Some(Arc::new(Inner {
                watcher,
                events,
                locations: Mutex::default(),
                next: AtomicU64::new(1),
                sweeper: Mutex::default(),
            })),
        }
    }

    /// One that refuses every watch.
    pub fn disabled() -> FileWatch {
        FileWatch { inner: None }
    }

    /// Start watching `file` (absolute) for `location`, or renew its expiry.
    pub fn watch(&self, location: &Path, file: &Path) -> Result<(), Disabled> {
        let inner = self.inner.as_ref().ok_or(Disabled)?;
        inner.start_sweeper();
        let expires = Instant::now() + TTL;
        let mut locations = inner.locations.lock().unwrap();
        let files = locations.entry(location.to_path_buf()).or_default();
        if let Some(e) = files.get_mut(file) {
            e.expires = expires;
            return Ok(());
        }
        if files.len() >= MAX_PER_LOCATION {
            let soonest = files
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(f, _)| f.clone());
            if let Some(f) = soonest {
                if let Some(e) = files.remove(&f) {
                    e.task.abort();
                }
            }
        }
        let id = inner.next.fetch_add(1, Ordering::Relaxed);
        let task = tokio::spawn(run(
            Arc::downgrade(inner),
            inner.watcher.clone(),
            inner.events.clone(),
            location.to_path_buf(),
            file.to_path_buf(),
            id,
        ));
        files.insert(file.to_path_buf(), Entry { id, expires, task });
        Ok(())
    }

    pub fn unwatch(&self, location: &Path, file: &Path) {
        let Some(inner) = &self.inner else { return };
        let mut locations = inner.locations.lock().unwrap();
        if let Some(files) = locations.get_mut(location) {
            if let Some(e) = files.remove(file) {
                e.task.abort();
            }
            if files.is_empty() {
                locations.remove(location);
            }
        }
    }

    /// Files watched for `location`.
    pub fn watched(&self, location: &Path) -> Vec<PathBuf> {
        let Some(inner) = &self.inner else {
            return vec![];
        };
        let mut v: Vec<PathBuf> = inner
            .locations
            .lock()
            .unwrap()
            .get(location)
            .map(|f| f.keys().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Stop the watches that expired (the sweeper does this every
    /// [`SWEEP_INTERVAL`]).
    pub fn sweep(&self) {
        if let Some(inner) = &self.inner {
            inner.sweep();
        }
    }
}

impl Inner {
    fn start_sweeper(self: &Arc<Self>) {
        let mut s = self.sweeper.lock().unwrap();
        if s.is_some() {
            return;
        }
        let weak = Arc::downgrade(self);
        *s = Some(tokio::spawn(async move {
            let mut tick =
                tokio::time::interval_at(Instant::now() + SWEEP_INTERVAL, SWEEP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                inner.sweep();
            }
        }));
    }

    fn sweep(&self) {
        let now = Instant::now();
        let mut locations = self.locations.lock().unwrap();
        for files in locations.values_mut() {
            files.retain(|_, e| {
                let keep = e.expires > now;
                if !keep {
                    e.task.abort();
                }
                keep
            });
        }
        locations.retain(|_, f| !f.is_empty());
    }
}

async fn run(
    inner: Weak<Inner>,
    watcher: Watcher,
    events: Events,
    location: PathBuf,
    file: PathBuf,
    id: u64,
) {
    {
        let mut updates = watcher.subscribe(WatchInput::File(file.clone()), None);
        while updates.recv().await.is_some() {
            let exists = tokio::fs::try_exists(&file).await.unwrap_or(false);
            events.publish(Changed {
                location: location.clone(),
                file: file.clone(),
                event: if exists {
                    Change::Change
                } else {
                    Change::Unlink
                },
            });
        }
    }
    // The watch ended or failed: forget it so the next watch starts anew.
    let Some(inner) = inner.upgrade() else { return };
    let mut locations = inner.locations.lock().unwrap();
    if let Some(files) = locations.get_mut(&location) {
        if files.get(&file).is_some_and(|e| e.id == id) {
            files.remove(&file);
        }
        if files.is_empty() {
            locations.remove(&location);
        }
    }
}
