//! What an owning process (the TUI, `codesmith serve`, ACP) runs so phones
//! can follow its sessions (I9, A8): one [`Hub`] per process, a [`Tap`] per
//! session it holds.
//!
//! The hub registers the process's sessions with the gateway over its
//! control socket (the registration's token, and both ends' uid checked)
//! and sends each session's events, as [`super::mirror`] numbers and keeps
//! them. The gateway asks it for snapshots and resumes, and fans its
//! events out to the devices granted each session. It connects only while
//! `remote.enabled` is on and a gateway is running, and tries again every
//! few seconds, so a gateway started later (by `/mobile`) or restarted
//! gets the sessions again; nothing is lost meanwhile, since the mirrors
//! keep their events.
//!
//! A tap tees the owner's own sink with [`crate::events::JsonEvents`]
//! ([`Tap::sink`]), and [`TapSlot`] wraps its asker and questioner so
//! permission requests and questions, and their answers, are events too.
//! Without the `remote` feature, or before the hub connects, every tap
//! works the same but nothing leaves the process.

use super::mirror::Mirror;
pub use super::mirror::{Info, Limits, Origin};
use crate::events::{JsonEvents, SharedChildren};
use crate::permission::{Asker, Reply, Request};
use crate::question::{Prompt, Questioner};
use crate::session::Session;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The kinds of owner, as `GET /v1/sessions` names them.
pub mod kind {
    pub const TUI: &str = "tui";
    pub const SERVE: &str = "serve";
    pub const ACP: &str = "acp";
}

/// A process's link to the gateway. Cheap to clone; an inert one (the
/// default) gives inert taps.
#[derive(Clone, Default)]
pub struct Hub(Option<Arc<HubInner>>);

struct HubInner {
    epoch: String,
    limits: Limits,
    mirrors: Mutex<HashMap<String, Arc<Mirror>>>,
    #[cfg(all(unix, feature = "remote"))]
    link: link::Link,
}

impl Hub {
    /// A hub that does nothing.
    pub fn inert() -> Hub {
        Hub(None)
    }

    /// The hub for an owner of `kind` ([`kind`]): it connects to the
    /// gateway whenever `remote.enabled` is on and one is running. Needs a
    /// tokio runtime; without one, or without the `remote` feature, inert.
    pub fn start(kind: &'static str) -> Hub {
        #[cfg(all(unix, feature = "remote"))]
        {
            if tokio::runtime::Handle::try_current().is_err() {
                return Hub::inert();
            }
            let inner = Arc::new(HubInner::new(Limits::default(), kind));
            link::spawn(&inner, link::Target::Configured);
            Hub(Some(inner))
        }
        #[cfg(not(all(unix, feature = "remote")))]
        {
            let _ = kind;
            Hub::inert()
        }
    }

    /// A hub that connects to the gateway in `dir` regardless of the
    /// settings, trying again every `poll` (tests).
    #[cfg(all(unix, feature = "remote"))]
    pub fn connect_to(
        kind: &'static str,
        dir: std::path::PathBuf,
        limits: Limits,
        poll: std::time::Duration,
    ) -> Hub {
        let inner = Arc::new(HubInner::new(limits, kind));
        link::spawn(&inner, link::Target::Dir(dir, poll));
        Hub(Some(inner))
    }

    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// This process's epoch (empty for an inert hub).
    pub fn epoch(&self) -> &str {
        self.0.as_ref().map_or("", |h| h.epoch.as_str())
    }

    /// The tap for session `s`, made the first time; an existing one gets
    /// its title, agent and model brought up to date.
    pub fn tap(&self, s: &Session) -> Tap {
        let info = info_of(s);
        let t = self.tap_for(&s.id, info.clone(), s.stored_seq());
        t.config(&info.agent, &info.model, info.effort.as_deref());
        t.title(info.title);
        t
    }

