//! What the location watcher leaves alone (OpenCode's
//! `LocationWatcherPolicy`): a list of ignore entries built from
//! registered transforms, such as config's `watcher.ignore`.
//!
//! A transform is kept while its [`Registration`] lives and is replayed on
//! [`Policy::reload`], so a transform that reads config sees its latest
//! state. Observers hear about every change, each reading the policy as it
//! is when its turn comes (so a change made by an earlier observer reaches
//! later ones); inside [`Policy::batch`] they hear once, at the end.

use std::sync::{Arc, Mutex, Weak};

/// Adds ignore entries while the policy is rebuilt.
#[derive(Default)]
pub struct Editor {
    ignore: Vec<String>,
}

impl Editor {
    pub fn add<S: AsRef<str>>(&mut self, ignore: impl IntoIterator<Item = S>) {
        self.ignore
            .extend(ignore.into_iter().map(|s| s.as_ref().to_string()));
    }

    pub fn list(&self) -> &[String] {
        &self.ignore
    }
}

type Transform = Arc<dyn Fn(&mut Editor) + Send + Sync>;
type Listener = Arc<dyn Fn(&[String]) + Send + Sync>;

#[derive(Default)]
struct Inner {
    transforms: Vec<(u64, Transform)>,
    listeners: Vec<(u64, Listener)>,
    current: Vec<String>,
    next: u64,
    batch: usize,
    dirty: bool,
}

/// The ignore policy. Clones share it.
#[derive(Clone, Default)]
pub struct Policy {
    inner: Arc<Mutex<Inner>>,
}

/// Keeps a transform or an observer; dropping it (or [`Self::dispose`])
/// removes it.
pub struct Registration {
    policy: Weak<Mutex<Inner>>,
    id: u64,
    listener: bool,
}

impl Registration {
    pub fn dispose(self) {}
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(inner) = self.policy.upgrade() else {
            return;
        };
        let policy = Policy { inner };
        if self.listener {
            policy
                .inner
                .lock()
                .unwrap()
                .listeners
                .retain(|(id, _)| *id != self.id);
        } else {
            policy
                .inner
                .lock()
                .unwrap()
                .transforms
                .retain(|(id, _)| *id != self.id);
            policy.changed();
        }
    }
}

impl Policy {
    pub fn new() -> Policy {
        Policy::default()
    }

    /// The ignore entries now.
    pub fn current(&self) -> Vec<String> {
        self.inner.lock().unwrap().current.clone()
    }

    /// Add a transform; it stays until the registration is dropped.
    pub fn transform(&self, f: impl Fn(&mut Editor) + Send + Sync + 'static) -> Registration {
        let id = {
            let mut g = self.inner.lock().unwrap();
            g.next += 1;
            let id = g.next;
            g.transforms.push((id, Arc::new(f)));
            id
        };
        self.changed();
        Registration {
            policy: Arc::downgrade(&self.inner),
            id,
            listener: false,
        }
    }

    /// Replay every transform (their inputs may have changed).
    pub fn reload(&self) {
        self.changed();
    }

    /// Call `f` with the policy now and after every change.
    pub fn observe(&self, f: impl Fn(&[String]) + Send + Sync + 'static) -> Registration {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        let id = g.next;
        g.listeners.push((id, Arc::new(f)));
        Registration {
            policy: Arc::downgrade(&self.inner),
            id,
            listener: true,
        }
    }

    /// Run `f` with observers held back; they hear once, after it.
    pub fn batch<R>(&self, f: impl FnOnce() -> R) -> R {
        self.inner.lock().unwrap().batch += 1;
        let out = f();
        let notify = {
            let mut g = self.inner.lock().unwrap();
            g.batch -= 1;
            g.batch == 0 && std::mem::take(&mut g.dirty)
        };
        if notify {
            self.notify();
        }
        out
    }

    fn changed(&self) {
        let transforms: Vec<Transform> = self
            .inner
            .lock()
            .unwrap()
            .transforms
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        let mut editor = Editor::default();
        for t in transforms {
            t(&mut editor);
        }
        let batching = {
            let mut g = self.inner.lock().unwrap();
            g.current = editor.ignore;
            if g.batch > 0 {
                g.dirty = true;
            }
            g.batch > 0
        };
        if !batching {
            self.notify();
        }
    }

    fn notify(&self) {
        let listeners: Vec<(u64, Listener)> = self.inner.lock().unwrap().listeners.clone();
        for (id, l) in listeners {
            // Read per listener: a change made by an earlier one reaches later ones.
            let (alive, now) = {
                let g = self.inner.lock().unwrap();
                (g.listeners.iter().any(|(i, _)| *i == id), g.current.clone())
            };
            if alive {
                l(&now);
            }
        }
    }
}
