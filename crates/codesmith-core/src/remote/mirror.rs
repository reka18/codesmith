//! One session's events, as its owner (the TUI, `codesmith serve` or ACP)
//! keeps them for the phones: the envelope and sequence numbers, the
//! resume buffer, the running turn, and the snapshot cursor. See
//! `docs/remote-protocol.md` (Events, Resuming) and requirements S4–S25.
//!
//! Everything happens under one lock: an event gets its seq, goes in the
//! buffer and is handed to the gateway ([`Mirror::connect`]) before the
//! next one, and a snapshot or a resume reads the buffer under the same
//! lock. So a snapshot's `events_seq` splits the stream exactly (S18), and
//! a resume's replay joins the live stream with nothing missing or twice.
//!
//! - **Envelope (S4, S17):** `{v, epoch, seq, session, turn, ts, type}`.
//!   `epoch` is random per owning process, `seq` rises per session from 1
//!   across turns, `turn` is the running turn's id (`trn_<epoch>_<n>`,
//!   unique across epochs, S25) or null. A subagent's events name it in
//!   `child` (serve's `session`).
//! - **Delivery (S8):** consecutive deltas of one block (`text`,
//!   `reasoning`, `child_text`, `child_reasoning`, and `tool_output` of one
//!   call) are held for up to [`Limits::merge_window`] and sent as one
//!   event, so each block streams at most 10 a second.
//! - **Size (S14):** no event is over [`Limits::event_bytes`]; a longer one
//!   has its longest text cut, with a marker, and `"truncated": true`.
//! - **The buffer (S5, S19, S20):** finished turns' events are kept up to
//!   2,000 events or 4 MiB. The running turn's are all kept from its
//!   `turn_start`, up to 8 MiB, with consecutive deltas of one block merged
//!   into one event that keeps the last seq and says `merged_from`. A
//!   resume inside a merged event gets the rest of it, from the next seq,
//!   so a client never sees a merged event that overlaps what it applied.
//!   Over 8 MiB, the oldest of the turn's events go, and a resume that
//!   needs them gets `reset` `turn_too_large`; a snapshot then carries the
//!   turn so far as `turn.partial`, with `partial_seq`.

use super::rebuild::Rebuild;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The envelope's version.
pub const VERSION: u64 = 1;

#[derive(Debug, Clone)]
pub struct Limits {
    /// Finished turns' events kept for resuming (S5).
    pub ring_events: usize,
    pub ring_bytes: usize,
    /// The running turn's events kept (S19).
    pub turn_bytes: usize,
    /// The most one event may be, serialised (S14).
    pub event_bytes: usize,
    /// How long consecutive deltas of one block are held to send as one
    /// (S8); zero sends each at once.
    pub merge_window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            ring_events: 2000,
            ring_bytes: 4 << 20,
            turn_bytes: 8 << 20,
            event_bytes: 1 << 20,
            merge_window: Duration::from_millis(100),
        }
    }
}

/// Why a resume can't be served, as `reset`'s `reason` (S23).
pub mod reason {
    /// The cursor is from another epoch: the owner restarted, or another
    /// process holds the session now.
    pub const EPOCH: &str = "epoch";
    /// The events after the cursor are older than the buffer.
    pub const GAP: &str = "gap";
    /// The running turn is over 8 MiB and the events the cursor needs are
    /// gone; a snapshot has `turn.partial`.
    pub const TURN_TOO_LARGE: &str = "turn_too_large";
    /// The session's history changed outside a turn (an undo, a
    /// compaction): reload it.
    pub const HISTORY: &str = "history";
}

/// What a session shows in a list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub title: Option<String>,
    pub agent: String,
    pub model: String,
    pub directory: String,
    pub effort: Option<String>,
}

impl Info {
    pub fn json(&self) -> Value {
        json!({"title": self.title, "agent": self.agent, "model": self.model,
               "directory": self.directory, "effort": self.effort})
    }
}