    /// The tap for session `id`, made with `info` and `history_seq` the
    /// first time.
    pub fn tap_for(&self, id: &str, info: Info, history_seq: i64) -> Tap {
        let Some(h) = &self.0 else {
            return Tap::default();
        };
        let mut map = h.mirrors.lock().unwrap();
        map.retain(|_, m| !m.is_released());
        let m = map
            .entry(id.to_string())
            .or_insert_with(|| {
                let m = Arc::new(Mirror::new(
                    id,
                    &h.epoch,
                    info,
                    history_seq,
                    h.limits.clone(),
                ));
                #[cfg(all(unix, feature = "remote"))]
                m.connect(h.link.outlet());
                m
            })
            .clone();
        Tap(Some(Arc::new(TapInner {
            mirror: m,
            children: SharedChildren::default(),
        })))
    }

    /// This process no longer holds session `id` (closed, deleted, or the
    /// UI moved to another): the gateway shows it as closed.
    pub fn release(&self, id: &str) {
        if let Some(h) = &self.0 {
            if let Some(m) = h.mirrors.lock().unwrap().remove(id) {
                m.release();
            }
        }
    }

    /// Wait (up to 10 s) until the hub is connected and the gateway has
    /// handled everything sent so far. For tests; false on a timeout.
    #[cfg(all(unix, feature = "remote"))]
    pub async fn sync(&self) -> bool {
        match &self.0 {
            Some(h) => h.link.sync().await,
            None => false,
        }
    }
}

#[cfg_attr(not(all(unix, feature = "remote")), allow(dead_code))]
impl HubInner {
    fn new(limits: Limits, kind: &'static str) -> HubInner {
        let _ = kind;
        HubInner {
            epoch: super::mirror::new_epoch(),
            limits,
            mirrors: Mutex::new(HashMap::new()),
            #[cfg(all(unix, feature = "remote"))]
            link: link::Link::new(kind),
        }
    }

    fn mirror(&self, id: &str) -> Option<Arc<Mirror>> {
        self.mirrors.lock().unwrap().get(id).cloned()
    }
}

/// A session's title, agent, model and directory, as a list shows them.
pub fn info_of(s: &Session) -> Info {
    Info {
        title: s.title.clone(),
        agent: s.agent.id.clone(),
        model: s.model_spec.clone(),
        directory: s.cwd.display().to_string(),
        effort: None,
    }
}

/// One session's events for the phones. Cheap to clone; the default does
/// nothing.
#[derive(Clone, Default)]
pub struct Tap(Option<Arc<TapInner>>);

struct TapInner {
    mirror: Arc<Mirror>,
    /// The subagents running, shared by [`Tap::sink`] and the prompts.
    children: SharedChildren,
}

impl Tap {
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// One event, in serve's JSON ([`crate::events`]).
    pub fn push(&self, ev: Value) {
        if let Some(t) = &self.0 {
            t.mirror.push(ev);
        }
    }

    /// A sink that sends this session's events; tee it with the owner's
    /// own ([`crate::events::Tee`]).
    pub fn sink(&self) -> JsonEvents<impl FnMut(Value) + Send> {
        let me = self.clone();
        let children = self
            .0
            .as_ref()
            .map(|t| t.children.clone())
            .unwrap_or_default();
        JsonEvents::new(children, move |v| me.push(v))
    }

    /// The running subagent, for a prompt it asks.
    fn child(&self) -> Option<String> {
        self.0
            .as_ref()
            .and_then(|t| t.children.lock().unwrap().current())
    }

    /// A turn starts (`turn_start`), with the prompt's text when it has
    /// one (`user_message`, exactly as typed or sent).
    pub fn turn_start(&self, origin: &Origin, text: Option<&str>) {
        if let Some(t) = &self.0 {
            *t.children.lock().unwrap() = Default::default();
            t.mirror.turn_start(origin, text);
        }
    }

    /// The turn ended with `s` given back: `done`, and the history the
    /// store has for it now.
    pub fn end(&self, s: &Session, error: Option<&str>) {
        self.end_with(s, error, json!({}));
    }

