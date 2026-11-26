//! The running sessions (OpenCode's `ClaudeSessions`): one `claude` per
//! CodeSmith session, kept between requests, and a throwaway one for each
//! side call (a title, a summary, a compaction).
//!
//! A process stops after [`Timing::idle`] without requests, except while
//! a turn waits for tool results: subagents, permission prompts and
//! questions can take longer than that, and stopping then would record the
//! calls as refused. Once CodeSmith stops running the session
//! ([`Registry::idle`]) the timer starts.
//!
//! The pool ([`Pool`]) bounds and warms these processes:
//!
//! - **One session per process.** A process is bound to one session's
//!   entry when it starts (or when a spare is claimed) and leaves it only
//!   by stopping. Nothing moves a bound process to another entry or back
//!   to the spares, so a process that carried a conversation never serves
//!   another session. A subagent's child session has an entry, and so a
//!   process, of its own.
//! - **Spares.** Claude Code takes its system prompt and tools in its
//!   start-up request, so a spare is started with a launch (system prompt,
//!   tools, folder, connection, output limit) and only a session starting
//!   a new Claude session with the same launch may claim it; the model and
//!   effort are set on it as on any running process. A spare never had a
//!   turn, and on claim it leaves the pool for good.
//! - **The cap.** Every process holds a slot until its child is reaped,
//!   and at most [`Pool::max_processes`] slots are taken. To make room a
//!   spare is stopped first, then the least recently used idle session's
//!   process (as its idle timer would stop it: it resumes next time). A
//!   running or parked turn is never stopped for room: the request waits
//!   for one to finish, failing after [`Timing::queue`].
//! - **Prewarm** starts a session's process as its turn begins, exactly as
//!   the turn would (resuming its Claude session or starting a new one);
//!   the turn still decides everything about the history, and stops a
//!   prewarmed process it cannot use.

use super::catalog::{self, Catalog, State};
use super::history::{self, Decision, Msg, ReplyPart};
use super::probe::{self, Probe};
use super::process::Handler;
use super::query::{Launch, System};
use super::session::{Session, Step};
use super::store::{Record, Records};
use super::tools::{self, Outcome, Pending};
use super::{Io, Pool, Timing};
use crate::ToolSpec;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// The error of a request that was cancelled.
pub const CANCELLED: &str = "cancelled";

/// What one model request asks of Claude Code.
#[derive(Debug, Clone)]
pub struct Request {
    /// The CodeSmith session; only chat steps have one. Side calls run in a
    /// throwaway process.
    pub session_id: Option<String>,
    /// The session a fork or a side call copies its history from.
    pub source_id: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub max_output: Option<u32>,
    pub connection: BTreeMap<String, String>,
    pub directory: Option<PathBuf>,
    pub system: System,
    pub prompt: Vec<Msg>,
    pub tools: Vec<ToolSpec>,
    /// Tool calls that were stopped rather than run.
    pub aborted: HashSet<String>,
}

impl Request {
    /// What needs a new process to change. Model and effort change on the
    /// running one.
    fn launch(&self) -> String {
        let system = match &self.system {
            System::Preset(s) => json!(["preset", s]),
            System::Custom(s) => json!(["custom", s]),
        };
        json!([
            self.max_output,
            self.connection,
            system,
            self.tools
                .iter()
                .map(|t| json!([t.name, t.description, t.parameters]))
                .collect::<Vec<_>>(),
            self.directory,
        ])
        .to_string()
    }
}

struct Live {
    session: Arc<Session>,
    pending: Arc<Pending>,
    model: String,
    effort: Option<String>,
    launch: String,
    /// The calls a parked turn waits on.
    waiting: Option<Vec<String>>,
    /// Started for a new Claude session rather than resuming one.
    fresh: bool,
    /// Its start-up report (the model list) was recorded.
    reported: bool,
    /// Bound by this request (started or claimed): when the request ends a
    /// spare takes its place, so the spare's start never competes with
    /// this turn's.
    refill: bool,
}

/// A never-used process, waiting for a new session launched like it.
struct Spare {
    id: u64,
    live: Live,
}

/// The slots processes hold (see [`Pool::max_processes`]).
struct Slots {
    taken: Mutex<usize>,
    /// Bumped whenever a slot is given back or a process becomes idle. Bump
    /// for an idle process only after unlocking its session: a waiter woken
    /// sooner cannot stop it, and no other wake-up may come.
    changed: watch::Sender<u64>,
}

impl Slots {
    fn bump(&self) {
        self.changed.send_modify(|g| *g = g.wrapping_add(1));
    }
}

/// One process's slot, given back when its child has been reaped.
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.taken.lock().unwrap() -= 1;
        self.0.bump();
    }
}

#[derive(Default)]
struct EntryState {
    process: Option<Live>,
    record: Option<Record>,
}

#[derive(Default)]
struct Entry {
    state: tokio::sync::Mutex<EntryState>,
    /// Bumped to disarm the idle timer.
    idle: AtomicU64,
    /// Cancels the running request.
    active: Mutex<Option<watch::Sender<bool>>>,
    /// When it was last used (the registry's clock), for eviction.
    used: AtomicU64,
}

struct Continuation {
    end: usize,
    results: Vec<(String, Outcome)>,
}

struct Inner {
    io: Io,
    timing: Timing,
    records: Records,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    temporary: Mutex<Vec<Arc<Entry>>>,
    status: watch::Sender<State>,
    checking: tokio::sync::Mutex<()>,
    checked: Mutex<HashSet<String>>,
    pool: Mutex<Pool>,
    slots: Arc<Slots>,
    /// Oldest first.
    spares: Mutex<Vec<Spare>>,
    filling: tokio::sync::Mutex<()>,
    clock: AtomicU64,
    /// Prewarms and spare fills still running.
    warming: watch::Sender<usize>,
    /// Processes started, for tests.
    started: AtomicUsize,
    /// Processes prewarming bound, for tests.
    prewarmed: AtomicUsize,
    /// Requests waiting for a slot.
    queued: watch::Sender<usize>,
}

