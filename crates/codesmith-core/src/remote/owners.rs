//! The gateway's side of the owners (I9): the processes holding sessions,
//! connected over the control socket, and the devices following those
//! sessions' events.
//!
//! An owner's lines come in order, and the gateway handles them one at a
//! time, so a resume's replay (which the owner made under the lock that
//! numbers events) is followed exactly by the events after it: the
//! subscriber joins the live stream at the replay's line.
//!
//! Lines from an owner: `hold`, `info`, `event`, `release` (see
//! [`super::tap`]), and the answers `snapshot`, `replay` and `sync`. Lines
//! to it: `snapshot`, `subscribe` and `synced`.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The most frames waiting for one subscriber: one that falls this far
/// behind is dropped, and resumes from its cursor.
pub const QUEUE: usize = 4096;

/// What an event stream sends next.
#[derive(Debug, Clone)]
pub enum Item {
    /// An SSE frame, ready to write.
    Frame(Arc<String>),
    /// The stream ends.
    End,
}

/// An SSE frame for `data`, with the SSE id `<epoch>:<seq>` when given.
pub fn frame(id: Option<&str>, data: &Value) -> Arc<String> {
    let kind = data["type"].as_str().unwrap_or("message");
    let mut s = String::new();
    if let Some(id) = id {
        s.push_str("id: ");
        s.push_str(id);
        s.push('\n');
    }
    s.push_str("event: ");
    s.push_str(kind);
    s.push_str("\ndata: ");
    s.push_str(&data.to_string());
    s.push_str("\n\n");
    Arc::new(s)
}

/// The frame for an enveloped event: its SSE id is `<epoch>:<seq>`.
pub fn event_frame(ev: &Value) -> Arc<String> {
    let id = match (ev["epoch"].as_str(), ev["seq"].as_u64()) {
        (Some(e), Some(s)) => Some(format!("{e}:{s}")),
        _ => None,
    };
    frame(id.as_deref(), ev)
}

/// A `reset` (S5, S23): reload the session; the stream ends after it.
pub fn reset_frame(session: &str, reason: &str) -> Arc<String> {
    frame(
        None,
        &json!({"v": super::mirror::VERSION, "session": session, "type": "reset", "reason": reason}),
    )
}

/// The epoch a session no process holds has (S3).
pub const CLOSED_EPOCH: &str = "closed";

#[derive(Debug, Clone)]
pub struct Held {
    pub owner: u64,
    /// `tui`, `serve` or `acp`.
    pub kind: String,
    pub info: Value,
    pub busy: bool,
}

/// The sessions a device may follow: every one, or these.
#[derive(Debug, Clone)]
pub enum Granted {
    All,
    Some(HashSet<String>),
}

impl Granted {
    pub fn covers(&self, session: &str) -> bool {
        match self {
            Granted::All => true,
            Granted::Some(s) => s.contains(session),
        }
    }
}

struct Sub {
    tx: mpsc::Sender<Item>,
}

struct MachineSub {
    tx: mpsc::Sender<Item>,
    granted: Granted,
}

enum Waiting {
    Snapshot(oneshot::Sender<Result<Value, String>>),
    Subscribe { session: String, sub: Sub },
}

struct Owner {
    tx: mpsc::UnboundedSender<String>,
    kind: String,
}

#[derive(Default)]
struct Inner {
    owners: HashMap<u64, Owner>,
    held: HashMap<String, Held>,
    waiting: HashMap<u64, (u64, Waiting)>,
    subs: HashMap<String, Vec<Sub>>,
    machine: Vec<MachineSub>,
}

pub struct Owners {
    inner: Mutex<Inner>,
    next: AtomicU64,
    /// Set when the gateway stops: owner connections close.
    closing: tokio::sync::watch::Sender<bool>,
}

impl Default for Owners {
    fn default() -> Self {
        Owners {
            inner: Default::default(),
            next: AtomicU64::new(0),
            closing: tokio::sync::watch::channel(false).0,
        }
    }
}