    /// [`Tap::end`] with more fields for `done` (serve's `resumed`).
    pub fn end_with(&self, s: &Session, error: Option<&str>, extra: Value) {
        if let Some(t) = &self.0 {
            let mut extra = extra;
            extra["usage"] = json!(s.usage);
            extra["title"] = json!(s.title);
            t.mirror.done(error, extra, s.stored_seq(), s.title.clone());
        }
    }

    /// The turn ended without its session back (it was stopped): `done`
    /// with `error`, history as it was. Nothing when it had ended.
    pub fn end_stopped(&self, error: &str) {
        if let Some(t) = &self.0 {
            if t.mirror.turn_running() {
                let seq = t.mirror.snapshot().history_seq;
                t.mirror.done(Some(error), json!({}), seq, None);
            }
        }
    }

    /// The session really is idle (C10).
    pub fn idle(&self) {
        if let Some(t) = &self.0 {
            t.mirror.idle();
        }
    }

    pub fn config(&self, agent: &str, model: &str, effort: Option<&str>) {
        if let Some(t) = &self.0 {
            t.mirror.config(agent, model, effort);
        }
    }

    pub fn title(&self, title: Option<String>) {
        if let Some(t) = &self.0 {
            t.mirror.title(title);
        }
    }

    /// The history changed outside a turn (an undo, a redo, a reload):
    /// clients reload it.
    pub fn history_changed(&self, s: &Session) {
        if let Some(t) = &self.0 {
            t.mirror.history_changed(s.stored_seq());
        }
    }

    /// Prompts waiting for a turn (`queued`): how many, and the next one's
    /// text.
    pub fn queued(&self, count: usize, next: Option<&str>) {
        self.push(json!({"type": "queued", "count": count, "next": next}));
    }

    /// The process lets the session go: the phones see it closed. A tap
    /// made for it later starts afresh.
    pub fn release(&self) {
        if let Some(t) = &self.0 {
            t.mirror.release();
        }
    }

    /// The mirror behind it (tests).
    pub fn mirror(&self) -> Option<Arc<Mirror>> {
        self.0.as_ref().map(|t| t.mirror.clone())
    }
}

/// Which session's tap an asker or questioner reports to: one per UI tab
/// or per session, set as the session it serves changes.
#[derive(Clone, Default)]
pub struct TapSlot(Arc<Mutex<Tap>>);

impl TapSlot {
    pub fn set(&self, tap: Tap) {
        *self.0.lock().unwrap() = tap;
    }

    pub fn get(&self) -> Tap {
        self.0.lock().unwrap().clone()
    }

    /// `inner`, with its requests and their answers sent as `permission`
    /// and `prompt_resolved` events.
    pub fn asker(&self, inner: Arc<dyn Asker>) -> Arc<dyn Asker> {
        Arc::new(SlotAsker {
            inner,
            slot: self.clone(),
        })
    }

    /// `inner`, with its questions and their answers sent as `question`
    /// and `prompt_resolved` events.
    pub fn questioner(&self, inner: Option<Arc<dyn Questioner>>) -> Option<Arc<dyn Questioner>> {
        inner.map(|inner| {
            Arc::new(SlotQuestioner {
                inner,
                slot: self.clone(),
            }) as Arc<dyn Questioner>
        })
    }
}

fn reply_name(r: &Reply) -> &'static str {
    match r {
        Reply::Once => "once",
        Reply::Always => "always",
        Reply::Session => "session",
        Reply::Reject(_) => "reject",
    }
}

/// Who answers a prompt in this process: its own UI or client.
fn answered_here() -> Value {
    json!({"kind": "terminal", "name": ""})
}

struct SlotAsker {
    inner: Arc<dyn Asker>,
    slot: TapSlot,
}