/// The registry of running sessions.
#[derive(Clone)]
pub struct Registry(Arc<Inner>);

/// Both a request's own cancel and the registry closing it.
struct Cancel {
    own: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
}

impl Cancel {
    fn is_set(&self) -> bool {
        *self.own.borrow() || *self.closed.borrow()
    }

    async fn wait(&mut self) {
        loop {
            if self.is_set() {
                return;
            }
            tokio::select! {
                r = self.own.changed() => if r.is_err() { std::future::pending::<()>().await },
                r = self.closed.changed() => if r.is_err() { std::future::pending::<()>().await },
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Warm {
    Bound,
    Skip,
    /// No slot is free.
    Room,
}

/// Counts something (a background task, a queued request) while it lasts.
struct Counted(watch::Sender<usize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

async fn stop(st: &mut EntryState) {
    if let Some(live) = st.process.take() {
        live.session.stop().await;
        live.pending.clear();
    }
}

async fn interrupt(st: &mut EntryState) -> Result<(), String> {
    if let Some(live) = st.process.as_mut() {
        live.session.interrupt().await?;
        live.pending.clear();
        live.waiting = None;
    }
    Ok(())
}

/// Answers Claude Code's hook callbacks and the tool server's messages.
struct ChatHandler {
    pending: Arc<Pending>,
    tools: Vec<ToolSpec>,
    cancelled: Mutex<HashSet<String>>,
}

fn mcp(id: &Value, result: Value) -> Value {
    json!({"mcp_response": {"jsonrpc": "2.0", "id": id, "result": result}})
}

fn mcp_error(id: &Value, code: i64, message: String) -> Value {
    json!({"mcp_response": {"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}})
}

impl Handler for Arc<ChatHandler> {
    fn handle(
        &self,
        request: Value,
    ) -> futures_util::future::BoxFuture<'static, Option<Result<Value, String>>> {
        let me = self.clone();
        Box::pin(async move { me.answer(request).await })
    }
}

impl ChatHandler {
    async fn answer(&self, request: Value) -> Option<Result<Value, String>> {
        match request["subtype"].as_str() {
            Some("hook_callback") => {
                let input = &request["input"];
                let id = input["tool_use_id"]
                    .as_str()
                    .or(request["tool_use_id"].as_str())
                    .unwrap_or_default();
                let name = input["tool_name"].as_str().unwrap_or_default();
                Some(match input["hook_event_name"].as_str() {
                    Some("PreToolUse") if name.starts_with(tools::PREFIX) => {
                        self.pending.before(id, name).map(|_| json!({}))
                    }
                    Some("PostToolUse") => Ok(match self.pending.after(id) {
                        Some(content) => json!({"hookSpecificOutput": {
                            "hookEventName": "PostToolUse",
                            "updatedMCPToolOutput": content,
                        }}),
                        None => json!({}),
                    }),
                    _ => Ok(json!({})),
                })
            }
            Some("mcp_message") => {
                if request["server_name"] != tools::SERVER {
                    return Some(Err(format!(
                        "Unknown MCP server {}",
                        request["server_name"]
                    )));
                }
                let m = &request["message"];
                let id = m["id"].clone();
                let method = m["method"].as_str().unwrap_or_default();
                if id.is_null() {
                    // A notification. A cancelled call is never answered.
                    if method == "notifications/cancelled" {
                        let call = m["params"]["requestId"].to_string();
                        self.cancelled.lock().unwrap().insert(call);
                    }
                    return Some(Ok(
                        json!({"mcp_response": {"jsonrpc": "2.0", "result": {}, "id": 0}}),
                    ));
                }
                Some(Ok(match method {
                    "initialize" => mcp(&id, tools::initialize_result()),
                    "tools/list" => mcp(&id, json!({"tools": tools::definitions(&self.tools)})),
                    "tools/call" => {
                        let name = format!(
                            "{}{}",
                            tools::PREFIX,
                            m["params"]["name"].as_str().unwrap_or_default()
                        );
                        if tools::original(&name, &self.tools).is_none() {
                            mcp_error(
                                &id,
                                -32602,
                                format!(
                                    "Claude Code called an unregistered CodeSmith tool: {name}"
                                ),
                            )
                        } else {
                            let result = self.pending.call(&name).await;
                            if self.cancelled.lock().unwrap().remove(&id.to_string()) {
                                return None;
                            }
                            match result {
                                Ok(r) => mcp(&id, r),
                                Err(e) => mcp_error(&id, -32603, e),
                            }
                        }
                    }
                    _ => mcp_error(&id, -32601, format!("Method not found: {method}")),
                }))
            }
            // Claude Code asks nothing with `dontAsk`; refuse if it does.
            Some("can_use_tool") => Some(Ok(json!({
                "behavior": "deny",
                "message": "Only CodeSmith's tools may run, and CodeSmith checks them itself.",
            }))),
            other => Some(Err(format!(
                "Unsupported control request {}",
                other.unwrap_or("?")
            ))),
        }
    }
}

/// The reply as it arrives, for the checkpoints.
#[derive(Default)]
struct Reply {
    parts: Vec<ReplyPart>,
    tool_messages: HashMap<String, String>,
    seen: HashSet<String>,
    message_id: Option<String>,
    waiting: Vec<String>,
}

impl Registry {
    pub fn new(io: Io, timing: Timing) -> Registry {
        let (status, _) = watch::channel(State::Checking);
        Registry(Arc::new(Inner {
            records: Records::new(&io.storage),
            io,
            timing,
            entries: Mutex::default(),
            temporary: Mutex::default(),
            status,
            checking: tokio::sync::Mutex::new(()),
            checked: Mutex::default(),
            pool: Mutex::new(Pool::default()),
            slots: Arc::new(Slots {
                taken: Mutex::new(0),
                changed: watch::channel(0).0,
            }),
            spares: Mutex::default(),
            filling: tokio::sync::Mutex::new(()),
            clock: AtomicU64::new(0),
            warming: watch::channel(0).0,
            started: AtomicUsize::new(0),
            prewarmed: AtomicUsize::new(0),
            queued: watch::channel(0).0,
        }))
    }

    /// A registry with these pool settings.
    pub fn with_pool(io: Io, timing: Timing, pool: Pool) -> Registry {
        let r = Registry::new(io, timing);
        r.set_pool(pool);
        r
    }

    /// The pool settings in force.
    pub fn pool(&self) -> Pool {
        *self.0.pool.lock().unwrap()
    }

    /// Change the pool settings (clamped, see [`Pool::clamped`]). Running
    /// processes are not stopped for a lower cap: the next process to
    /// start makes room. Spares over the new count are stopped.
    pub fn set_pool(&self, pool: Pool) {
        let pool = pool.clamped();
        let surplus: Vec<Spare> = {
            let mut current = self.0.pool.lock().unwrap();
            if *current == pool {
                return;
            }
            *current = pool;
            let mut spares = self.0.spares.lock().unwrap();
            let n = spares.len().saturating_sub(pool.spare);
            spares.drain(..n).collect()
        };
        if !surplus.is_empty() {
            self.background(async move {
                for s in surplus {
                    s.live.session.stop().await;
                }
            });
        }
        self.0.slots.bump();
    }

    /// Live `claude` processes: every one that holds a slot (sessions',
    /// spares, side calls and login checks), until it is reaped.
    pub fn live(&self) -> usize {
        *self.0.slots.taken.lock().unwrap()
    }

    /// Wait until the live count satisfies `f`.
    pub async fn live_until(&self, f: impl Fn(usize) -> bool) {
        let mut changed = self.0.slots.changed.subscribe();
        loop {
            changed.mark_unchanged();
            if f(self.live()) {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    /// How many processes prewarming bound to a session.
    pub fn prewarmed(&self) -> usize {
        self.0.prewarmed.load(Ordering::SeqCst)
    }

    /// How many `claude` processes this registry has started.
    pub fn started(&self) -> usize {
        self.0.started.load(Ordering::SeqCst)
    }

    /// How many requests wait for a slot.
    pub fn queued(&self) -> watch::Receiver<usize> {
        self.0.queued.subscribe()
    }

    /// Spares waiting to be claimed.
    pub fn spares(&self) -> usize {
        self.0.spares.lock().unwrap().len()
    }

    /// The process id of each session's process.
    pub async fn pids(&self) -> BTreeMap<String, u32> {
        let all: Vec<(String, Arc<Entry>)> = self
            .0
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        let mut out = BTreeMap::new();
        for (id, e) in all {
            let st = e.state.lock().await;
            if let Some(pid) = st.process.as_ref().and_then(|p| p.session.pid()) {
                out.insert(id, pid);
            }
        }
        out
    }

    /// Wait until prewarms and spare fills finished and every process
    /// they started answered its start-up request.
    pub async fn settled(&self) {
        let mut rx = self.0.warming.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
        let spares: Vec<Arc<Session>> = self
            .0
            .spares
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.live.session.clone())
            .collect();
        let entries: Vec<Arc<Entry>> = self.0.entries.lock().unwrap().values().cloned().collect();
        let mut sessions = spares;
        for e in entries {
            if let Ok(st) = e.state.try_lock() {
                if let Some(p) = &st.process {
                    sessions.push(p.session.clone());
                }
            }
        }
        for s in sessions {
            let _ = tokio::time::timeout(self.0.timing.startup, s.ready()).await;
        }
    }

    fn background(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        self.0.warming.send_modify(|n| *n += 1);
        let counted = Counted(self.0.warming.clone());
        tokio::spawn(async move {
            let _counted = counted;
            f.await;
        });
    }

    fn tick(&self) -> u64 {
        self.0.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// A slot, if one is free now.
    fn try_slot(&self) -> Option<Slot> {
        let max = self.pool().max_processes;
        let mut taken = self.0.slots.taken.lock().unwrap();
        if *taken >= max {
            return None;
        }
        *taken += 1;
        Some(Slot(self.0.slots.clone()))
    }

    /// A slot now, making room if needed, or none.
    async fn slot_now(&self) -> Option<Slot> {
        loop {
            if let Some(s) = self.try_slot() {
                return Some(s);
            }
            if !self.make_room().await {
                return None;
            }
        }
    }

    /// A slot, making room if needed, else waiting for a busy process to
    /// finish (or be cancelled), up to [`Timing::queue`].
    async fn slot(&self, mut cancel: Option<&mut Cancel>) -> Result<Slot, String> {
        let deadline = tokio::time::Instant::now() + self.0.timing.queue;
        loop {
            let mut changed = self.0.slots.changed.subscribe();
            changed.mark_unchanged();
            if let Some(s) = self.slot_now().await {
                return Ok(s);
            }
            let cancelled = async {
                match cancel.as_mut() {
                    Some(c) => c.wait().await,
                    None => std::future::pending().await,
                }
            };
            self.0.queued.send_modify(|n| *n += 1);
            let _queued = Counted(self.0.queued.clone());
            tokio::select! {
                _ = changed.changed() => {}
                _ = cancelled => return Err(CANCELLED.into()),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(format!(
                        "CodeSmith is already running {} Claude Code processes (claude_agent.max_processes) and none of them is idle",
                        self.pool().max_processes
                    ));
                }
            }
        }
    }

    /// Stop one process to make room: a spare first (it never carried a
    /// conversation), else the least recently used idle session's, as its
    /// idle timer would. False when every process is busy.
    async fn make_room(&self) -> bool {
        let spare = {
            let mut spares = self.0.spares.lock().unwrap();
            (!spares.is_empty()).then(|| spares.remove(0))
        };
        if let Some(s) = spare {
            s.live.session.stop().await;
            return true;
        }
        let mut all: Vec<(u64, Arc<Entry>)> = self
            .0
            .entries
            .lock()
            .unwrap()
            .values()
            .map(|e| (e.used.load(Ordering::SeqCst), e.clone()))
            .collect();
        all.sort_by_key(|(used, _)| *used);
        for (_, e) in all {
            if e.active.lock().unwrap().is_some() {
                continue;
            }
            // A request holds the lock for as long as it runs.
            let Ok(mut st) = e.state.try_lock() else {
                continue;
            };
            let idle = st
                .process
                .as_ref()
                .is_some_and(|p| p.waiting.is_none() && !p.session.running());
            if !idle {
                continue;
            }
            e.idle.fetch_add(1, Ordering::SeqCst);
            stop(&mut st).await;
            return true;
        }
        false
    }

    /// Start a process for `request`'s entry: resuming `resume`, else a new
    /// Claude session.
    fn spawn(
        &self,
        request: &Request,
        mut launch: Launch,
        resume: Option<String>,
        slot: Slot,
    ) -> Result<Live, String> {
        let pending = Arc::new(Pending::default());
        launch.directory = request.directory.clone();
        launch.model = Some(request.model.clone());
        launch.effort = request.effort.clone();
        launch.max_output = request.max_output;
        launch.system = Some(request.system.clone());
        launch.tools = request.tools.clone();
        launch.chat = true;
        launch.persist = request.session_id.is_some();
        launch.session_id = resume.is_none().then(super::uuid);
        let fresh = resume.is_none();
        launch.resume = resume;
        let handler = Arc::new(ChatHandler {
            pending: pending.clone(),
            tools: request.tools.clone(),
            cancelled: Mutex::default(),
        });
        let session = Session::start(&launch, Arc::new(handler), Some(Box::new(slot)))?;
        self.0.started.fetch_add(1, Ordering::SeqCst);
        Ok(Live {
            session: Arc::new(session),
            pending,
            model: request.model.clone(),
            effort: request.effort.clone(),
            launch: request.launch(),
            waiting: None,
            fresh,
            reported: false,
            refill: true,
        })
    }

    /// Take a spare launched like `request`, binding it to the caller.
    fn claim(&self, request: &Request) -> Option<Live> {
        request.session_id.as_ref()?;
        let launch = request.launch();
        let mut spares = self.0.spares.lock().unwrap();
        spares.retain(|s| !s.live.session.ended());
        let i = spares.iter().position(|s| s.live.launch == launch)?;
        let mut live = spares.remove(i).live;
        debug_assert!(!live.session.used());
        live.refill = true;
        Some(live)
    }

    /// Keep a spare launched like `request` (a session's request) for the
    /// next new session, while there are fewer than [`Pool::spare`], in a
    /// free slot only: a spare never stops a session's process for room,
    /// nor another spare (which would only trade one launch for another,
    /// as a subagent's would each time).
    async fn fill(&self, request: &Request) {
        let pool = self.pool();
        if pool.spare == 0 || request.session_id.is_none() {
            return;
        }
        let _one = self.0.filling.lock().await;
        {
            let mut spares = self.0.spares.lock().unwrap();
            spares.retain(|s| !s.live.session.ended());
            if spares.len() >= pool.spare {
                return;
            }
        }
        let Some(base) = self.launch_base(&request.connection) else {
            return;
        };
        let Some(slot) = self.try_slot() else {
            return;
        };
        let Ok(mut live) = self.spawn(request, base, None, slot) else {
            return;
        };
        live.refill = false;
        let id = self.tick();
        self.0.spares.lock().unwrap().push(Spare { id, live });
        // An unclaimed spare stops after the idle time, as a session's does.
        let me = self.clone();
        let idle = self.0.timing.idle;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            let spare = {
                let mut spares = me.0.spares.lock().unwrap();
                spares
                    .iter()
                    .position(|s| s.id == id)
                    .map(|i| spares.remove(i))
            };
            if let Some(s) = spare {
                s.live.session.stop().await;
            }
        });
    }

    fn fill_later(&self, request: &Request) {
        if self.pool().spare == 0 || request.session_id.is_none() {
            return;
        }
        let me = self.clone();
        let request = request.clone();
        self.background(async move { me.fill(&request).await });
    }

    /// Start the process `request`'s session will use, in the background,
    /// so its turn does not wait for Claude Code to start. It binds to that
    /// session at once, resuming its Claude session (if Claude Code still
    /// has it) or starting a new one, from a spare when one matches. Does
    /// nothing while the session runs a request or has a process, for a
    /// fork's first request (which branches its source), or when no slot
    /// can be had without waiting (after stopping a spare or an idle
    /// session's process, as a request would). The turn decides about the
    /// history as it always does, and stops a prewarmed process it cannot
    /// use.
    pub fn prewarm(&self, request: Request) {
        if !self.pool().prewarm || request.session_id.is_none() {
            return;
        }
        // Bound before this returns when a slot is free; making room (which
        // stops another process) goes on in the background.
        if self.warm(&request, false) == Warm::Room {
            let me = self.clone();
            self.background(async move {
                if me.make_room().await {
                    me.warm(&request, true);
                }
            });
        }
    }

    /// Bind a process to `request`'s session, if it should have one now;
    /// [`Warm::Room`] when it should but no slot is free.
    fn warm(&self, request: &Request, again: bool) -> Warm {
        let Some(session_id) = &request.session_id else {
            return Warm::Skip;
        };
        let entry = self.entry_for(request);
        if entry.active.lock().unwrap().is_some() {
            return Warm::Skip;
        }
        let Ok(mut st) = entry.state.try_lock() else {
            return Warm::Skip;
        };
        if st.process.is_some() {
            return Warm::Skip;
        }
        if st.record.is_none() {
            st.record = self.0.records.get(session_id);
        }
        let resume = match &st.record {
            Some(r) => {
                let saved = r.history.as_ref().is_some_and(|h| !h.is_empty());
                if !saved || !self.0.io.transcripts().exists(&r.claude_session_id) {
                    return Warm::Skip;
                }
                Some(r.claude_session_id.clone())
            }
            None if request.source_id.is_some() => return Warm::Skip,
            None => None,
        };
        let live = match resume.is_none().then(|| self.claim(request)).flatten() {
            Some(live) => live,
            None => {
                let Some(base) = self.launch_base(&request.connection) else {
                    return Warm::Skip;
                };
                let Some(slot) = self.try_slot() else {
                    return if again { Warm::Skip } else { Warm::Room };
                };
                match self.spawn(request, base, resume, slot) {
                    Ok(live) => live,
                    Err(_) => return Warm::Skip,
                }
            }
        };
        st.process = Some(live);
        entry.used.store(self.tick(), Ordering::SeqCst);
        drop(st);
        self.0.prewarmed.fetch_add(1, Ordering::SeqCst);
        self.arm_idle(&entry);
        self.0.slots.bump();
        Warm::Bound
    }

    pub fn io(&self) -> &Io {
        &self.0.io
    }

    pub fn records(&self) -> &Records {
        &self.0.records
    }

    /// Claude Code's status, as last checked or reported.
    pub fn status(&self) -> watch::Receiver<State> {
        self.0.status.subscribe()
    }

    /// The saved model list.
    pub fn catalog(&self) -> Catalog {
        Catalog::load(&self.0.io.storage)
    }

    fn launch_base(&self, connection: &BTreeMap<String, String>) -> Option<Launch> {
        let executable = (self.0.io.which)()?;
        Some(Launch {
            executable,
            env: (self.0.io.env)(),
            connection: connection.clone(),
            ..Default::default()
        })
    }

    /// Check Claude Code now (the login check): its login and models,
    /// which are saved. Checks run one at a time, so a reconnect checks
    /// again after a check already running.
    pub async fn refresh(&self, connection: &BTreeMap<String, String>) -> State {
        let _one = self.0.checking.lock().await;
        let state = match self.launch_base(connection) {
            None => State::Missing,
            Some(launch) => match self.slot(None).await {
                Ok(slot) => self.probe(&launch, slot).await,
                Err(e) => State::Failed(e),
            },
        };
        self.report(state.clone(), connection);
        state
    }

    async fn probe(&self, launch: &Launch, slot: Slot) -> State {
        self.0.started.fetch_add(1, Ordering::SeqCst);
        match probe::run_with(launch, self.0.timing.startup, Some(Box::new(slot))).await {
            Ok(probe) => State::Ready {
                probe,
                from_chat: false,
            },
            Err(e) => State::Failed(e),
        }
    }

    /// A check a chat's report asked for, only if a slot is free: it is
    /// not worth stopping or waiting for anything.
    async fn recheck(&self, connection: &BTreeMap<String, String>) {
        let _one = self.0.checking.lock().await;
        let Some(launch) = self.launch_base(connection) else {
            return;
        };
        let Some(slot) = self.try_slot() else {
            return;
        };
        let state = self.probe(&launch, slot).await;
        self.report(state, connection);
    }

    /// Record what Claude Code reported: the status, and with a report its
    /// model list. A chat's start reports no context windows, so a model
    /// listed without a known one gets one full check.
    fn report(&self, state: State, connection: &BTreeMap<String, String>) {
        if let State::Ready { probe, from_chat } = &state {
            let saved = self.catalog();
            let base = catalog::base_url(connection, &(self.0.io.env)());
            let next = Catalog::from_probe(probe, &saved, base.as_deref());
            let unknown: Vec<String> = next
                .models
                .iter()
                .filter(|m| m.context_window.is_none())
                .map(|m| m.id.clone())
                .filter(|id| !self.0.checked.lock().unwrap().contains(id))
                .collect();
            if *from_chat && self.0.io.recheck && !unknown.is_empty() {
                self.0.checked.lock().unwrap().extend(unknown);
                let me = self.clone();
                let connection = connection.clone();
                tokio::spawn(async move {
                    me.recheck(&connection).await;
                });
            }
            if next != saved {
                let _ = next.save(&self.0.io.storage);
            }
        }
        self.0.status.send_replace(state);
    }

    fn entry_for(&self, request: &Request) -> Arc<Entry> {
        match &request.session_id {
            Some(id) => self
                .0
                .entries
                .lock()
                .unwrap()
                .entry(id.clone())
                .or_default()
                .clone(),
            None => {
                let e = Arc::new(Entry::default());
                self.0.temporary.lock().unwrap().push(e.clone());
                e
            }
        }
    }

    /// Run one model request, handing each of Claude Code's messages to
    /// `emit`. Cancelling it (through `cancel`) interrupts the turn and
    /// drains it before the session takes another request.
    pub async fn run(
        &self,
        request: Request,
        emit: &mut (dyn FnMut(&Value) + Send),
        cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let entry = self.entry_for(&request);
        let (close_tx, close_rx) = watch::channel(false);
        let mut cancel = Cancel {
            own: cancel,
            closed: close_rx,
        };
        let mut st = tokio::select! {
            g = entry.state.lock() => g,
            _ = cancel.wait() => {
                self.forget(&entry, &request);
                return Err(CANCELLED.into());
            }
        };
        entry.idle.fetch_add(1, Ordering::SeqCst);
        entry.used.store(self.tick(), Ordering::SeqCst);
        *entry.active.lock().unwrap() = Some(close_tx);
        let r = async {
            let (start, continuation) = self.prepare(&mut st, &request).await?;
            self.turn(&mut st, &request, start, continuation, emit, &mut cancel)
                .await
        }
        .await;
        self.release(&entry, &mut st, &request).await;
        drop(st);
        // A waiting request may now stop this process for room.
        self.0.slots.bump();
        r
    }

    fn forget(&self, entry: &Arc<Entry>, request: &Request) {
        if request.session_id.is_none() {
            self.0
                .temporary
                .lock()
                .unwrap()
                .retain(|e| !Arc::ptr_eq(e, entry));
        }
    }

    async fn release(&self, entry: &Arc<Entry>, st: &mut EntryState, request: &Request) {
        entry.active.lock().unwrap().take();
        entry.used.store(self.tick(), Ordering::SeqCst);
        if request.session_id.is_none() {
            stop(st).await;
            if let Some(r) = st.record.take() {
                let _ = self.0.io.transcripts().remove(&r.claude_session_id);
            }
            self.forget(entry, request);
            return;
        }
        if let Some(live) = st.process.as_mut().filter(|l| l.refill) {
            live.refill = false;
            self.fill_later(request);
        }
        // A turn waiting for tool results is still busy (see `idle`).
        if st.process.as_ref().is_some_and(|p| p.waiting.is_some()) {
            return;
        }
        self.arm_idle(entry);
    }

    fn arm_idle(&self, entry: &Arc<Entry>) {
        let generation = entry.idle.fetch_add(1, Ordering::SeqCst) + 1;
        let entry = entry.clone();
        let idle = self.0.timing.idle;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            if entry.idle.load(Ordering::SeqCst) != generation {
                return;
            }
            let mut st = entry.state.lock().await;
            if entry.idle.load(Ordering::SeqCst) == generation {
                stop(&mut st).await;
            }
        });
    }

    /// CodeSmith stopped running the session, so a turn still waiting for
    /// tool results will not get them: its idle timer starts now.
    pub async fn idle(&self, session_id: &str) {
        let Some(entry) = self.0.entries.lock().unwrap().get(session_id).cloned() else {
            return;
        };
        if entry.active.lock().unwrap().is_some() {
            return;
        }
        let Ok(st) = entry.state.try_lock() else {
            return;
        };
        if st.process.as_ref().is_some_and(|p| p.waiting.is_some()) {
            drop(st);
            self.arm_idle(&entry);
        }
    }

    /// Stop everything: cancel running requests (they drain), then stop
    /// every process.
    pub async fn close(&self) {
        let mut warming = self.0.warming.subscribe();
        let _ = warming.wait_for(|n| *n == 0).await;
        let spares: Vec<Spare> = self.0.spares.lock().unwrap().drain(..).collect();
        for s in spares {
            s.live.session.stop().await;
        }
        let all: Vec<Arc<Entry>> = {
            let entries = self.0.entries.lock().unwrap();
            let temporary = self.0.temporary.lock().unwrap();
            entries.values().chain(temporary.iter()).cloned().collect()
        };
        for e in &all {
            if let Some(c) = e.active.lock().unwrap().as_ref() {
                let _ = c.send(true);
            }
        }
        for e in &all {
            let mut st = e.state.lock().await;
            e.idle.fetch_add(1, Ordering::SeqCst);
            stop(&mut st).await;
        }
        self.0.entries.lock().unwrap().clear();
        // A spare a cancelled request asked for meanwhile.
        let _ = warming.wait_for(|n| *n == 0).await;
        let spares: Vec<Spare> = self.0.spares.lock().unwrap().drain(..).collect();
        for s in spares {
            s.live.session.stop().await;
        }
    }

    /// How many sessions have a running process.
    pub async fn running(&self) -> usize {
        let all: Vec<Arc<Entry>> = self.0.entries.lock().unwrap().values().cloned().collect();
        let mut n = 0;
        for e in all {
            if e.state.lock().await.process.is_some() {
                n += 1;
            }
        }
        n
    }

    async fn prepare(
        &self,
        st: &mut EntryState,
        request: &Request,
    ) -> Result<(usize, Option<Continuation>), String> {
        // Retire an exited or incompatible process first. A launch change
        // waits for a parked turn to end (stopping it would record its
        // calls as refused), unless this step offers no tools (the step
        // limit), since the old process still offers them.
        if let Some(p) = &st.process {
            let parked = p.waiting.is_some() && p.session.running() && !request.tools.is_empty();
            if p.session.ended() || (p.launch != request.launch() && !parked) {
                stop(st).await;
            }
        }
        let Some(session_id) = &request.session_id else {
            return Ok((self.snapshot(st, request).await?, None));
        };
        if st.record.is_none() {
            st.record = self.0.records.get(session_id);
        }
        let decision = history::decide(&request.prompt, st.record.as_ref());
        let start = match decision {
            Decision::Fresh | Decision::Changed => {
                // A process that never carried a turn and resumes nothing
                // (a prewarmed one) is what a new Claude session starts
                // with anyway: it stays if the request starts one.
                let unused = match &st.process {
                    Some(p) if p.fresh && !p.session.used() => st.process.take(),
                    _ => None,
                };
                stop(st).await;
                let previous = st.record.take();
                let start = if decision == Decision::Changed {
                    self.branch(st, request, previous.as_ref())
                } else if previous.is_none() {
                    self.snapshot(st, request).await
                } else {
                    Ok(0)
                };
                if let Some(live) = unused {
                    if st.record.is_none() {
                        st.process = Some(live);
                    } else {
                        live.session.stop().await;
                    }
                }
                start?
            }
            _ if st.process.is_some() || st.record.is_none() => decision.start(),
            _ => {
                let id = st.record.as_ref().unwrap().claude_session_id.clone();
                if self.0.io.transcripts().exists(&id) {
                    decision.start()
                } else {
                    st.record = None;
                    0
                }
            }
        };
        let Some(live) = st.process.as_ref() else {
            return Ok((start, None));
        };
        let same_launch = live.launch == request.launch();
        let Some(waiting) = live.waiting.clone().filter(|_| live.session.running()) else {
            return Ok((start, None));
        };
        let collected = tools::collect(&request.prompt, start, &request.aborted)?;
        let mut continuation = Continuation {
            end: collected.end,
            results: Vec::new(),
        };
        for id in waiting {
            match collected.results.get(&id) {
                Some(r) => continuation.results.push((id, r.clone())),
                None => {
                    stop(st).await;
                    st.record = None;
                    return Ok((0, None));
                }
            }
        }
        if collected.aborted {
            // A stopped CodeSmith turn needs a fresh Claude turn; steering
            // would only go on with the old one. After a launch change it
            // needs a new process too.
            if same_launch {
                interrupt(st).await?;
            } else {
                stop(st).await;
            }
            return Ok((start, None));
        }
        Ok((start, Some(continuation)))
    }

    /// Copy the settled part of `source`'s transcript into a branch this
    /// entry continues from, at the last checkpoint the prompt agrees on.
    /// The prompt index the branch continues from (0: start over).
    fn branch(
        &self,
        st: &mut EntryState,
        request: &Request,
        source: Option<&Record>,
    ) -> Result<usize, String> {
        let Some(source) = source else {
            return Ok(0);
        };
        let Some(boundary) = history::boundary(&request.prompt, Some(source)) else {
            return Ok(0);
        };
        // Check the catch-up first: never fork for a request that fails.
        history::catch_up(&request.prompt, boundary.start)?;
        let fork = self
            .0
            .io
            .transcripts()
            .fork(&source.claude_session_id, &boundary.message_id)
            .map_err(|e| e.to_string())?;
        let Some(fork) = fork else {
            return Ok(0);
        };
        let history = boundary
            .history
            .iter()
            .map(|c| super::store::Checkpoint {
                hash: c.hash.clone(),
                claude_message_id: c
                    .claude_message_id
                    .as_ref()
                    .and_then(|id| fork.messages.get(id).cloned()),
            })
            .collect();
        st.record = Some(Record {
            claude_session_id: fork.session_id,
            history: Some(history),
            interrupted: None,
        });
        Ok(boundary.start)
    }

    /// A fork's first request, or a side call: branch the source session.
    async fn snapshot(&self, st: &mut EntryState, request: &Request) -> Result<usize, String> {
        let Some(source_id) = &request.source_id else {
            return Ok(0);
        };
        let source = self.0.entries.lock().unwrap().get(source_id).cloned();
        // Hold the source only while copying its settled prefix.
        let record = match &source {
            Some(e) => {
                let s = e.state.lock().await;
                s.record.clone().or_else(|| self.0.records.get(source_id))
            }
            None => self.0.records.get(source_id),
        };
        self.branch(st, request, record.as_ref())
    }

    async fn open(
        &self,
        st: &mut EntryState,
        request: &Request,
        cancel: &mut Cancel,
    ) -> Result<(Arc<Session>, Arc<Pending>), String> {
        if st.process.is_none() {
            let Some(launch) = self.launch_base(&request.connection) else {
                self.report(State::Missing, &request.connection);
                return Err("Install Claude Code and run `claude auth login` first".into());
            };
            let resume = st.record.as_ref().map(|r| r.claude_session_id.clone());
            let live = match resume.is_none().then(|| self.claim(request)).flatten() {
                Some(live) => live,
                None => {
                    let slot = self.slot(Some(cancel)).await?;
                    self.spawn(request, launch, resume, slot)?
                }
            };
            st.process = Some(live);
        }
        let session = st.process.as_ref().unwrap().session.clone();
        let startup = self.0.timing.startup;
        let ready = tokio::select! {
            r = tokio::time::timeout(startup, session.ready()) => match r {
                Ok(r) => r,
                Err(_) => Err(format!("claude did not start within {} seconds", startup.as_secs().max(1))),
            },
            _ = cancel.wait() => Err(CANCELLED.to_string()),
        };
        let probe: Probe = match ready {
            Ok(p) => p,
            Err(e) => {
                stop(st).await;
                return Err(e);
            }
        };
        // A new process reports its version, login and models, which keeps
        // the model list current without a separate check.
        let live = st.process.as_mut().unwrap();
        if !live.reported {
            live.reported = true;
            self.report(
                State::Ready {
                    probe,
                    from_chat: true,
                },
                &request.connection,
            );
        }
        // Controls go before input or parked results.
        let live = st.process.as_mut().unwrap();
        if live.model != request.model {
            live.session.set_model(&request.model).await?;
            live.model = request.model.clone();
        }
        if live.effort != request.effort {
            live.session.set_effort(request.effort.as_deref()).await?;
            live.effort = request.effort.clone();
        }
        Ok((live.session.clone(), live.pending.clone()))
    }

    async fn turn(
        &self,
        st: &mut EntryState,
        request: &Request,
        start: usize,
        continuation: Option<Continuation>,
        emit: &mut (dyn FnMut(&Value) + Send),
        cancel: &mut Cancel,
    ) -> Result<(), String> {
        // Check before opening a process: a rejected rebuild must not pair
        // an empty process with the old record.
        let content = if request.session_id.is_some() || st.record.is_some() {
            history::catch_up(
                &request.prompt,
                continuation.as_ref().map_or(start, |c| c.end),
            )?
        } else {
            history::content(&request.prompt)?
        };
        let (session, pending) = self.open(st, request, cancel).await?;
        let adds_user = continuation.is_none() || !content.is_empty();
        let user_id = adds_user.then(super::uuid);
        let mut reply = Reply::default();
        let (mut completed, mut interrupted, mut started, mut parked) =
            (false, false, false, false);

        let step: Result<(), String> = async {
            match &continuation {
                None => {
                    session
                        .begin(content, user_id.as_deref().unwrap_or_default())
                        .await?
                }
                Some(c) => {
                    if let Some(u) = &user_id {
                        session.add_user(content, u).await?;
                    }
                    for (id, outcome) in &c.results {
                        pending.resolve(id, outcome.clone())?;
                    }
                }
            }
            started = true;
            let tools = &request.tools;
            let mut handle = |m: &Value| -> Result<(), String> {
                observe(m, tools, &pending, &mut reply)?;
                emit(m);
                Ok(())
            };
            let outcome = tokio::select! {
                r = session.read_step(&mut handle, tools) => r?,
                _ = cancel.wait() => {
                    interrupted = true;
                    return Err(CANCELLED.to_string());
                }
            };
            parked = outcome == Step::Tools;
            completed = true;
            Ok(())
        }
        .await;
        if let Some(live) = st.process.as_mut() {
            live.waiting = parked.then(|| reply.waiting.clone());
        }

        // A tool boundary ends only the CodeSmith step; the Claude turn
        // waits for the results. Anything else interrupts and drains it.
        if !parked {
            let _ = interrupt(st).await;
        }
        if let Some(session_id) = &request.session_id {
            if started && session.persisted() {
                let history = (completed || interrupted).then(|| {
                    history::checkpoints(history::Checkpoints {
                        prompt: &request.prompt,
                        previous: st.record.as_ref(),
                        user_id: user_id.as_deref(),
                        reply: if completed { &reply.parts } else { &[] },
                        message_id: if completed {
                            reply.message_id.as_deref()
                        } else {
                            None
                        },
                        tool_messages: &reply.tool_messages,
                    })
                });
                let record = Record {
                    claude_session_id: session.id.clone(),
                    history,
                    interrupted: Some(interrupted && !completed),
                };
                let _ = self.0.records.set(session_id, &record);
                st.record = Some(record);
            }
        }
        step
    }
}

/// What a step keeps from each message: the reply's text and CodeSmith
/// tool calls (each noted as waiting), and which transcript message holds
/// each tool result. Fails closed when CodeSmith's tool server did not
/// connect: Claude never falls back to its own tools.
fn observe(
    m: &Value,
    offered: &[ToolSpec],
    pending: &Pending,
    reply: &mut Reply,
) -> Result<(), String> {
    if m["type"] == "system" && m["subtype"] == "init" && !offered.is_empty() {
        let connected = m["mcp_servers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["name"] == tools::SERVER && s["status"] == "connected");
        if !connected {
            return Err("The CodeSmith tool server did not connect to Claude Code".into());
        }
    }
    let uuid = m["uuid"].as_str().unwrap_or_default();
    if m["type"] == "assistant"
        && m["parent_tool_use_id"].is_null()
        && m["error"].is_null()
        && reply.seen.insert(uuid.to_string())
    {
        reply.message_id = Some(uuid.to_string());
        for part in m["message"]["content"].as_array().into_iter().flatten() {
            match part["type"].as_str() {
                Some("text") => reply.parts.push(ReplyPart::Text(
                    part["text"].as_str().unwrap_or_default().into(),
                )),
                Some("tool_use") => {
                    let called = part["name"].as_str().unwrap_or_default();
                    let id = part["id"].as_str().unwrap_or_default();
                    let name = tools::original(called, offered);
                    if name.is_none() && called.starts_with(tools::PREFIX) {
                        // A tool removed since this process started is still
                        // on its server; answer here so the turn goes on.
                        pending.expect(id, called)?;
                        pending.resolve(id, tools::error_outcome(tools::REMOVED))?;
                    }
                    let Some(name) = name else {
                        continue;
                    };
                    pending.expect(id, called)?;
                    reply.waiting.push(id.to_string());
                    reply.parts.push(ReplyPart::Call {
                        id: id.into(),
                        name: name.into(),
                        input: part["input"].clone(),
                    });
                }
                _ => {}
            }
        }
    }
    if m["type"] == "user" && !uuid.is_empty() {
        for part in m["message"]["content"].as_array().into_iter().flatten() {
            if part["type"] == "tool_result" {
                if let Some(t) = part["tool_use_id"].as_str() {
                    reply.tool_messages.insert(t.into(), uuid.into());
                }
            }
        }
    }
    Ok(())
}