fn line(v: Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

/// Send to a subscriber; false when it's gone or too far behind.
fn give(tx: &mpsc::Sender<Item>, f: &Arc<String>) -> bool {
    tx.try_send(Item::Frame(f.clone())).is_ok()
}

impl Owners {
    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// A new owner connection of `kind` (`tui`, `serve`, `acp`); its
    /// lines to send go to `tx`.
    pub fn connect(&self, kind: &str, tx: mpsc::UnboundedSender<String>) -> u64 {
        let id = self.id();
        let kind: String = kind
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        self.inner
            .lock()
            .unwrap()
            .owners
            .insert(id, Owner { tx, kind });
        id
    }

    /// The owner's connection ended: its sessions are closed now, and
    /// their streams end (a client resuming gets `reset` `epoch`).
    pub fn disconnect(&self, owner: u64) {
        let mut g = self.inner.lock().unwrap();
        g.owners.remove(&owner);
        let gone: Vec<String> = g
            .held
            .iter()
            .filter(|(_, h)| h.owner == owner)
            .map(|(s, _)| s.clone())
            .collect();
        for s in gone {
            g.held.remove(&s);
            for sub in g.subs.remove(&s).unwrap_or_default() {
                let _ = sub.tx.try_send(Item::End);
            }
        }
        g.waiting.retain(|_, (o, _)| *o != owner);
    }

    /// The gateway is stopping: every owner connection and event stream
    /// ends.
    pub fn close(&self) {
        self.closing.send_replace(true);
        let mut g = self.inner.lock().unwrap();
        for (_, subs) in g.subs.drain() {
            for s in subs {
                let _ = s.tx.try_send(Item::End);
            }
        }
        for m in g.machine.drain(..) {
            let _ = m.tx.try_send(Item::End);
        }
        g.held.clear();
        g.waiting.clear();
    }

    /// Resolves once the gateway is stopping.
    pub async fn closed(&self) {
        let mut rx = self.closing.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// How many owners are connected.
    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().owners.len()
    }

    /// One line from `owner`.
    pub fn handle(&self, owner: u64, v: &Value) {
        let session = v["session"].as_str().unwrap_or("").to_string();
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let ours = |g: &Inner| g.held.get(&session).is_some_and(|h| h.owner == owner);
        match v["op"].as_str().unwrap_or("") {
            "hold" => {
                if !valid_id(&session) {
                    return;
                }
                if g.held.get(&session).is_some_and(|h| h.owner != owner) {
                    // Another process holds it: the first keeps it.
                    return;
                }
                let kind = g
                    .owners
                    .get(&owner)
                    .map(|o| o.kind.clone())
                    .unwrap_or_default();
                g.held.insert(
                    session.clone(),
                    Held {
                        owner,
                        kind,
                        info: v["info"].clone(),
                        busy: v["busy"].as_bool().unwrap_or(false),
                    },
                );
                // Followers of it while it was closed reload.
                for sub in g.subs.remove(&session).unwrap_or_default() {
                    give(
                        &sub.tx,
                        &reset_frame(&session, super::mirror::reason::EPOCH),
                    );
                    let _ = sub.tx.try_send(Item::End);
                }
            }
            "info" if ours(g) => {
                let h = g.held.get_mut(&session).unwrap();
                h.info = v["info"].clone();
                h.busy = v["busy"].as_bool().unwrap_or(h.busy);
            }
            "release" if ours(g) => {
                g.held.remove(&session);
                for sub in g.subs.remove(&session).unwrap_or_default() {
                    let _ = sub.tx.try_send(Item::End);
                }
            }
            "event" if ours(g) => {
                let f = event_frame(&v["event"]);
                if let Some(subs) = g.subs.get_mut(&session) {
                    subs.retain(|s| give(&s.tx, &f));
                }
                g.machine
                    .retain(|m| !m.granted.covers(&session) || give(&m.tx, &f));
            }
            "sync" => {
                if let Some(o) = g.owners.get(&owner) {
                    let _ = o.tx.send(line(json!({"op": "synced", "req": v["req"]})));
                }
            }
            "snapshot" | "replay" => {
                let Some(req) = v["req"].as_u64() else { return };
                let Some((from, w)) = g.waiting.remove(&req) else {
                    return;
                };
                if from != owner {
                    return;
                }
                match w {
                    Waiting::Snapshot(tx) => {
                        let _ = tx.send(match v["error"].as_str() {
                            Some(e) => Err(e.to_string()),
                            None => Ok(v["snapshot"].clone()),
                        });
                    }
                    Waiting::Subscribe { session, sub } => {
                        let held = g.held.get(&session).is_some_and(|h| h.owner == owner);
                        if !held || v["error"].is_string() {
                            let _ = sub.tx.try_send(Item::End);
                            return;
                        }
                        if let Some(r) = v["reset"].as_str() {
                            give(&sub.tx, &reset_frame(&session, r));
                            let _ = sub.tx.try_send(Item::End);
                            return;
                        }
                        let mut ok = true;
                        for e in v["events"].as_array().into_iter().flatten() {
                            ok &= give(&sub.tx, &event_frame(e));
                        }
                        // The open prompts it hasn't had (S6): no SSE id,
                        // so the client's position stays.
                        for e in v["resend"].as_array().into_iter().flatten() {
                            ok &= give(&sub.tx, &frame(None, e));
                        }
                        if ok {
                            g.subs.entry(session).or_default().push(sub);
                        } else {
                            let _ = sub.tx.try_send(Item::End);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn held(&self, session: &str) -> Option<Held> {
        self.inner.lock().unwrap().held.get(session).cloned()
    }

    pub fn all_held(&self) -> Vec<(String, Held)> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<_> = g.held.iter().map(|(k, h)| (k.clone(), h.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Ask the owner holding `session` for a snapshot.
    pub async fn snapshot(&self, session: &str) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(h) = g.held.get(session).cloned() else {
                return Err("not held".into());
            };
            let req = self.id();
            g.waiting.insert(req, (h.owner, Waiting::Snapshot(tx)));
            let sent = g.owners.get(&h.owner).is_some_and(|o| {
                o.tx.send(line(
                    json!({"op": "snapshot", "req": req, "session": session}),
                ))
                .is_ok()
            });
            if !sent {
                g.waiting.remove(&req);
                return Err("the owner went".into());
            }
        }
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(r)) => r,
            _ => Err("the owner didn't answer".into()),
        }
    }

    /// Follow `session` after `cursor` (`<epoch>:<seq>`, or none for from
    /// now). Its replay, then its live events, go to `tx`. For a session
    /// no process holds: from the closed epoch, the stream waits (and
    /// gets `reset` when an owner takes it); from any other, `reset`.
    pub fn subscribe(&self, session: &str, cursor: Option<&str>, tx: mpsc::Sender<Item>) {
        let mut g = self.inner.lock().unwrap();
        let Some(h) = g.held.get(session).cloned() else {
            let epoch = cursor.and_then(|c| c.rsplit_once(':')).map(|(e, _)| e);
            if epoch.is_some_and(|e| e != CLOSED_EPOCH) {
                give(&tx, &reset_frame(session, super::mirror::reason::EPOCH));
                let _ = tx.try_send(Item::End);
            } else {
                g.subs
                    .entry(session.to_string())
                    .or_default()
                    .push(Sub { tx });
            }
            return;
        };
        let req = self.id();
        let sent = g.owners.get(&h.owner).is_some_and(|o| {
            o.tx.send(line(
                json!({"op": "subscribe", "req": req, "session": session, "cursor": cursor}),
            ))
            .is_ok()
        });
        if sent {
            g.waiting.insert(
                req,
                (
                    h.owner,
                    Waiting::Subscribe {
                        session: session.to_string(),
                        sub: Sub { tx },
                    },
                ),
            );
        } else {
            let _ = tx.try_send(Item::End);
        }
    }

    /// Follow every session `granted` covers, from now on.
    pub fn subscribe_all(&self, granted: Granted, tx: mpsc::Sender<Item>) {
        self.inner
            .lock()
            .unwrap()
            .machine
            .push(MachineSub { tx, granted });
    }
}

/// A session id an owner may register: what the stores use.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
