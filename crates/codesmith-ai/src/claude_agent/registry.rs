//! The running sessions (OpenCode's `ClaudeSessions`): one `claude` per
//! CodeSmith session, kept between requests, and a throwaway one for each
//! side call (a title, a summary, a compaction).
//!
//! A process stops after [`Timing::idle`] without requests, except while
//! a turn waits for tool results: subagents, permission prompts and
//! questions can take longer than that, and stopping then would record the
//! calls as refused. Once CodeSmith stops running the session
//! ([`Registry::idle`]) the timer starts.

use super::catalog::{self, Catalog, State};
use super::history::{self, Decision, Msg, ReplyPart};
use super::probe::{self, Probe};
use super::process::Handler;
use super::query::{Launch, System};
use super::session::{Session, Step};
use super::store::{Record, Records};
use super::tools::{self, Outcome, Pending};
use super::{Io, Timing};
use crate::ToolSpec;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
        }))
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
            Some(launch) => match probe::run(&launch, self.0.timing.startup).await {
                Ok(probe) => State::Ready {
                    probe,
                    from_chat: false,
                },
                Err(e) => State::Failed(e),
            },
        };
        self.report(state.clone(), connection);
        state
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
                    me.refresh(&connection).await;
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
        *entry.active.lock().unwrap() = Some(close_tx);
        let r = async {
            let (start, continuation) = self.prepare(&mut st, &request).await?;
            self.turn(&mut st, &request, start, continuation, emit, &mut cancel)
                .await
        }
        .await;
        self.release(&entry, &mut st, &request).await;
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
        if request.session_id.is_none() {
            stop(st).await;
            if let Some(r) = st.record.take() {
                let _ = self.0.io.transcripts().remove(&r.claude_session_id);
            }
            self.forget(entry, request);
            return;
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
                stop(st).await;
                let previous = st.record.take();
                if decision == Decision::Changed {
                    self.branch(st, request, previous.as_ref())?
                } else if previous.is_none() {
                    self.snapshot(st, request).await?
                } else {
                    0
                }
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
        let starting = st.process.is_none();
        if starting {
            let Some(mut launch) = self.launch_base(&request.connection) else {
                self.report(State::Missing, &request.connection);
                return Err("Install Claude Code and run `claude auth login` first".into());
            };
            let pending = Arc::new(Pending::default());
            let resume = st.record.as_ref().map(|r| r.claude_session_id.clone());
            launch.directory = request.directory.clone();
            launch.model = Some(request.model.clone());
            launch.effort = request.effort.clone();
            launch.max_output = request.max_output;
            launch.system = Some(request.system.clone());
            launch.tools = request.tools.clone();
            launch.chat = true;
            launch.persist = request.session_id.is_some();
            launch.session_id = resume.is_none().then(super::uuid);
            launch.resume = resume;
            let handler = Arc::new(ChatHandler {
                pending: pending.clone(),
                tools: request.tools.clone(),
                cancelled: Mutex::default(),
            });
            let session = Session::start(&launch, Arc::new(handler))?;
            st.process = Some(Live {
                session: Arc::new(session),
                pending,
                model: request.model.clone(),
                effort: request.effort.clone(),
                launch: request.launch(),
                waiting: None,
            });
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
        if starting {
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