/// Who started a turn, as `turn_start.origin` carries it:
/// `{kind, name}`, `kind` being `terminal`, `device`, `bus` or `resumed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub kind: &'static str,
    pub name: String,
}

impl Origin {
    pub fn terminal() -> Origin {
        Origin {
            kind: "terminal",
            name: String::new(),
        }
    }
    pub fn resumed() -> Origin {
        Origin {
            kind: "resumed",
            name: String::new(),
        }
    }
    pub fn bus(from: &str) -> Origin {
        Origin {
            kind: "bus",
            name: from.to_string(),
        }
    }
    pub fn device(name: &str) -> Origin {
        Origin {
            kind: "device",
            name: name.to_string(),
        }
    }
    pub fn json(&self) -> Value {
        json!({"kind": self.kind, "name": self.name})
    }
}

/// What a mirror hands the gateway, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// The session, its info and whether it's busy, on (re)connecting.
    Hold { info: Value, busy: bool },
    /// Its info changed (title, agent, model, busy, history).
    Info { info: Value, busy: bool },
    /// An event, enveloped.
    Event(Value),
    /// The process no longer holds it.
    Release,
}

/// Where a connected mirror's [`Out`]s go.
pub type Outlet = Arc<dyn Fn(&str, Out) + Send + Sync>;

/// A snapshot for `GET /v1/sessions/{id}` (S12, S18, S20, S23).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub epoch: String,
    /// The last seq whose effect is in the snapshot: everything up to the
    /// last finished turn (whose messages are the store's up to
    /// `history_seq`); 0 before any event.
    pub events_seq: u64,
    /// `null` while idle; `{id, start_seq}`, and while the running turn is
    /// over 8 MiB also `partial` and `partial_seq`, while one runs.
    pub turn: Value,
    pub busy: bool,
    /// The session store's last record seq that the snapshot covers.
    pub history_seq: i64,
    pub info: Info,
}

/// What a resume gives a new subscriber, before it joins the live stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replay {
    /// It can't resume: one `reset` with this reason, and the stream ends.
    pub reset: Option<&'static str>,
    /// The events after its cursor, with their own seqs.
    pub events: Vec<Value>,
    /// The open permission requests and questions it hasn't been sent
    /// (S6): sent again after the replay, without an SSE id.
    pub resend: Vec<Value>,
}

/// One kept event.
#[derive(Debug, Clone)]
struct Stored {
    seq: u64,
    /// `merged_from`, or `seq` for one not merged.
    from: u64,
    body: Value,
    /// Its serialised size.
    bytes: usize,
    /// The block a delta belongs to, for merging.
    block: Option<String>,
    /// A merged event's parts: each original seq and where its delta
    /// starts in the merged one (bytes); empty when not merged.
    pieces: Vec<(u64, usize)>,
}

/// What a kept event costs the buffer: its size and its parts' offsets.
fn cost(e: &Stored) -> usize {
    e.bytes + e.pieces.len() * 16
}

#[derive(Debug)]
struct Turn {
    id: String,
    start_seq: u64,
    events: VecDeque<Stored>,
    bytes: usize,
    /// The newest seq dropped to stay under the cap.
    dropped: Option<u64>,
    rebuild: Rebuild,
}

/// A delta held for merging (S8), not numbered yet.
#[derive(Debug)]
struct Pending {
    block: String,
    ev: Value,
    since: Instant,
}

struct State {
    seq: u64,
    ring: VecDeque<Stored>,
    ring_bytes: usize,
    turn: Option<Turn>,
    /// Open permission requests and questions, by id, as sent.
    open: BTreeMap<String, Value>,
    busy: bool,
    history_seq: i64,
    info: Info,
    pending: Option<Pending>,
    out: Option<Outlet>,
    released: bool,
}

pub struct Mirror {
    session: String,
    epoch: String,
    limits: Limits,
    state: Mutex<State>,
}