impl SlotAsker {
    fn asked(&self, req: &Request, answer: impl FnOnce() -> Reply) -> Reply {
        let tap = self.slot.get();
        if !tap.is_on() {
            return answer();
        }
        let id = format!("perm_{}", crate::paths::random_token(16));
        let mut ev = json!({"type": "permission", "id": id, "action": req.action,
            "resources": req.resources, "title": req.title, "save": req.save});
        if let Some(child) = tap.child() {
            ev["session"] = json!(child);
        }
        tap.push(ev);
        let r = answer();
        tap.push(
            json!({"type": "prompt_resolved", "id": id, "kind": "permission",
            "reply": reply_name(&r), "by": answered_here()}),
        );
        r
    }
}

impl Asker for SlotAsker {
    fn ask(&self, req: &Request) -> Reply {
        self.asked(req, || self.inner.ask(req))
    }
    fn ask_for(&self, req: &Request, by: &str) -> Reply {
        self.asked(req, || self.inner.ask_for(req, by))
    }
}

struct SlotQuestioner {
    inner: Arc<dyn Questioner>,
    slot: TapSlot,
}

impl Questioner for SlotQuestioner {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>> {
        let tap = self.slot.get();
        if !tap.is_on() {
            return self.inner.ask(prompts);
        }
        let id = format!("q_{}", crate::paths::random_token(16));
        tap.push(json!({"type": "question", "id": id, "prompts": prompts}));
        let a = self.inner.ask(prompts);
        tap.push(
            json!({"type": "prompt_resolved", "id": id, "kind": "question",
            "reply": a, "by": answered_here()}),
        );
        a
    }
    fn ask_form(&self, form: &crate::form::Info) -> Option<Option<crate::form::Answer>> {
        self.inner.ask_form(form)
    }
}

/// The hub's connection to the gateway.
#[cfg(all(unix, feature = "remote"))]
mod link {
    use super::super::control::{self, Frame, Request};
    use super::super::mirror::{Out, Outlet};
    use super::HubInner;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, Weak};
    use std::time::{Duration, Instant};
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{mpsc, oneshot, watch};

    pub enum Target {
        /// The gateway `remote.*` settings and the runtime folder name,
        /// while `remote.enabled` is on.
        Configured,
        /// This folder's, always; trying again this often (tests).
        Dir(PathBuf, Duration),
    }

    pub struct Link {
        kind: &'static str,
        tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
        synced: Mutex<HashMap<u64, oneshot::Sender<()>>>,
        next: AtomicU64,
        connected: watch::Sender<bool>,
    }

    impl Link {
        pub fn new(kind: &'static str) -> Link {
            Link {
                kind,
                tx: Mutex::new(None),
                synced: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                connected: watch::channel(false).0,
            }
        }

        /// Where mirrors send to while connected.
        pub fn outlet(&self) -> Option<Outlet> {
            let tx = self.tx.lock().unwrap().clone()?;
            Some(outlet(tx))
        }

        pub async fn sync(&self) -> bool {
            let mut c = self.connected.subscribe();
            let wait = async {
                loop {
                    if *c.borrow_and_update() {
                        let (otx, orx) = oneshot::channel();
                        let n = self.next.fetch_add(1, Ordering::Relaxed);
                        self.synced.lock().unwrap().insert(n, otx);
                        let sent =
                            self.tx.lock().unwrap().as_ref().is_some_and(|t| {
                                t.send(line(json!({"op": "sync", "req": n}))).is_ok()
                            });
                        if sent && orx.await.is_ok() {
                            return;
                        }
                    }
                    if c.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(10), wait)
                .await
                .is_ok()
        }
    }

    fn line(v: Value) -> String {
        let mut s = v.to_string();
        s.push('\n');
        s
    }

    /// What a mirror's [`Out`] is on the wire.
    pub fn out_line(session: &str, out: Out) -> String {
        line(match out {
            Out::Hold { info, busy } => {
                json!({"op": "hold", "session": session, "info": info, "busy": busy})
            }
            Out::Info { info, busy } => {
                json!({"op": "info", "session": session, "info": info, "busy": busy})
            }
            Out::Event(e) => json!({"op": "event", "session": session, "event": e}),
            Out::Release => json!({"op": "release", "session": session}),
        })
    }

    fn outlet(tx: mpsc::UnboundedSender<String>) -> Outlet {
        Arc::new(move |session: &str, out: Out| {
            let _ = tx.send(out_line(session, out));
        })
    }

    pub fn spawn(inner: &Arc<HubInner>, target: Target) {
        let weak = Arc::downgrade(inner);
        let ticker = weak.clone();
        tokio::spawn(async move {
            // Held deltas go out when their time is up.
            let mut t = tokio::time::interval(Duration::from_millis(25));
            loop {
                t.tick().await;
                let Some(h) = ticker.upgrade() else { return };
                let now = Instant::now();
                let ms: Vec<_> = h.mirrors.lock().unwrap().values().cloned().collect();
                for m in ms {
                    m.flush_due(now);
                }
            }
        });
        tokio::spawn(async move {
            loop {
                let (dir, poll) = match &target {
                    Target::Dir(d, p) => (Some(d.clone()), *p),
                    Target::Configured => (configured(), Duration::from_secs(2)),
                };
                if let Some(dir) = dir {
                    if weak.upgrade().is_none() {
                        return;
                    }
                    let _ = run(&weak, &dir).await;
                }
                if weak.upgrade().is_none() {
                    return;
                }
                tokio::time::sleep(poll).await;
            }
        });
    }

    /// The gateway's folder, when remote access is on and one runs there.
    fn configured() -> Option<PathBuf> {
        let dir = super::super::dir_path().ok()?;
        if !dir.join(super::super::REGISTRATION).exists() {
            return None;
        }
        super::super::settings::load()
            .ok()
            .filter(|s| s.enabled)
            .map(|_| dir)
    }

    /// One connection, until it ends.
    async fn run(weak: &Weak<HubInner>, dir: &std::path::Path) -> std::io::Result<()> {
        let kind = weak.upgrade().map(|h| h.link.kind).unwrap_or("");
        let mut conn = control::connect(
            dir,
            Request::Owner {
                kind: kind.to_string(),
                pid: std::process::id(),
            },
        )
        .await?;
        match tokio::time::timeout(Duration::from_secs(10), conn.next()).await {
            Ok(Ok(Some(Frame::Ok))) => {}
            _ => return Err(std::io::Error::other("the gateway refused the owner")),
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut writer = conn.writer;
        let writing = tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                if writer.write_all(l.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        {
            let Some(h) = weak.upgrade() else {
                return Ok(());
            };
            // Under the mirrors' lock, so a tap made meanwhile is either
            // connected here or finds the outlet set.
            let map = h.mirrors.lock().unwrap();
            *h.link.tx.lock().unwrap() = Some(tx.clone());
            let out = outlet(tx.clone());
            for m in map.values() {
                m.connect(Some(out.clone()));
            }
            drop(map);
            h.link.connected.send_replace(true);
        }
        let mut reader = conn.reader;
        loop {
            let l = match control::read_line(&mut reader).await {
                Ok(Some(l)) => l,
                _ => break,
            };
            let Ok(v) = serde_json::from_str::<Value>(&l) else {
                continue;
            };
            let Some(h) = weak.upgrade() else { break };
            handle(&h, &tx, &v);
        }
        writing.abort();
        if let Some(h) = weak.upgrade() {
            let map = h.mirrors.lock().unwrap();
            *h.link.tx.lock().unwrap() = None;
            for m in map.values() {
                m.connect(None);
            }
            drop(map);
            h.link.connected.send_replace(false);
            h.link.synced.lock().unwrap().clear();
        }
        Ok(())
    }

    /// One request from the gateway.
    fn handle(h: &HubInner, tx: &mpsc::UnboundedSender<String>, v: &Value) {
        let req = v["req"].clone();
        let session = v["session"].as_str().unwrap_or("");
        match v["op"].as_str().unwrap_or("") {
            "synced" => {
                if let Some(t) = v["req"]
                    .as_u64()
                    .and_then(|n| h.link.synced.lock().unwrap().remove(&n))
                {
                    let _ = t.send(());
                }
            }
            "snapshot" => {
                let body = match h.mirror(session) {
                    Some(m) => {
                        let s = m.snapshot();
                        json!({"op": "snapshot", "req": req, "snapshot": {
                            "epoch": s.epoch, "events_seq": s.events_seq, "turn": s.turn,
                            "busy": s.busy, "history_seq": s.history_seq, "info": s.info.json()}})
                    }
                    None => json!({"op": "snapshot", "req": req, "error": "not held"}),
                };
                let _ = tx.send(line(body));
            }
            "subscribe" => {
                let cursor = v["cursor"].as_str().and_then(|c| {
                    let (e, s) = c.rsplit_once(':')?;
                    Some((e.to_string(), s.parse::<u64>().ok()?))
                });
                match h.mirror(session) {
                    Some(m) => {
                        m.subscribe_then(cursor.as_ref().map(|(e, s)| (e.as_str(), *s)), |r| {
                            let _ = tx.send(line(json!({"op": "replay", "req": req,
                                "reset": r.reset, "events": r.events, "resend": r.resend})));
                        })
                    }
                    None => {
                        let _ = tx.send(line(
                            json!({"op": "replay", "req": req, "error": "not held"}),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::Fixed;

    fn on() -> Hub {
        Hub(Some(Arc::new(HubInner::new(
            Limits {
                merge_window: std::time::Duration::ZERO,
                ..Default::default()
            },
            kind::TUI,
        ))))
    }

    #[test]
    fn an_inert_hub_gives_inert_taps() {
        let t = Hub::inert().tap_for("ses_1", Info::default(), 0);
        assert!(!t.is_on());
        t.push(json!({"type": "text", "delta": "x"}));
        t.idle();
    }

    #[test]
    fn prompts_and_their_answers_are_events() {
        let hub = on();
        let tap = hub.tap_for("ses_1", Info::default(), 0);
        let slot = TapSlot::default();
        slot.set(tap.clone());
        let asker = slot.asker(Arc::new(Fixed(Reply::Session)));
        tap.turn_start(&Origin::terminal(), Some("x"));
        let req = Request {
            action: "edit".into(),
            resources: vec!["a.rs".into()],
            save: vec![],
            title: "Edit a.rs".into(),
            access: None,
        };
        assert_eq!(asker.ask(&req), Reply::Session);
        let r = tap.mirror().unwrap().subscribe(Some((hub.epoch(), 0)));
        let p = &r.events[2];
        assert_eq!(p["type"], "permission");
        assert_eq!(p["title"], "Edit a.rs");
        assert_eq!(p["resources"], json!(["a.rs"]));
        let done = &r.events[3];
        assert_eq!(done["type"], "prompt_resolved");
        assert_eq!(done["id"], p["id"], "S15: the same id");
        assert_eq!(done["reply"], "session");
        assert_eq!(done["kind"], "permission");
        // The same session gets the same mirror; release lets it go.
        let again = hub.tap_for("ses_1", Info::default(), 0);
        assert!(Arc::ptr_eq(
            &again.mirror().unwrap(),
            &tap.mirror().unwrap()
        ));
        hub.release("ses_1");
        let fresh = hub.tap_for("ses_1", Info::default(), 0);
        assert!(!Arc::ptr_eq(
            &fresh.mirror().unwrap(),
            &tap.mirror().unwrap()
        ));
    }

    #[test]
    fn a_slot_without_a_tap_just_asks() {
        let slot = TapSlot::default();
        let asker = slot.asker(Arc::new(Fixed(Reply::Once)));
        let req = Request {
            action: "x".into(),
            resources: vec![],
            save: vec![],
            title: String::new(),
            access: None,
        };
        assert_eq!(asker.ask(&req), Reply::Once);
    }
}