static TURNS: AtomicU64 = AtomicU64::new(1);

/// A new epoch: 8 random bytes as hex.
pub fn new_epoch() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The number of bytes `v` serialises to.
pub fn json_len(v: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

fn str_len(s: &str) -> usize {
    json_len(&Value::String(s.to_string()))
}

fn digits(n: i64) -> usize {
    n.to_string().len()
}

/// The block a delta event belongs to, or `None` for any other event.
fn block_of(ev: &Value) -> Option<String> {
    let t = ev["type"].as_str()?;
    let child = ev["child"].as_str().unwrap_or("");
    match t {
        "text" | "reasoning" | "child_text" | "child_reasoning" => Some(format!("{t}\u{0}{child}")),
        "tool_output" => Some(format!("{t}\u{0}{child}\u{0}{}", ev["id"].as_str()?)),
        _ => None,
    }
    .filter(|_| ev["delta"].is_string())
}

/// The marker a cut text ends with (S14).
pub fn cut_marker(cut: usize) -> String {
    format!("\n[… {cut} more bytes not sent]")
}

/// Cut `ev` to at most `max` bytes serialised (S14): its longest string
/// loses its end, with [`cut_marker`], and the event says
/// `"truncated": true`.
pub fn fit(ev: &mut Value, max: usize) {
    for _ in 0..8 {
        let len = json_len(ev);
        if len <= max {
            return;
        }
        ev["truncated"] = json!(true);
        let Some(s) = longest(ev) else { return };
        let marker = cut_marker(s.len());
        // Escapes only make a string longer, so cutting this many bytes
        // of it is enough, or close enough for another round.
        let over = len + 32 + marker.len() + 24 - max;
        let mut keep = s.len().saturating_sub(over);
        while !s.is_char_boundary(keep) {
            keep -= 1;
        }
        let cut = s.len() - keep;
        s.truncate(keep);
        s.push_str(&cut_marker(cut));
    }
}

fn longest(v: &mut Value) -> Option<&mut String> {
    fn walk<'a>(v: &'a mut Value, best: &mut Option<&'a mut String>) {
        match v {
            Value::String(s) => {
                if best.as_ref().is_none_or(|b| s.len() > b.len()) {
                    *best = Some(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, best)),
            Value::Object(o) => o.values_mut().for_each(|x| walk(x, best)),
            _ => {}
        }
    }
    let mut best = None;
    walk(v, &mut best);
    best
}

impl Mirror {
    pub fn new(session: &str, epoch: &str, info: Info, history_seq: i64, limits: Limits) -> Mirror {
        Mirror {
            session: session.to_string(),
            epoch: epoch.to_string(),
            limits,
            state: Mutex::new(State {
                seq: 0,
                ring: VecDeque::new(),
                ring_bytes: 0,
                turn: None,
                open: BTreeMap::new(),
                busy: false,
                history_seq,
                info,
                pending: None,
                out: None,
                released: false,
            }),
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Hand every [`Out`] from now on to `out`, starting with a `Hold`
    /// (`None`: disconnected; events are still kept).
    pub fn connect(&self, out: Option<Outlet>) {
        let mut st = self.state.lock().unwrap();
        if st.released {
            return;
        }
        if let Some(o) = &out {
            o(
                &self.session,
                Out::Hold {
                    info: info_json(&st),
                    busy: st.busy,
                },
            );
        }
        st.out = out;
    }

    /// The process lets the session go (closed, or moved elsewhere).
    pub fn release(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(o) = st.out.take() {
            o(&self.session, Out::Release);
        }
        st.released = true;
    }

    fn send(&self, st: &State, out: Out) {
        if let Some(o) = &st.out {
            o(&self.session, out);
        }
    }

    /// An event from the session's sink or its prompts (S7, S13). Deltas
    /// may be held briefly to merge (S8); everything else goes at once.
    pub fn push(&self, ev: Value) {
        self.push_at(ev, Instant::now());
    }

    pub fn push_at(&self, mut ev: Value, now: Instant) {
        // The envelope's `session` is this one; a subagent's is `child`.
        if let Some(child) = ev.as_object_mut().and_then(|o| o.remove("session")) {
            if !child.is_null() {
                ev["child"] = child;
            }
        }
        let mut st = self.state.lock().unwrap();
        let block = block_of(&ev);
        if let (Some(b), Some(p)) = (&block, &mut st.pending) {
            let fits = json_len(&p.ev) + str_len(ev["delta"].as_str().unwrap_or(""))
                < self.limits.event_bytes.saturating_sub(512);
            if p.block == *b && now.duration_since(p.since) < self.limits.merge_window && fits {
                let d = ev["delta"].as_str().unwrap_or("").to_string();
                if let Some(Value::String(s)) = p.ev.get_mut("delta") {
                    s.push_str(&d);
                }
                return;
            }
        }
        self.flush_locked(&mut st);
        match block {
            Some(b) if !self.limits.merge_window.is_zero() => {
                st.pending = Some(Pending {
                    block: b,
                    ev,
                    since: now,
                })
            }
            _ => self.record(&mut st, ev),
        }
    }

    /// Send a held delta whose time is up (the owner calls this a few
    /// times a second).
    pub fn flush_due(&self, now: Instant) {
        let mut st = self.state.lock().unwrap();
        if st
            .pending
            .as_ref()
            .is_some_and(|p| now.duration_since(p.since) >= self.limits.merge_window)
        {
            self.flush_locked(&mut st);
        }
    }

    /// Send a held delta now.
    pub fn flush(&self) {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
    }

    fn flush_locked(&self, st: &mut State) {
        if let Some(p) = st.pending.take() {
            self.record(st, p.ev);
        }
    }

    /// A turn starts: `turn_start` with its origin (S7), then the
    /// prompt's text as `user_message` when it has one (prompts typed in
    /// the terminal too).
    pub fn turn_start(&self, origin: &Origin, text: Option<&str>) {
        {
            let mut st = self.state.lock().unwrap();
            self.flush_locked(&mut st);
            if st.turn.is_some() {
                // The last one never ended (its task was dropped).
                self.end_turn(&mut st);
            }
            let id = format!(
                "trn_{}_{}",
                self.epoch,
                TURNS.fetch_add(1, Ordering::Relaxed)
            );
            st.turn = Some(Turn {
                id,
                start_seq: st.seq + 1,
                events: VecDeque::new(),
                bytes: 0,
                dropped: None,
                rebuild: Rebuild::default(),
            });
            if !st.busy {
                st.busy = true;
                self.send(
                    &st,
                    Out::Info {
                        info: info_json(&st),
                        busy: true,
                    },
                );
            }
            self.record(
                &mut st,
                json!({"type": "turn_start", "origin": origin.json()}),
            );
        }
        if let Some(t) = text {
            self.push(json!({"type": "user_message", "text": t, "attachments": []}));
        }
    }

    /// The turn ended: `done` (with `error` null when clean), and the
    /// store's history now covers it up to `history_seq` (S18).
    pub fn done(&self, error: Option<&str>, extra: Value, history_seq: i64, title: Option<String>) {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
        let mut ev = json!({"type": "done", "error": error});
        if let Value::Object(o) = extra {
            for (k, v) in o {
                ev[k] = v;
            }
        }
        if st.turn.is_none() {
            // A done with no turn_start (an owner that started none):
            // number it as a turn of its own so it still ends one.
            let id = format!(
                "trn_{}_{}",
                self.epoch,
                TURNS.fetch_add(1, Ordering::Relaxed)
            );
            st.turn = Some(Turn {
                id,
                start_seq: st.seq + 1,
                events: VecDeque::new(),
                bytes: 0,
                dropped: None,
                rebuild: Rebuild::default(),
            });
        }
        self.record(&mut st, ev);
        st.history_seq = history_seq;
        if title.is_some() {
            st.info.title = title;
        }
        self.end_turn(&mut st);
        self.send(
            &st,
            Out::Info {
                info: info_json(&st),
                busy: st.busy,
            },
        );
    }

    /// The session really is idle now (C10): `session_status {busy: false}`.
    pub fn idle(&self) {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
        if st.turn.is_some() {
            self.end_turn(&mut st);
        }
        st.busy = false;
        st.open.clear();
        self.record(&mut st, json!({"type": "session_status", "busy": false}));
        self.send(
            &st,
            Out::Info {
                info: info_json(&st),
                busy: false,
            },
        );
    }

    /// The agent, model or effort changed: `config`, once per change.
    pub fn config(&self, agent: &str, model: &str, effort: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        if st.info.agent == agent && st.info.model == model && st.info.effort.as_deref() == effort {
            return;
        }
        self.flush_locked(&mut st);
        st.info.agent = agent.to_string();
        st.info.model = model.to_string();
        st.info.effort = effort.map(str::to_string);
        self.record(
            &mut st,
            json!({"type": "config", "agent": agent, "model": model, "effort": effort}),
        );
        self.send(
            &st,
            Out::Info {
                info: info_json(&st),
                busy: st.busy,
            },
        );
    }

    /// The title changed outside a turn's end.
    pub fn title(&self, title: Option<String>) {
        let mut st = self.state.lock().unwrap();
        if st.info.title == title {
            return;
        }
        st.info.title = title;
        self.send(
            &st,
            Out::Info {
                info: info_json(&st),
                busy: st.busy,
            },
        );
    }

    /// The history changed outside a turn (an undo, redo or compaction):
    /// a `reset` with reason `history`, numbered like any event, so a
    /// client following or resuming across it reloads.
    pub fn history_changed(&self, history_seq: i64) {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
        st.history_seq = history_seq;
        self.record(&mut st, json!({"type": "reset", "reason": reason::HISTORY}));
        self.send(
            &st,
            Out::Info {
                info: info_json(&st),
                busy: st.busy,
            },
        );
    }

    /// Number, envelope, keep and send one event.
    fn record(&self, st: &mut State, mut ev: Value) {
        st.seq += 1;
        let seq = st.seq;
        let turn = st.turn.as_ref().map(|t| t.id.clone());
        let ts = now_ms();
        let body = ev.as_object_mut().map(std::mem::take).unwrap_or_default();
        let mut env = serde_json::Map::new();
        env.insert("v".into(), json!(VERSION));
        env.insert("epoch".into(), json!(self.epoch));
        env.insert("seq".into(), json!(seq));
        env.insert("session".into(), json!(self.session));
        env.insert("turn".into(), json!(turn));
        env.insert("ts".into(), json!(ts));
        for (k, v) in body {
            if !matches!(
                k.as_str(),
                "v" | "epoch" | "seq" | "session" | "turn" | "ts"
            ) {
                env.insert(k, v);
            }
        }
        let mut ev = Value::Object(env);
        fit(&mut ev, self.limits.event_bytes);
        let kind = ev["type"].as_str().unwrap_or("").to_string();
        let id = ev["id"].as_str().map(str::to_string);
        match (kind.as_str(), id) {
            ("permission" | "question", Some(id)) => {
                st.open.insert(id, ev.clone());
            }
            ("prompt_resolved", Some(id)) => {
                st.open.remove(&id);
            }
            _ => {}
        }
        self.send(st, Out::Event(ev.clone()));
        let stored = Stored {
            seq,
            from: seq,
            bytes: json_len(&ev),
            block: block_of(&ev),
            pieces: vec![],
            body: ev,
        };
        match &mut st.turn {
            Some(t) => {
                t.rebuild.apply(&stored.body);
                self.keep_in_turn(t, stored);
            }
            None => self.keep_in_ring(st, stored),
        }
    }

    fn keep_in_turn(&self, t: &mut Turn, e: Stored) {
        let merge = match (t.events.back(), &e.block) {
            (Some(last), Some(b)) => {
                last.block.as_ref() == Some(b)
                    && last.bytes + e.bytes + 64 <= self.limits.event_bytes
            }
            _ => false,
        };
        if merge {
            let last = t.events.back_mut().unwrap();
            let before = cost(last);
            merge_into(last, &e);
            t.bytes = t.bytes - before + cost(last);
        } else {
            t.bytes += cost(&e);
            t.events.push_back(e);
        }
        while t.bytes > self.limits.turn_bytes {
            let Some(old) = t.events.pop_front() else {
                break;
            };
            t.bytes -= cost(&old);
            t.dropped = Some(old.seq);
        }
    }

    fn keep_in_ring(&self, st: &mut State, e: Stored) {
        st.ring_bytes += cost(&e);
        st.ring.push_back(e);
        while st.ring.len() > self.limits.ring_events || st.ring_bytes > self.limits.ring_bytes {
            let Some(old) = st.ring.pop_front() else {
                break;
            };
            st.ring_bytes -= cost(&old);
        }
    }

    /// The running turn ended: its events join the finished ones.
    fn end_turn(&self, st: &mut State) {
        let Some(t) = st.turn.take() else { return };
        if t.dropped.is_some() {
            // Its start is gone: the ring can't resume across it.
            st.ring.clear();
            st.ring_bytes = 0;
        }
        for e in t.events {
            self.keep_in_ring(st, e);
        }
    }

    /// The snapshot for a GET (S18, S20, S23), read under the lock that
    /// numbers events.
    pub fn snapshot(&self) -> Snapshot {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
        let (events_seq, turn) = match &st.turn {
            None => (st.seq, Value::Null),
            Some(t) => {
                let mut v = json!({"id": t.id, "start_seq": t.start_seq});
                if t.dropped.is_some() {
                    v["partial"] = serde_json::to_value(t.rebuild.messages()).unwrap_or_default();
                    v["partial_seq"] = json!(st.seq);
                }
                (t.start_seq - 1, v)
            }
        };
        Snapshot {
            epoch: self.epoch.clone(),
            events_seq,
            turn,
            busy: st.busy,
            history_seq: st.history_seq,
            info: st.info.clone(),
        }
    }

    /// What a subscriber resuming after `cursor` (`<epoch>:<seq>`, or
    /// none to start from now) gets before the live stream (S5, S6, S19,
    /// S20, S21, S25). Read under the lock that numbers events: the live
    /// stream goes on from the last event here.
    pub fn subscribe(&self, cursor: Option<(&str, u64)>) -> Replay {
        self.subscribe_then(cursor, |r| r.clone())
    }

    /// [`Mirror::subscribe`], with `then` run on the replay before the lock
    /// is let go: the owner queues it for the gateway there, so no event
    /// numbered after it can reach the gateway before it.
    pub fn subscribe_then<T>(
        &self,
        cursor: Option<(&str, u64)>,
        then: impl FnOnce(&Replay) -> T,
    ) -> T {
        let mut st = self.state.lock().unwrap();
        self.flush_locked(&mut st);
        let r = self.replay_locked(&st, cursor);
        then(&r)
    }

    fn replay_locked(&self, st: &State, cursor: Option<(&str, u64)>) -> Replay {
        let resend = |after: u64| -> Vec<Value> {
            st.open
                .values()
                .filter(|e| e["seq"].as_u64().unwrap_or(0) <= after)
                .cloned()
                .collect()
        };
        let Some((epoch, c)) = cursor else {
            return Replay {
                resend: resend(u64::MAX),
                ..Default::default()
            };
        };
        let reset = |r| Replay {
            reset: Some(r),
            ..Default::default()
        };
        if epoch != self.epoch {
            return reset(reason::EPOCH);
        }
        if c > st.seq {
            return reset(reason::GAP);
        }
        if c == st.seq {
            return Replay {
                resend: resend(c),
                ..Default::default()
            };
        }
        let need = c + 1;
        let turn_events = st.turn.iter().flat_map(|t| t.events.iter());
        let all: Vec<&Stored> = st.ring.iter().chain(turn_events).collect();
        let Some(first) = all.iter().position(|e| e.seq >= need) else {
            return reset(reason::GAP);
        };
        let e = all[first];
        // Whether the events from `need` on are all here: the first one
        // starts at or before it, and the running turn lost none of those
        // after it.
        let in_turn = first >= st.ring.len();
        let dropped = st.turn.as_ref().and_then(|t| t.dropped);
        if e.from > need {
            return reset(if in_turn && dropped.is_some() {
                reason::TURN_TOO_LARGE
            } else {
                reason::GAP
            });
        }
        if !in_turn && dropped.is_some() {
            return reset(reason::TURN_TOO_LARGE);
        }
        let mut events = Vec::with_capacity(all.len() - first);
        events.push(slice_from(e, need));
        for e in &all[first + 1..] {
            events.push(e.body.clone());
        }
        Replay {
            reset: None,
            events,
            resend: resend(c),
        }
    }

    /// The last seq given out (tests).
    pub fn last_seq(&self) -> u64 {
        self.state.lock().unwrap().seq
    }

    /// What the buffer holds, in bytes (tests).
    pub fn kept(&self) -> (usize, usize, Option<usize>) {
        let st = self.state.lock().unwrap();
        (
            st.ring.len(),
            st.ring_bytes,
            st.turn.as_ref().map(|t| t.bytes),
        )
    }
}

fn info_json(st: &State) -> Value {
    let mut v = st.info.json();
    v["history_seq"] = json!(st.history_seq);
    v
}

/// Merge the delta `e` into `last` (consecutive, one block): the last
/// seq and ts, `merged_from` the first.
fn merge_into(last: &mut Stored, e: &Stored) {
    let delta = e.body["delta"].as_str().unwrap_or("");
    let offset = last.body["delta"].as_str().map_or(0, str::len);
    if last.pieces.is_empty() {
        last.pieces.push((last.seq, 0));
        last.body["merged_from"] = json!(last.seq);
        last.bytes += r#","merged_from":"#.len() + digits(last.seq as i64);
    }
    last.pieces.push((e.seq, offset));
    let (old_seq, old_ts) = (
        last.body["seq"].as_i64().unwrap_or(0),
        last.body["ts"].as_i64().unwrap_or(0),
    );
    let new_ts = e.body["ts"].as_i64().unwrap_or(0);
    if let Some(Value::String(s)) = last.body.get_mut("delta") {
        s.push_str(delta);
    }
    last.body["seq"] = json!(e.seq);
    last.body["ts"] = json!(new_ts);
    last.bytes = last.bytes + (str_len(delta) - 2) + digits(e.seq as i64) + digits(new_ts)
        - digits(old_seq)
        - digits(old_ts);
    last.seq = e.seq;
}

/// `e` as a subscriber that has applied everything before `need` gets
/// it: whole, or for a merged event begun earlier, the rest of it from
/// `need`'s part on.
fn slice_from(e: &Stored, need: u64) -> Value {
    if e.from >= need {
        return e.body.clone();
    }
    let at = e
        .pieces
        .iter()
        .position(|(s, _)| *s >= need)
        .expect("a merged event covers the seqs up to its own");
    let (from, offset) = e.pieces[at];
    let mut v = e.body.clone();
    let rest = e.body["delta"].as_str().unwrap_or("")[offset..].to_string();
    v["delta"] = json!(rest);
    if from == e.seq {
        if let Some(o) = v.as_object_mut() {
            o.remove("merged_from");
        }
    } else {
        v["merged_from"] = json!(from);
    }
    v
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod tests;
