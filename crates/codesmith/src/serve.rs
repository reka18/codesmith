//! `codesmith serve`: a local HTTP API for driving sessions from other
//! programs (editors, scripts). It listens on loopback by default. Nothing
//! here talks to any hosted service.
//!
//! The API runs shell commands and edits files, so every request must carry
//! `Authorization: Bearer <token>`. Without `--token` the server uses the
//! token saved in the data dir's `serve-token` (owner-only), generating it
//! on first use, so local clients can read it from there. Because a web
//! page can reach loopback too, requests are also refused when their `Host`
//! is not this server (DNS rebinding), when they carry a foreign `Origin`
//! (cross-site requests), or when a body is not JSON. Sessions belong to the
//! server's directory; ids from other projects are not found.
//!
//! Routes (JSON in, JSON out; a turn streams Server-Sent Events):
//!
//! - `GET    /health`
//! - `GET    /sessions` · `POST /sessions {agent?, model?}`
//! - `GET    /sessions/{id}` (info and messages) · `DELETE /sessions/{id}`
//! - `POST   /sessions/{id}/messages {text, attachments?}` → SSE: text,
//!   reasoning, media, tool_start, tool_progress, tool_output, tool_end,
//!   child_start, child_text, child_reasoning, child_end, step_end, retry,
//!   compacted, todos, notice, permission, question, done. `attachments` are images or PDFs,
//!   each `{data: base64, mime?, name?}` or `{path}`; the bytes decide the
//!   type. A path is read as the `read` tool would, so it is asked about
//!   (`external_directory`, `read`) over the stream before the turn starts.
//! - `GET    /sessions/{id}/events` → SSE: every turn's events from now
//!   on, including the turns the server starts itself when a background
//!   command of an idle session finishes (their `done` has `resumed: true`)
//! - `POST   /sessions/{id}/abort`
//! - `POST   /sessions/{id}/permissions/{rid} {reply: once|always|reject, message?}`
//! - `POST   /sessions/{id}/questions/{qid} {answers: [[...]]}` or `{dismiss: true}`
//! - `POST   /sessions/{id}/undo` · `POST /sessions/{id}/redo`
//! - `GET    /events` → SSE: `filesystem.changed` (`{location, file,
//!   event: add|change|unlink}`) for the files watched below and the
//!   project's branch metadata (`.git/HEAD`)
//! - `POST   /fs/watch {path}` · `POST /fs/unwatch {path}`: watch one file
//!   of the directory for 24 hours (watching it again renews that), as
//!   OpenCode's `fs.watch`; 404 when it is not an existing file inside the
//!   directory, 503 when file watching is off
//! - terminals: `/pty`, `/pty/{pty}`, `/pty/{pty}/connect-token`, the
//!   `/pty/{pty}/connect` WebSocket and `/sessions/{id}/terminals`; see
//!   [`pty`]

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Request as HttpRequest, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codesmith_ai::{FinishReason, Usage};
use codesmith_core::config::Loaded;
use codesmith_core::permission::{Approvals, Asker, Fixed, Reply, Request};
use codesmith_core::question::{Prompt, Questioner};
use codesmith_core::session::{self, Options, Session, Sink};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::Store;
use codesmith_core::tool::shell::Jobs;
use codesmith_core::tool::todo::Todo;
use codesmith_core::watch;
use futures_util::Stream;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
mod pty;

type Tx = mpsc::UnboundedSender<Value>;

enum Pending {
    Permission(oneshot::Sender<Reply>),
    Question(oneshot::Sender<Option<Vec<Vec<String>>>>),
}

/// How long an open permission request or question waits while no client
/// can see it (the turn's client went and no one follows the session),
/// before it is refused. A follower connecting meanwhile is shown it.
const UNSEEN_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(1500)
} else {
    Duration::from_secs(30)
};

/// Clients following a session's events (`GET /sessions/{id}/events`),
/// kept across reloads.
type Watchers = Arc<Mutex<Vec<Tx>>>;

/// Routes a session's events, permission requests and questions to the
/// client streaming its current turn and to every watcher.
#[derive(Default)]
struct Prompter {
    events: Mutex<Option<Tx>>,
    watchers: Watchers,
    /// Open prompts by id, with the event that showed them (replayed to
    /// followers that connect while they are open).
    pending: Mutex<HashMap<String, (Pending, Value)>>,
    /// The subagents running in the current turn, innermost last.
    children: Mutex<Children>,
}

/// The subagents a turn runs, as its events name them.
#[derive(Default)]
struct Children {
    /// Running subagent sessions, outermost first.
    stack: Vec<String>,
    /// The latest `subagent` call and the agent it named, for the next
    /// `child_start`.
    call: Option<(String, Option<String>)>,
}

impl Prompter {
    /// The running subagent whose events these are, if any.
    fn child(&self) -> Option<String> {
        self.children.lock().unwrap().stack.last().cloned()
    }
    fn set(&self, tx: Option<Tx>) {
        *self.events.lock().unwrap() = tx;
    }
    /// Send to the turn's client and the watchers; false when none got it.
    fn send(&self, v: Value) -> bool {
        let sent = self
            .events
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(v.clone()).is_ok());
        self.watch(v) || sent
    }
    /// Send to the watchers only (dropping those gone); false when none are left.
    fn watch(&self, v: Value) -> bool {
        let mut w = self.watchers.lock().unwrap();
        w.retain(|tx| tx.send(v.clone()).is_ok());
        !w.is_empty()
    }
    /// Drop every open prompt (each is then rejected or dismissed).
    fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
    /// An unguessable id, so a client can only answer prompts it was shown.
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", codesmith_core::paths::random_token(16))
    }
    /// Whether some client still receives this session's events.
    fn seen(&self) -> bool {
        self.events
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
            || self
                .watchers
                .lock()
                .unwrap()
                .iter()
                .any(|tx| !tx.is_closed())
    }
    /// Show a prompt and wait (on a blocking thread) for its answer.
    fn open<T>(
        &self,
        id: String,
        ev: Value,
        pending: Pending,
        mut rx: oneshot::Receiver<T>,
    ) -> Shown<T> {
        {
            // Shown and recorded at once, so a follower connecting
            // meanwhile gets it exactly once (see `follow`).
            let mut open = self.pending.lock().unwrap();
            if !self.send(ev.clone()) {
                return Shown::Unseen;
            }
            open.insert(id.clone(), (pending, ev));
        }
        let mut unseen = Duration::ZERO;
        let tick = Duration::from_millis(100);
        loop {
            match rx.try_recv() {
                Ok(v) => return Shown::Answered(v),
                Err(oneshot::error::TryRecvError::Closed) => return Shown::Dropped,
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            unseen = if self.seen() {
                Duration::ZERO
            } else {
                unseen + tick
            };
            if unseen >= UNSEEN_WAIT {
                self.pending.lock().unwrap().remove(&id);
                // Answered just before it was withdrawn.
                return rx.try_recv().map_or(Shown::Unseen, Shown::Answered);
            }
            std::thread::sleep(tick);
        }
    }
    /// Add a follower, first showing it the open prompts.
    fn follow(&self, tx: Tx) {
        let open = self.pending.lock().unwrap();
        for (_, ev) in open.values() {
            let _ = tx.send(ev.clone());
        }
        self.watchers.lock().unwrap().push(tx);
    }
}

/// How an open prompt ended.
enum Shown<T> {
    Answered(T),
    /// No client got it, or none could see it for [`UNSEEN_WAIT`] (its
    /// client went without answering and no follower came).
    Unseen,
    /// Dropped unanswered (the turn was aborted).
    Dropped,
}

impl Asker for Prompter {
    fn ask(&self, req: &Request) -> Reply {
        let (tx, rx) = oneshot::channel();
        let id = self.id("perm");
        let mut ev = json!({"type": "permission", "id": id, "action": req.action, "resources": req.resources, "save": req.save, "title": req.title});
        // Asked by a subagent: prompts come in order with its events.
        if let Some(child) = self.child() {
            ev["session"] = json!(child);
        }
        match self.open(id, ev, Pending::Permission(tx), rx) {
            Shown::Answered(r) => r,
            Shown::Unseen => Reply::Reject(Some("No client is connected to approve this.".into())),
            Shown::Dropped => Reply::Reject(None),
        }
    }
}

impl Questioner for Prompter {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>> {
        let (tx, rx) = oneshot::channel();
        let id = self.id("q");
        let ev = json!({"type": "question", "id": id, "prompts": prompts});
        match self.open(id, ev, Pending::Question(tx), rx) {
            Shown::Answered(a) => a,
            Shown::Unseen | Shown::Dropped => None,
        }
    }
}

struct JsonSink(Arc<Prompter>);

impl JsonSink {
    fn send(&self, v: Value) {
        self.0.send(v);
    }
    /// Send a tool event, naming the subagent whose call it is.
    fn send_tool(&self, mut v: Value) {
        if let Some(child) = self.0.child() {
            v["session"] = json!(child);
        }
        self.send(v);
    }
}

/// A generated file's metadata (its bytes are in the session's messages).
fn media_event(m: &codesmith_ai::Media) -> Value {
    let mut ev = json!({"type": "media", "mime": m.mime, "name": m.filename});
    match &m.source {
        codesmith_ai::Source::Base64(b) => {
            let pad = b.bytes().rev().take_while(|c| *c == b'=').count();
            ev["size"] = json!((b.len() / 4 * 3).saturating_sub(pad));
        }
        codesmith_ai::Source::Url(u) => ev["url"] = json!(u),
    }
    ev
}

impl Sink for JsonSink {
    fn text(&mut self, delta: &str) {
        self.send(json!({"type": "text", "delta": delta}));
    }
    fn reasoning(&mut self, delta: &str) {
        self.send(json!({"type": "reasoning", "delta": delta}));
    }
    fn media(&mut self, media: &codesmith_ai::Media) {
        self.send(media_event(media));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        if name.rsplit(" › ").next() == Some("subagent") {
            let agent = input["agent"].as_str().map(str::to_string);
            self.0.children.lock().unwrap().call = Some((id.to_string(), agent));
        }
        self.send_tool(json!({"type": "tool_start", "id": id, "name": name, "input": input}));
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        self.send_tool(json!({"type": "tool_end", "id": id, "name": name, "output": output, "error": is_error}));
    }
    fn child_start(&mut self, child: &session::ChildSession) {
        let (call, agent) = {
            let mut c = self.0.children.lock().unwrap();
            c.stack.push(child.id.clone());
            c.call.take().unzip()
        };
        self.send(json!({
            "type": "child_start",
            "session": child.id,
            "parent": child.parent_id,
            "depth": child.depth,
            "title": child.title,
            "agent": agent.flatten(),
            "tool": call,
        }));
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        {
            let mut c = self.0.children.lock().unwrap();
            if let Some(at) = c.stack.iter().rposition(|s| s == id) {
                c.stack.truncate(at);
            }
        }
        self.send(json!({"type": "child_end", "session": id, "status": status, "error": error}));
    }
    fn child_text(&mut self, delta: &str) {
        self.send(json!({"type": "child_text", "session": self.0.child(), "delta": delta}));
    }
    fn child_reasoning(&mut self, delta: &str) {
        self.send(json!({"type": "child_reasoning", "session": self.0.child(), "delta": delta}));
    }
    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        self.send(json!({"type": "step_end", "reason": reason.normalized, "finish": reason, "usage": usage}));
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        self.send(json!({"type": "retry", "attempt": attempt, "error": error, "wait_ms": wait.as_millis() as u64}));
    }
    fn compacted(&mut self, before: u64, after: u64) {
        self.send(json!({"type": "compacted", "before": before, "after": after}));
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.send(json!({"type": "todos", "todos": todos}));
    }
    fn notice(&mut self, message: &str) {
        self.send(json!({"type": "notice", "message": message}));
    }
    fn tool_progress(&mut self, id: &str, metadata: &Value) {
        self.send_tool(json!({"type": "tool_progress", "id": id, "metadata": metadata}));
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        self.send_tool(json!({"type": "tool_output", "id": id, "delta": delta}));
    }
}

/// A session held in memory. `session` is `None` while a turn runs.
struct Live {
    session: Option<Session>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Waits, while the session is idle, for one of its background
    /// commands to finish, to resume it (see [`wake`]).
    waiter: Option<tokio::task::JoinHandle<()>>,
    /// After an abort, how many finished background commands are not
    /// resumed for (see [`Session::idle_wake`]).
    held: Option<usize>,
    prompter: Arc<Prompter>,
    /// The "always" approvals given in this session, kept across reloads.
    approvals: Approvals,
    /// The agent and model it was made with, for a reload before anything
    /// was stored.
    selection: (String, String),
    /// Its background commands, given to every reload (abort, undo,
    /// redo), so they outlive the session a stopped turn dropped. Stopped
    /// by `DELETE` or an abort of the idle session.
    jobs: Jobs,
}

/// How long an abort waits for the turn's task to stop. A tool parked in a
/// blocking call only notices the abort when that call returns.
const STOP_WAIT: Duration = Duration::from_secs(10);

pub struct Server {
    pub loaded: Loaded,
    pub cwd: PathBuf,
    pub db: PathBuf,
    pub auto: bool,
    pub token: String,
    pub mcp: Arc<codesmith_core::mcp::Mcp>,
    /// `Host` values this server answers to (`None`: any, when bound to a
    /// wildcard address). Set when it starts listening.
    hosts: OnceLock<Option<Vec<String>>>,
    live: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Live>>>>,
    /// File watching, started when the server starts listening (or on
    /// first use).
    watching: OnceLock<Watching>,
    /// Terminals, stopped with the server.
    #[cfg(unix)]
    ptys: codesmith_core::pty::Ptys,
    /// Tickets for opening a terminal's WebSocket.
    #[cfg(unix)]
    tickets: codesmith_core::pty::ticket::Tickets,
}

/// The server's file watches and where their changes go.
struct Watching {
    events: watch::Events,
    files: watch::file_watch::FileWatch,
    _location: watch::location::LocationWatcher,
    _ignore: watch::policy::Registration,
}

impl Watching {
    /// Watch `cwd`'s branch metadata (in the background) and be ready for
    /// file watches; `watcher.ignore` is the policy.
    fn start(cwd: &Path, ignore: Vec<String>) -> Watching {
        let watcher = watch::global();
        let events = watch::Events::new();
        let files = if watcher.enabled() {
            watch::file_watch::FileWatch::new(watcher.clone(), events.clone())
        } else {
            watch::file_watch::FileWatch::disabled()
        };
        let policy = watch::policy::Policy::new();
        let reg = policy.transform(move |e| e.add(&ignore));
        let vcs = watch::location::Vcs::detect(cwd);
        let location = watch::location::LocationWatcher::start(
            cwd,
            watcher,
            policy,
            events.clone(),
            watch::location::discover(cwd, vcs),
            None,
        );
        Watching {
            events,
            files,
            _location: location,
            _ignore: reg,
        }
    }
}

type St = Arc<Server>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("no session {id}"))
}

fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "a turn is running in this session; abort it or wait".into(),
    )
}

impl Server {
    pub fn new(
        loaded: Loaded,
        cwd: PathBuf,
        db: PathBuf,
        auto: bool,
        token: String,
        mcp: Arc<codesmith_core::mcp::Mcp>,
    ) -> Arc<Server> {
        #[cfg(unix)]
        let ptys = {
            let shell = loaded
                .config
                .shell
                .clone()
                .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "/bin/sh".into());
            codesmith_core::pty::Ptys::new(cwd.clone(), shell)
        };
        Arc::new(Server {
            #[cfg(unix)]
            ptys,
            #[cfg(unix)]
            tickets: Default::default(),
            loaded,
            cwd,
            db,
            auto,
            token,
            mcp,
            hosts: OnceLock::new(),
            live: Default::default(),
            watching: OnceLock::new(),
        })
    }

    fn watching(&self) -> &Watching {
        self.watching
            .get_or_init(|| Watching::start(&self.cwd, self.loaded.config.watcher.ignore.clone()))
    }

    /// Whether a stored session belongs to this server's directory.
    fn ours(&self, info: &codesmith_core::store::Info) -> bool {
        info.directory == self.cwd.display().to_string()
    }

    fn store(&self) -> Result<Store> {
        Store::open(&self.db)
    }

    fn make(
        &self,
        resume: Option<String>,
        agent: Option<String>,
        model: Option<String>,
    ) -> Result<Live> {
        self.make_with(resume, agent, model, Watchers::default())
    }

    fn make_with(
        &self,
        resume: Option<String>,
        agent: Option<String>,
        model: Option<String>,
        watchers: Watchers,
    ) -> Result<Live> {
        let prompter = Arc::new(Prompter {
            watchers,
            ..Default::default()
        });
        let (asker, questioner): (Arc<dyn Asker>, Option<Arc<dyn Questioner>>) = if self.auto {
            (Arc::new(Fixed(Reply::Once)), None)
        } else {
            (prompter.clone(), Some(prompter.clone()))
        };
        let s = Session::new(
            &self.loaded,
            self.cwd.clone(),
            Options {
                agent,
                model,
                resume,
                title: None,
                parent: None,
                questioner,
                mcp: Some(self.mcp.clone()),
            },
            asker,
            Some(self.store()?),
        )?;
        Ok(Live {
            jobs: s.jobs(),
            approvals: s.approvals(),
            selection: (s.agent.id.clone(), s.model_spec.clone()),
            session: Some(s),
            task: None,
            waiter: None,
            held: None,
            prompter,
        })
    }

    /// Rebuild `g`'s session `id` from the store, where every message of
    /// a stopped turn already is. One never stored starts empty under the
    /// same id and selection. "Always" approvals, watchers and background
    /// commands carry over (call [`wake`] after, to wait for the commands).
    fn reload(&self, g: &mut Live, id: &str) -> Result<()> {
        if let Some(w) = g.waiter.take() {
            w.abort();
        }
        let stored = self.store()?.get(id)?.is_some();
        let watchers = g.prompter.watchers.clone();
        let fresh = if stored {
            self.make_with(Some(id.to_string()), None, None, watchers)?
        } else {
            let (agent, model) = g.selection.clone();
            self.make_with(None, Some(agent), Some(model), watchers)?
        };
        let mut s = fresh.session.expect("a made session");
        s.set_id(id);
        s.share_approvals(g.approvals.clone());
        s.adopt_jobs(g.jobs.clone());
        g.selection = fresh.selection;
        g.session = Some(s);
        g.prompter = fresh.prompter;
        Ok(())
    }

    /// The in-memory session, loading it from the store on first use.
    async fn live(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<Live>>, ApiError> {
        let mut map = self.live.lock().await;
        if let Some(l) = map.get(id) {
            return Ok(l.clone());
        }
        if !self.store()?.get(id)?.is_some_and(|i| self.ours(&i)) {
            return Err(not_found(id));
        }
        let l = Arc::new(tokio::sync::Mutex::new(self.make(
            Some(id.to_string()),
            None,
            None,
        )?));
        map.insert(id.to_string(), l.clone());
        Ok(l)
    }
}

/// Messages as a client shows them: without the ones the engine wrote
/// itself (system updates, output-limit nudges), as in the TUI and ACP replay.
fn transcript(messages: &[codesmith_ai::Message]) -> Vec<&codesmith_ai::Message> {
    messages
        .iter()
        .filter(|m| !codesmith_core::runner::is_engine(m))
        .collect()
}

fn summary(s: &Session) -> Value {
    json!({"id": s.id, "title": s.title, "agent": s.agent.name, "model": s.model_spec, "usage": s.usage, "directory": s.cwd})
}

/// Requests larger than this are refused (413).
const MAX_BODY: usize = 8 << 20;

fn forbidden(why: &str) -> Response {
    ApiError(StatusCode::FORBIDDEN, why.into()).into_response()
}

/// The `Host` values a server bound to `addr` (as `--host host`) answers to,
/// or `None` for any when it listens on every interface.
fn allowed_hosts(host: &str, addr: SocketAddr) -> Option<Vec<String>> {
    let ip = addr.ip();
    if ip.is_unspecified() {
        return None;
    }
    let mut names: Vec<String> = if ip.is_loopback() {
        vec!["localhost".into(), "127.0.0.1".into(), "[::1]".into()]
    } else {
        let mut v = vec![host.to_ascii_lowercase()];
        v.push(match ip {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        });
        v
    };
    names.dedup();
    let port = addr.port();
    let mut out: Vec<String> = names.iter().map(|n| format!("{n}:{port}")).collect();
    if port == 80 {
        out.extend(names);
    }
    Some(out)
}

/// Refuse anything that might come from a web page rather than a local
/// client: a `Host` that isn't this server (DNS rebinding), an `Origin` that
/// isn't this server (cross-site requests), a missing or wrong bearer token,
/// or a body that isn't JSON (which browsers cannot send cross-site without
/// a preflight that this server never answers).
async fn guard(State(st): State<St>, req: HttpRequest, next: Next) -> Response {
    let h = req.headers();
    let text = |name: header::HeaderName| h.get(name).and_then(|v| v.to_str().ok());
    let Some(host) = text(header::HOST).map(str::to_ascii_lowercase) else {
        return forbidden("a Host header is required");
    };
    match st.hosts.get() {
        Some(Some(allowed)) if !allowed.contains(&host) => {
            return forbidden("this server does not answer to that Host")
        }
        Some(_) => {}
        None => return forbidden("the server is not listening yet"),
    }
    if let Some(origin) = h.get(header::ORIGIN) {
        let same = origin
            .to_str()
            .is_ok_and(|o| o.eq_ignore_ascii_case(&format!("http://{host}")));
        if !same {
            return forbidden("cross-origin requests are not allowed");
        }
    }
    let ok = text(header::AUTHORIZATION)
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| constant_eq(v.as_bytes(), st.token.as_bytes()));
    // A terminal's WebSocket may carry a ticket instead, which its handler
    // checks (a browser cannot set the header).
    #[cfg(unix)]
    let ok = ok || pty::ticketed(&req);
    if !ok {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token".into(),
        )
        .into_response();
    }
    let has_body = h.contains_key(header::TRANSFER_ENCODING)
        || text(header::CONTENT_LENGTH).is_some_and(|l| l.trim() != "0");
    if has_body && !matches!(*req.method(), Method::GET | Method::HEAD) {
        let json = text(header::CONTENT_TYPE).is_some_and(|t| {
            t.split(';')
                .next()
                .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
        });
        if !json {
            return ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "request bodies must be application/json".into(),
            )
            .into_response();
        }
    }
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn router(st: St) -> Router {
    let routes = Router::new();
    #[cfg(unix)]
    let routes = routes.merge(pty::routes());
    routes
        .route("/health", get(health))
        .route("/sessions", get(list).post(create))
        .route("/sessions/{id}", get(show).delete(remove))
        .route("/sessions/{id}/messages", post(message))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/abort", post(abort))
        .route("/sessions/{id}/permissions/{rid}", post(answer_permission))
        .route("/sessions/{id}/questions/{qid}", post(answer_question))
        .route(
            "/sessions/{id}/environment",
            get(environment).put(set_environment),
        )
        .route("/sessions/{id}/background", post(background))
        .route("/sessions/{id}/jobs", get(jobs))
        .route("/sessions/{id}/jobs/{jid}/cancel", post(cancel_job))
        .route("/sessions/{id}/undo", post(undo))
        .route("/sessions/{id}/redo", post(redo))
        .route("/events", get(fs_events))
        .route("/fs/watch", post(fs_watch))
        .route("/fs/unwatch", post(fs_unwatch))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(st.clone(), guard))
        .with_state(st)
}

async fn health(State(st): State<St>) -> Json<Value> {
    Json(
        json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "directory": st.cwd, "model": st.loaded.config.model, "auto": st.auto}),
    )
}

async fn list(State(st): State<St>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(st
        .store()?
        .list(Some(&st.cwd.display().to_string()), false)?)))
}

async fn create(State(st): State<St>, body: Option<Json<Value>>) -> Result<Json<Value>, ApiError> {
    let b = body.map(|b| b.0).unwrap_or_default();
    let s = |k: &str| b[k].as_str().map(str::to_string);
    let live = st
        .make(None, s("agent"), s("model"))
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let out = summary(live.session.as_ref().unwrap());
    let id = live.session.as_ref().unwrap().id.clone();
    st.live
        .lock()
        .await
        .insert(id, Arc::new(tokio::sync::Mutex::new(live)));
    Ok(Json(out))
}

async fn show(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    match &g.session {
        Some(s) => {
            let mut v = summary(s);
            v["messages"] = json!(transcript(&s.messages));
            v["todos"] = json!(s.todos);
            v["busy"] = json!(false);
            v["background"] = json!(s.background_running());
            Ok(Json(v))
        }
        None => {
            let info = st.store()?.get(&id)?.ok_or_else(|| not_found(&id))?;
            Ok(Json(
                json!({"id": info.id, "title": info.title, "agent": info.agent, "model": info.model, "usage": info.usage, "messages": transcript(&st.store()?.history(&id)?), "busy": true}),
            ))
        }
    }
}

async fn remove(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    if !st.store()?.get(&id)?.is_some_and(|i| st.ours(&i)) {
        return Err(not_found(&id));
    }
    // Its terminals go with it.
    #[cfg(unix)]
    st.ptys.remove_session(&id);
    let gone = st.live.lock().await.remove(&id);
    if let Some(l) = gone {
        let mut g = l.lock().await;
        if let Some(w) = g.waiter.take() {
            w.abort();
        }
        if let Some(mut t) = g.task.take() {
            t.abort();
            g.prompter.set(None);
            g.prompter.clear();
            // Let it stop before deleting, so it cannot write afterwards.
            let _ = tokio::time::timeout(STOP_WAIT, &mut t).await;
        }
        // Its background commands go with it; watchers' streams end.
        g.session.take();
        g.jobs.stop_all().await;
        g.prompter.watchers.lock().unwrap().clear();
    }
    let deleted = st.store()?.delete(&id)?;
    Ok(Json(json!({"deleted": deleted})))
}

async fn message(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<Value>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let attachments = attachments(&b, &st.loaded.config.media.image)?;
    let text = b["text"]
        .as_str()
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty() || !attachments.is_empty())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "\"text\" is required".into()))?;
    let live = st.live(&id).await?;
    let mut g = live.lock().await;
    let mut s = g.session.take().ok_or_else(busy)?;
    // The prompt reports any background command that finished meanwhile.
    if let Some(w) = g.waiter.take() {
        w.abort();
    }
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    g.prompter.set(Some(tx.clone()));
    let (live2, prompter) = (live.clone(), g.prompter.clone());
    g.task = Some(tokio::spawn(async move {
        let mut sink = JsonSink(prompter.clone());
        // Paths are checked now, so their permission requests reach this
        // stream.
        let mut media = Vec::new();
        let mut failed = None;
        for a in attachments {
            match a {
                Attachment::Media(m) => media.push(m),
                Attachment::Path(p) => match s.attach(&p) {
                    Ok(Some(m)) => media.push(m),
                    Ok(None) => {
                        failed = Some(format!("attachment {p} is not an image, PDF or video"));
                        break;
                    }
                    Err(e) => {
                        failed = Some(format!("attachment {p}: {e}"));
                        break;
                    }
                },
            }
        }
        let r = match failed {
            Some(e) => Err(anyhow::anyhow!(e)),
            None => s.prompt_with(&text, media, &mut sink).await,
        };
        let done = json!({"type": "done", "error": r.err().map(|e| format!("{e:#}")), "usage": s.usage, "title": s.title});
        let mut g = live2.lock().await;
        g.session = Some(s);
        g.task = None;
        prompter.set(None);
        prompter.watch(done.clone());
        wake(&live2, &mut g);
        drop(g);
        let _ = tx.send(done);
    }));
    drop(g);
    Ok(sse(rx))
}

fn sse(
    mut rx: mpsc::UnboundedReceiver<Value>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        while let Some(v) = rx.recv().await {
            let kind = v["type"].as_str().unwrap_or("message").to_string();
            yield Ok(Event::default().event(kind).data(v.to_string()));
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Follow a session: the events of every turn from now on, whoever started
/// it, as `POST /messages` streams them. The turns the server starts itself
/// (a background command finished while the session was idle) are only
/// seen here, and their permission requests and questions only reach
/// watchers. The stream stays open until the client goes or the session is
/// deleted.
async fn events(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    // A prompt shown before it connected (to a client since gone) can
    // still be answered from here.
    g.prompter.follow(tx);
    Ok(sse(rx))
}

/// `filesystem.changed` for the server's directory, from now on.
async fn fs_events(State(st): State<St>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = st.watching().events.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(c) => {
                    let mut v = json!(c);
                    v["type"] = json!("filesystem.changed");
                    yield Ok(Event::default().event("filesystem.changed").data(v.to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The absolute path of `{path}` in a watch request, and whether it is an
/// existing file inside the server's directory (the same confinement as
/// reading it).
fn watch_target(st: &Server, b: &Value) -> Result<(PathBuf, bool), ApiError> {
    let rel = b["path"]
        .as_str()
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "path is required".into()))?;
    let file = watch::absolute(&st.cwd.join(rel));
    let inside = match (std::fs::canonicalize(&file), std::fs::canonicalize(&st.cwd)) {
        (Ok(f), Ok(root)) => f.starts_with(&root) && f.is_file(),
        _ => false,
    };
    let inside = inside && file.starts_with(&st.cwd);
    Ok((file, inside))
}

async fn fs_watch(State(st): State<St>, Json(b): Json<Value>) -> Result<Json<Value>, ApiError> {
    let (file, ok) = watch_target(&st, &b)?;
    if !ok {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("File not found: {}", b["path"].as_str().unwrap_or_default()),
        ));
    }
    st.watching()
        .files
        .watch(&st.cwd, &file)
        .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    Ok(Json(json!({"ok": true})))
}

async fn fs_unwatch(State(st): State<St>, Json(b): Json<Value>) -> Result<Json<Value>, ApiError> {
    let (file, _) = watch_target(&st, &b)?;
    st.watching().files.unwatch(&st.cwd, &file);
    Ok(Json(json!({"ok": true})))
}

/// `g`'s session is idle: if a background command of it finished, resume
/// it as a turn of its own (OpenCode wakes an idle session with the
/// command's notice); if one still runs, wait for it and come back here.
/// A prompt sent meanwhile cancels the wait and reports the command itself.
///
/// A resume that failed backs off before the next (see
/// [`Session::resume_due`]), and after an abort the commands that had
/// finished wait for the next prompt or for another one to finish ([`hold`]).
fn wake(live: &Arc<tokio::sync::Mutex<Live>>, g: &mut Live) {
    if let Some(w) = g.waiter.take() {
        w.abort();
    }
    let Some(s) = g.session.as_ref().filter(|_| g.task.is_none()) else {
        return;
    };
    let reaper = s.reaper();
    match s.idle_wake(&mut g.held) {
        session::IdleWake::Resume => resume(live, g),
        session::IdleWake::At(due) => wake_after(live, g, tokio::time::sleep_until(due.into())),
        session::IdleWake::Finish(seen) => {
            wake_after(live, g, async move { reaper.wait_more(seen).await })
        }
        session::IdleWake::Idle => {}
    }
}

/// Run [`wake`] again once `wait` is over (aborted when `g`'s waiter is).
fn wake_after(
    live: &Arc<tokio::sync::Mutex<Live>>,
    g: &mut Live,
    wait: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let live = live.clone();
    g.waiter = Some(tokio::spawn(async move {
        wait.await;
        let mut g = live.lock().await;
        // This task: drop its handle without aborting it.
        g.waiter.take();
        wake(&live, &mut g);
    }));
}

/// The user aborted `g`'s turn: don't resume it for the background
/// commands already finished (they are reported with the next prompt, or
/// when another finishes), or an aborted resume would start over at once.
fn hold(g: &mut Live) {
    g.held = g.session.as_ref().map(|s| s.background_finished());
}

/// Run the turn that reports `g`'s finished background commands. Its
/// events go to the watchers; it is aborted like any turn.
fn resume(live: &Arc<tokio::sync::Mutex<Live>>, g: &mut Live) {
    let Some(mut s) = g.session.take() else {
        return;
    };
    let (live2, prompter) = (live.clone(), g.prompter.clone());
    g.task = Some(tokio::spawn(async move {
        let mut sink = JsonSink(prompter.clone());
        let r = s.resume_background(&mut sink).await;
        let done = json!({"type": "done", "resumed": true, "error": r.err().map(|e| format!("{e:#}")), "usage": s.usage, "title": s.title});
        let mut g = live2.lock().await;
        g.session = Some(s);
        g.task = None;
        prompter.watch(done);
        wake(&live2, &mut g);
    }));
}

/// A prompt attachment: inline media, or a path checked when the turn starts.
enum Attachment {
    Media(codesmith_ai::Media),
    Path(String),
}

/// The `attachments` of a message body. Inline data must be an image or
/// PDF; images are fitted to the configured limits.
fn attachments(
    b: &Value,
    limits: &codesmith_core::media::ImageLimits,
) -> Result<Vec<Attachment>, ApiError> {
    use codesmith_core::media;
    let bad = |why: String| ApiError(StatusCode::BAD_REQUEST, why);
    let list = match &b["attachments"] {
        Value::Null => return Ok(vec![]),
        Value::Array(list) => list,
        _ => return Err(bad("\"attachments\" must be a list".into())),
    };
    let mut out = Vec::new();
    for (i, a) in list.iter().enumerate() {
        match (a["data"].as_str(), a["path"].as_str()) {
            (Some(data), None) => {
                let name = a["name"].as_str().unwrap_or("attachment").to_string();
                let bytes = media::unbase64(data.trim())
                    .ok_or_else(|| bad(format!("attachment {i}: \"data\" is not base64")))?;
                let mime = media::detect(&bytes)
                    .ok_or_else(|| bad(format!("attachment {i}: not an image, PDF or video")))?;
                let mut m = codesmith_ai::Media::base64(mime, data.trim());
                if mime.starts_with("image/") {
                    match media::normalize(mime, data.trim(), limits) {
                        Ok(None) => {}
                        Ok(Some((mime, data))) => m = codesmith_ai::Media::base64(mime, data),
                        Err(e) => return Err(bad(e.message(&name, limits))),
                    }
                }
                out.push(Attachment::Media(match a["name"].as_str() {
                    Some(n) => m.named(n),
                    None => m,
                }));
            }
            (None, Some(path)) if !path.trim().is_empty() => {
                out.push(Attachment::Path(path.to_string()))
            }
            _ => {
                return Err(bad(format!(
                    "attachment {i}: give one of \"data\" (base64) or \"path\""
                )))
            }
        }
    }
    Ok(out)
}

async fn abort(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let mut g = live.lock().await;
    let Some(mut t) = g.task.take() else {
        // Idle: stop its background commands (and the wait to resume for
        // them); reloading drops the stopped ones unreported.
        let waiting = g.waiter.take().inspect(|w| w.abort()).is_some();
        let running = g.session.as_ref().map_or(0, |s| s.background_running());
        if waiting || running > 0 {
            g.jobs.stop_all().await;
            // A fresh registry: the stopped ones are dropped unreported.
            // The session's environment stays.
            let environment = g.jobs.environment();
            g.jobs = Jobs::default();
            g.jobs.set_environment(environment);
            st.reload(&mut g, &id)?;
            return Ok(Json(
                json!({"aborted": false, "background_stopped": running}),
            ));
        }
        return Ok(Json(json!({"aborted": false})));
    };
    t.abort();
    // Unblocks a tool waiting on a prompt (it is declined), and any later
    // prompt is refused at once; the abort lands at the task's next await.
    let events = g.prompter.events.lock().unwrap().take();
    g.prompter.clear();
    // Until the task stops it can still record a result. Reloading first
    // would leave the call unanswered in the reloaded session, which the
    // next prompt then answers a second time. The task cannot finish
    // normally meanwhile: it needs this lock to hand the session back.
    let stopped = tokio::time::timeout(STOP_WAIT, &mut t).await.is_ok();
    if let Some(tx) = events {
        let _ = tx.send(json!({"type": "done", "error": "aborted"}));
    }
    g.prompter
        .watch(json!({"type": "done", "error": "aborted"}));
    if !stopped {
        // Still inside a blocking call: reload when it returns. The
        // session stays busy until then.
        let (st, live, id) = (st.clone(), live.clone(), id.clone());
        tokio::spawn(async move {
            let _ = t.await;
            let mut g = live.lock().await;
            if g.session.is_none() && g.task.is_none() {
                match st.reload(&mut g, &id) {
                    Ok(()) => {
                        hold(&mut g);
                        wake(&live, &mut g)
                    }
                    Err(e) => eprintln!("serve: could not reload session {id}: {e:#}"),
                }
            }
        });
        return Ok(Json(json!({"aborted": true, "stopping": true})));
    }
    if let Err(e) = st.reload(&mut g, &id) {
        // Not left busy forever: the next request loads it afresh.
        drop(g);
        st.live.lock().await.remove(&id);
        return Err(e.into());
    }
    // Its background commands survive the abort (as in OpenCode): wait
    // for them again. An abort of the idle session stops them.
    hold(&mut g);
    wake(&live, &mut g);
    Ok(Json(json!({"aborted": true})))
}

/// The environment the session's shell commands run in, when one was set.
async fn environment(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    Ok(Json(json!({"variables": g.jobs.environment()})))
}

/// Replace the environment the session's shell commands run in, as
/// OpenCode's `PUT /api/session/:id/environment` does: `{"variables":
/// {...}}` replaces this server's own for its commands; `null` goes back
/// to it.
async fn set_environment(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let variables = match b.get("variables") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<std::collections::BTreeMap<String, String>>(v.clone())
                .map_err(|_| {
                    ApiError(
                        StatusCode::BAD_REQUEST,
                        "variables must map names to strings".into(),
                    )
                })?,
        ),
    };
    let live = st.live(&id).await?;
    let g = live.lock().await;
    g.jobs.set_environment(variables);
    Ok(Json(json!({"variables": g.jobs.environment()})))
}

/// Move the running turn's foreground commands to the background (OpenCode's
/// `POST /api/session/:id/background`); the model is told and carries on.
async fn background(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    let moved: Vec<Value> = g
        .jobs
        .background_all()
        .iter()
        .map(|r| json!({"id": r.id, "type": "shell", "title": r.command}))
        .collect();
    Ok(Json(json!({"backgrounded": moved})))
}

/// The session's running commands, foreground and background.
async fn jobs(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    let mut data: Vec<Value> = g
        .jobs
        .foreground()
        .iter()
        .map(|r| json!({"id": r.id, "command": r.command, "pid": r.pid, "background": false}))
        .collect();
    data.extend(g.jobs.running_jobs().iter().map(|j| {
        json!({"id": j.id, "command": j.command, "pid": j.pid, "background": true, "output": j.file})
    }));
    Ok(Json(json!({"data": data})))
}

/// Stop one running command (OpenCode's job cancel). Cancelling one that
/// already finished does nothing.
async fn cancel_job(
    State(st): State<St>,
    UrlPath((id, jid)): UrlPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let g = live.lock().await;
    Ok(Json(json!({"cancelled": g.jobs.cancel(&jid)})))
}

async fn answer_permission(
    State(st): State<St>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
    Json(b): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let reply = match b["reply"].as_str() {
        Some("once") => Reply::Once,
        Some("always") => Reply::Always,
        Some("reject") => Reply::Reject(b["message"].as_str().map(str::to_string)),
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "reply must be once, always or reject".into(),
            ))
        }
    };
    let live = st.live(&id).await?;
    let p = live.lock().await.prompter.clone();
    let pending = p.pending.lock().unwrap().remove(&rid);
    match pending {
        Some((Pending::Permission(tx), _)) => {
            let _ = tx.send(reply);
            Ok(Json(json!({"ok": true})))
        }
        _ => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no open permission request {rid}"),
        )),
    }
}

async fn answer_question(
    State(st): State<St>,
    UrlPath((id, qid)): UrlPath<(String, String)>,
    Json(b): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let answers: Option<Vec<Vec<String>>> = if b["dismiss"].as_bool() == Some(true) {
        None
    } else {
        Some(serde_json::from_value(b["answers"].clone()).map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "answers must be a list of lists of strings".into(),
            )
        })?)
    };
    let live = st.live(&id).await?;
    let p = live.lock().await.prompter.clone();
    let pending = p.pending.lock().unwrap().remove(&qid);
    match pending {
        Some((Pending::Question(tx), _)) => {
            let _ = tx.send(answers);
            Ok(Json(json!({"ok": true})))
        }
        _ => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no open question {qid}"),
        )),
    }
}

async fn undo(st: State<St>, id: UrlPath<String>) -> Result<Json<Value>, ApiError> {
    undo_redo(st, id, true).await
}

async fn redo(st: State<St>, id: UrlPath<String>) -> Result<Json<Value>, ApiError> {
    undo_redo(st, id, false).await
}

async fn undo_redo(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    is_undo: bool,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let mut g = live.lock().await;
    if g.session.is_none() {
        return Err(busy());
    }
    let root = codesmith_core::config::project_root(&st.cwd);
    let snaps = Snapshots::for_project(&root).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "snapshots are not kept for this project".into(),
        )
    })?;
    let db = st.store()?;
    let changes = if is_undo {
        session::undo(&db, &id, &snaps)?
    } else {
        session::redo(&db, &id, &snaps)?
    };
    st.reload(&mut g, &id)?;
    // Undo restores files, not commands: the background ones keep running.
    wake(&live, &mut g);
    Ok(Json(
        json!({"files": changes.map(|c| c.into_iter().map(|c| c.path).collect::<Vec<_>>())}),
    ))
}

/// Where the generated token is kept for local clients to read.
pub fn token_path() -> PathBuf {
    codesmith_core::paths::data_dir().join("serve-token")
}

/// The token saved at `path`, or a new random one saved there (0600). The
/// boolean is true when it was just created.
pub fn load_or_create_token(path: &Path) -> Result<(String, bool)> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim();
        if t.len() >= 32 {
            codesmith_core::paths::tighten(path)?;
            return Ok((t.to_string(), false));
        }
    }
    let t = codesmith_core::paths::random_token(32);
    codesmith_core::paths::write_private(path, format!("{t}\n").as_bytes())
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    Ok((t, true))
}

pub async fn run(st: St, host: &str, port: u16) -> Result<()> {
    codesmith_core::automation::spawn_scheduler(&st.loaded);
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    eprintln!(
        "codesmith serving {} on http://{} (bearer token required)",
        st.cwd.display(),
        listener.local_addr()?,
    );
    serve_on(st, listener, host).await
}

/// Serve on an already bound listener; `host` is what it was bound as.
pub async fn serve_on(st: St, listener: tokio::net::TcpListener, host: &str) -> Result<()> {
    let _ = st.hosts.set(allowed_hosts(host, listener.local_addr()?));
    // Cheap: the branch watch finds its target in the background.
    st.watching();
    axum::serve(listener, router(st)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    // Generated media is announced by its metadata only; clients read the
    // bytes from the session's messages.
    #[test]
    fn media_events_carry_metadata_not_bytes() {
        let p = Arc::new(Prompter::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        p.follow(tx);
        let mut sink = JsonSink(p);
        let png = codesmith_ai::Media::base64("image/png", "iVBORw0KGgo=").named("cat.png");
        sink.media(&png);
        let ev = rx.try_recv().unwrap();
        assert_eq!(
            ev,
            json!({"type": "media", "mime": "image/png", "name": "cat.png", "size": 8})
        );
        let mut url = codesmith_ai::Media::base64("image/jpeg", "");
        url.source = codesmith_ai::Source::Url("https://example.com/a.jpg".into());
        sink.media(&url);
        assert_eq!(
            rx.try_recv().unwrap(),
            json!({"type": "media", "mime": "image/jpeg", "name": null, "url": "https://example.com/a.jpg"})
        );
    }

    // Nested subagents: events name the innermost one running, and the
    // parent's again once they end.
    #[test]
    fn child_events_name_the_innermost_subagent() {
        let p = Arc::new(Prompter::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        p.follow(tx);
        let mut sink = JsonSink(p);
        let child = |id: &str, parent: &str, depth| session::ChildSession {
            id: id.into(),
            parent_id: parent.into(),
            depth,
            title: format!("t-{id}"),
        };
        sink.tool_start("t1", "subagent", &json!({"agent": "general"}));
        sink.child_start(&child("c1", "top", 1));
        sink.tool_start("t2", "general › subagent", &json!({"agent": "explore"}));
        sink.child_start(&child("c2", "c1", 2));
        sink.child_text("deep");
        sink.child_end("c2", "completed", None);
        sink.tool_end("t2", "general › subagent", "ok", false);
        sink.child_end("c1", "failed", Some("boom"));
        sink.tool_end("t1", "subagent", "failed", true);
        let evs: Vec<Value> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let s = |i: usize| evs[i].get("session").cloned().unwrap_or(Value::Null);
        assert_eq!(evs[1]["tool"], "t1");
        assert_eq!(evs[1]["agent"], "general");
        assert_eq!(
            (s(2), &evs[3]["tool"], &evs[3]["agent"]),
            (json!("c1"), &json!("t2"), &json!("explore"))
        );
        assert_eq!(
            (s(3), &evs[3]["parent"], &evs[3]["depth"]),
            (json!("c2"), &json!("c1"), &json!(2))
        );
        assert_eq!((s(4), &evs[4]["delta"]), (json!("c2"), &json!("deep")));
        assert_eq!(s(6), json!("c1"), "{:?}", evs[6]);
        assert_eq!(
            evs[7],
            json!({"type": "child_end", "session": "c1", "status": "failed", "error": "boom"})
        );
        assert_eq!(s(8), Value::Null, "the parent's own call");
    }

    /// An OpenAI-compatible model: asks to run `echo served`, then reports the result.
    async fn fake_model() -> String {
        async fn chat(Json(b): Json<Value>) -> Response {
            let last = b["messages"].as_array().unwrap().last().unwrap().clone();
            let chunks: Vec<Value> = if last["content"].is_array() {
                // A prompt with attachments: say what came, run nothing.
                let parts = last["content"].clone();
                vec![
                    json!({"choices": [{"delta": {"content": format!("got: {parts}")}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
                ]
            } else if b.get("tools").is_none() {
                vec![
                    json!({"choices": [{"delta": {"content": "A title"}, "finish_reason": "stop"}]}),
                ]
            } else if last["role"] == "tool" {
                vec![
                    json!({"choices": [{"delta": {"content": format!("done: {}", last["content"].as_str().unwrap_or(""))}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
                ]
            } else {
                vec![
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"echo served\", \"description\": \"echo\"}"}}]}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
                ]
            };
            let mut body = String::new();
            for c in chunks {
                body.push_str(&format!("data: {c}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            ([("content-type", "text/event-stream")], body).into_response()
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, Router::new().route("/v1/chat/completions", post(chat)))
                .await
                .unwrap()
        });
        format!("http://{addr}/v1")
    }

    const TOKEN: &str = "test-token";

    /// A client that sends the test token on every request.
    fn client() -> reqwest::Client {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        reqwest::Client::builder()
            .default_headers(h)
            .build()
            .unwrap()
    }

    async fn start(dir: &std::path::Path, token: &str) -> String {
        start_with(dir, token, json!([])).await
    }

    /// A server whose config adds `permissions` to asking for shell commands.
    async fn start_with(dir: &std::path::Path, token: &str, permissions: Value) -> String {
        let base = fake_model().await;
        let mut rules = vec![json!({"action": "shell", "resource": "*", "effect": "ask"})];
        rules.extend(permissions.as_array().cloned().unwrap_or_default());
        let config = serde_json::from_value(json!({
            "model": "fake/m",
            "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
            "permissions": rules
        }))
        .unwrap();
        let loaded = Loaded {
            config,
            files: vec![],
            root: dir.to_path_buf(),
        };
        let st = Server::new(
            loaded,
            dir.to_path_buf(),
            dir.join("sessions.db"),
            false,
            token.to_string(),
            codesmith_core::mcp::Mcp::empty(),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { serve_on(st, l, "127.0.0.1").await.unwrap() });
        format!("http://{addr}")
    }

    /// Send a message and follow its event stream, answering permission requests.
    async fn turn(
        c: &reqwest::Client,
        base: &str,
        id: &str,
        text: &str,
        reply: Value,
    ) -> Vec<Value> {
        turn_with(c, base, id, json!({"text": text}), reply).await
    }

    /// `turn` with a whole message body.
    async fn turn_with(
        c: &reqwest::Client,
        base: &str,
        id: &str,
        body: Value,
        reply: Value,
    ) -> Vec<Value> {
        let resp = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let mut stream = resp.bytes_stream();
        let (mut buf, mut events) = (String::new(), Vec::new());
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            while let Some(i) = buf.find("\n\n") {
                let block: String = buf.drain(..i + 2).collect();
                let Some(data) = block.lines().find_map(|l| l.strip_prefix("data: ")) else {
                    continue;
                };
                let v: Value = serde_json::from_str(data).unwrap();
                if v["type"] == "permission" {
                    let rid = v["id"].as_str().unwrap();
                    assert!(
                        rid.starts_with("perm_") && rid.len() == 5 + 32,
                        "ids are random: {rid}"
                    );
                    let r = c
                        .post(format!(
                            "{base}/sessions/{id}/permissions/{}",
                            v["id"].as_str().unwrap()
                        ))
                        .json(&reply)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(r.status(), 200);
                }
                events.push(v);
            }
        }
        events
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_with_an_approved_and_a_rejected_command() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        assert_eq!(
            c.get(format!("{base}/health"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["ok"],
            true
        );

        let s: Value = c
            .post(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = s["id"].as_str().unwrap().to_string();
        assert_eq!(s["model"], "fake/m");

        let ev = turn(&c, &base, &id, "run it", json!({"reply": "once"})).await;
        let kinds: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert!(
            kinds.contains(&"permission") && kinds.contains(&"tool_start"),
            "{kinds:?}"
        );
        let end = ev.iter().find(|e| e["type"] == "tool_end").unwrap();
        assert_eq!(end["error"], false);
        assert!(end["output"].as_str().unwrap().contains("served"));
        assert!(ev
            .iter()
            .any(|e| e["type"] == "text" && e["delta"].as_str().unwrap().starts_with("done:")));
        assert_eq!(ev.last().unwrap()["type"], "done");
        assert!(ev.last().unwrap()["error"].is_null());

        let ev = turn(
            &c,
            &base,
            &id,
            "again",
            json!({"reply": "reject", "message": "not now"}),
        )
        .await;
        let end = ev.iter().find(|e| e["type"] == "tool_end").unwrap();
        assert_eq!(end["error"], true);
        assert!(end["output"].as_str().unwrap().contains("not now"), "{end}");

        let shown: Value = c
            .get(format!("{base}/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(shown["busy"], false);
        assert_eq!(
            shown["messages"].as_array().unwrap().len(),
            8,
            "two turns of user, call, result, answer"
        );
        let listed: Value = c
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(listed
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == id.as_str()));

        let bad = c
            .post(format!("{base}/sessions/{id}/permissions/perm_1"))
            .json(&json!({"reply": "once"}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 404);
        let missing = c
            .get(format!("{base}/sessions/ses_nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        let del: Value = c
            .delete(format!("{base}/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(del["deleted"], true);
        assert_eq!(
            c.get(format!("{base}/sessions/{id}"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }

    /// The next `data:` event of an SSE stream.
    async fn next_event(
        stream: &mut (impl futures_util::Stream<Item = reqwest::Result<axum::body::Bytes>> + Unpin),
        buf: &mut String,
    ) -> Option<Value> {
        loop {
            if let Some(i) = buf.find("\n\n") {
                let block: String = buf.drain(..i + 2).collect();
                match block.lines().find_map(|l| l.strip_prefix("data: ")) {
                    Some(data) => return Some(serde_json::from_str(data).unwrap()),
                    None => continue,
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(30), stream.next())
                .await
                .expect("timed out waiting for an event")?;
            buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    }

    /// Start a turn and read its stream up to the permission request, then
    /// drop the stream (the client went without answering).
    async fn ask_and_leave(c: &reqwest::Client, base: &str, id: &str) -> Value {
        let resp = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": "run it"}))
            .send()
            .await
            .unwrap();
        let (mut stream, mut buf) = (resp.bytes_stream(), String::new());
        loop {
            let v = next_event(&mut stream, &mut buf).await.unwrap();
            if v["type"] == "permission" {
                return v;
            }
        }
    }

    /// Fixed: a permission request shown to a client that went (or one
    /// asked before a follower connected) was never shown again, so the
    /// turn waited forever. A follower connecting while it is open is
    /// shown it, and can answer it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_follower_is_shown_the_open_permission_request() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        let id = new_session(&c, &base).await;
        let asked = ask_and_leave(&c, &base, &id).await;
        let resp = c
            .get(format!("{base}/sessions/{id}/events"))
            .send()
            .await
            .unwrap();
        let (mut stream, mut buf) = (resp.bytes_stream(), String::new());
        let shown = next_event(&mut stream, &mut buf).await.unwrap();
        assert_eq!(shown, asked);
        let r = c
            .post(format!(
                "{base}/sessions/{id}/permissions/{}",
                asked["id"].as_str().unwrap()
            ))
            .json(&json!({"reply": "once"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut seen = vec![];
        loop {
            let v = next_event(&mut stream, &mut buf).await.unwrap();
            if v["type"] == "done" {
                assert!(v["error"].is_null(), "{v}");
                break;
            }
            seen.push(v);
        }
        let end = seen.iter().find(|e| e["type"] == "tool_end").unwrap();
        assert_eq!(end["error"], false, "{seen:?}");
        assert!(
            seen.iter().all(|e| e["type"] != "permission"),
            "shown once: {seen:?}"
        );
    }

    /// Fixed: with its client gone and no follower, a permission request
    /// kept the turn (and the session) busy until an abort. It is refused
    /// once no client could see it for a while.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unseen_permission_request_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        let id = new_session(&c, &base).await;
        ask_and_leave(&c, &base, &id).await;
        let shown = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let shown: Value = c
                    .get(format!("{base}/sessions/{id}"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if shown["busy"] == false {
                    return shown;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("the turn still waits for an answer");
        let text = shown["messages"].to_string();
        assert!(
            text.contains("No client is connected to approve this."),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_sessions_and_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        let id = c
            .post(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        // Start a turn and leave its permission request unanswered.
        let resp = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": "run it"}))
            .send()
            .await
            .unwrap();
        let mut stream = resp.bytes_stream();
        let mut seen = String::new();
        while !seen.contains("event: permission") {
            seen.push_str(&String::from_utf8_lossy(
                &stream.next().await.unwrap().unwrap(),
            ));
        }
        let again = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 409);
        let a: Value = c
            .post(format!("{base}/sessions/{id}/abort"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(a["aborted"], true);
        let mut rest = String::new();
        while let Some(Ok(ch)) = stream.next().await {
            rest.push_str(&String::from_utf8_lossy(&ch));
        }
        assert!(rest.contains("\"error\":\"aborted\""), "{rest}");
        let shown: Value = c
            .get(format!("{base}/sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(shown["busy"], false, "the session is usable again");
        assert_eq!(
            shown["messages"][0]["parts"][0]["data"], "run it",
            "reloaded from the store"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bearer_token() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), "s3cret").await;
        let c = reqwest::Client::new();
        for path in ["/health", "/sessions"] {
            assert_eq!(
                c.get(format!("{base}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401,
                "no token on {path}"
            );
        }
        assert_eq!(
            c.post(format!("{base}/sessions"))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "a bodyless POST needs the token too"
        );
        assert_eq!(
            c.get(format!("{base}/health"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            c.get(format!("{base}/health"))
                .bearer_auth("s3cret")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn token_from_the_saved_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data/serve-token");
        let (t, new) = load_or_create_token(&path).unwrap();
        assert!(new && t.len() == 64, "{t}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let (again, new) = load_or_create_token(&path).unwrap();
        assert!(!new);
        assert_eq!(again, t, "later servers reuse the saved token");

        let base = start(dir.path(), &t).await;
        let saved = std::fs::read_to_string(&path).unwrap();
        let r = reqwest::Client::new()
            .get(format!("{base}/health"))
            .bearer_auth(saved.trim())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn web_pages_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let port = base.rsplit(':').next().unwrap().to_string();
        let c = client();

        // DNS rebinding: the page's own name in Host, even with the token.
        let r = c
            .post(format!("{base}/sessions"))
            .header("host", format!("attacker.example:{port}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .get(format!("{base}/sessions"))
            .header("host", "127.0.0.1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "the port must match");
        for ok in [
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
        ] {
            let r = c
                .get(format!("{base}/health"))
                .header("host", &ok)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{ok}");
        }

        // Cross-site requests carry a foreign Origin.
        for origin in [
            format!("http://attacker.example:{port}"),
            "null".to_string(),
            format!("http://localhost:{port}"),
        ] {
            let r = c
                .post(format!("{base}/sessions"))
                .header("origin", &origin)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{origin}");
        }
        let r = c
            .get(format!("{base}/health"))
            .header("origin", format!("http://127.0.0.1:{port}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "the server's own origin is fine");

        // Simple (preflight-free) bodies are not JSON.
        let r = c
            .post(format!("{base}/sessions"))
            .header("content-type", "text/plain")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 415);
        let r = c
            .post(format!("{base}/sessions"))
            .header("content-type", "application/json; charset=utf-8")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[test]
    fn hosts_follow_the_bound_address() {
        let lo: SocketAddr = "127.0.0.1:4096".parse().unwrap();
        let h = allowed_hosts("127.0.0.1", lo).unwrap();
        assert!(h.contains(&"localhost:4096".to_string()));
        assert!(!h.iter().any(|x| x.contains("example")));
        let lan: SocketAddr = "192.168.1.5:4096".parse().unwrap();
        let h = allowed_hosts("box.lan", lan).unwrap();
        assert_eq!(h, ["box.lan:4096", "192.168.1.5:4096"]);
        assert!(allowed_hosts("0.0.0.0", "0.0.0.0:4096".parse().unwrap()).is_none());
        assert!(allowed_hosts("::", "[::]:80".parse().unwrap()).is_none());
        let h = allowed_hosts("localhost", "[::1]:80".parse().unwrap()).unwrap();
        assert!(h.contains(&"localhost".to_string()) && h.contains(&"[::1]:80".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn large_bodies_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        let id = c
            .post(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": "x".repeat(MAX_BODY + 1)}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 413);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_of_other_projects_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let db = Store::open(&dir.path().join("sessions.db")).unwrap();
        db.create(&codesmith_core::store::Info {
            id: "ses_elsewhere".into(),
            directory: "/some/other/project".into(),
            title: None,
            agent: "build".into(),
            model: "fake/m".into(),
            parent_id: None,
            created: 1,
            updated: 1,
            usage: Usage::default(),
        })
        .unwrap();
        let c = client();
        let id = "ses_elsewhere";
        assert_eq!(
            c.get(format!("{base}/sessions/{id}"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        for route in ["abort", "undo", "redo"] {
            let r = c
                .post(format!("{base}/sessions/{id}/{route}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{route}");
        }
        let r = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let r = c
            .delete(format!("{base}/sessions/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(db.get(id).unwrap().is_some(), "not deleted");
    }

    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    /// The session environment, the jobs list, backgrounding and cancelling,
    /// and the shell ID a running command reports.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_environment_and_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let base = start(dir.path(), TOKEN).await;
        let c = client();
        let id = new_session(&c, &base).await;
        let url = |p: &str| format!("{base}/sessions/{id}/{p}");
        let get = |u: String| {
            let c = c.clone();
            async move {
                c.get(u)
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            }
        };
        assert_eq!(get(url("environment")).await, json!({"variables": null}));
        let r = c
            .put(url("environment"))
            .json(&json!({"variables": {"TOOLCHAIN": "session"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.json::<Value>().await.unwrap(),
            json!({"variables": {"TOOLCHAIN": "session"}})
        );
        assert_eq!(
            get(url("environment")).await,
            json!({"variables": {"TOOLCHAIN": "session"}})
        );
        let r = c
            .put(url("environment"))
            .json(&json!({"variables": {"N": 1}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(get(url("jobs")).await, json!({"data": []}));
        let r: Value = c
            .post(url("background"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r, json!({"backgrounded": []}));
        let r: Value = c
            .post(url("jobs/shell_9/cancel"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r, json!({"cancelled": false}));

        let ev = turn(&c, &base, &id, "go", json!({"reply": "once"})).await;
        let progress: Vec<&Value> = ev.iter().filter(|e| e["type"] == "tool_progress").collect();
        assert_eq!(progress.len(), 1, "{ev:?}");
        assert_eq!(progress[0]["metadata"], json!({"shellID": "shell_1"}));
        // Unknown sessions are not found.
        let r = c
            .get(format!("{base}/sessions/nope/jobs"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    async fn new_session(c: &reqwest::Client, base: &str) -> String {
        let s: Value = c
            .post(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        s["id"].as_str().unwrap().to_string()
    }

    fn said(ev: &[Value]) -> String {
        ev.iter()
            .filter(|e| e["type"] == "text")
            .filter_map(|e| e["delta"].as_str())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attachments_inline_and_by_path() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(PIXEL)
            .unwrap();
        std::fs::write(root.join("shot.png"), &png).unwrap();
        std::fs::write(root.join("notes.txt"), "text").unwrap();
        std::fs::write(root.join("secret.png"), &png).unwrap();
        // Outside the project (and the temp dir, which is allowed).
        let far = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let base = start_with(
            &root,
            TOKEN,
            json!([
                {"action": "read", "resource": "*.png", "effect": "ask"},
                {"action": "external_directory", "resource": "*", "effect": "ask"},
                {"action": "read", "resource": "secret*", "effect": "deny"}
            ]),
        )
        .await;
        let c = client();
        let id = new_session(&c, &base).await;
        let url = format!("data:image/png;base64,{PIXEL}");

        // Inline data needs no permission.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"mime": "image/png", "data": PIXEL, "name": "a.png"}]}),
            json!({"reply": "reject"}),
        )
        .await;
        assert!(!ev.iter().any(|e| e["type"] == "permission"), "{ev:?}");
        assert!(said(&ev).contains(&url), "{ev:?}");
        assert!(ev.last().unwrap()["error"].is_null());

        // A path is asked about as `read` would ask; a refusal stops the turn.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"path": "shot.png"}]}),
            json!({"reply": "reject"}),
        )
        .await;
        let asked = ev.iter().find(|e| e["type"] == "permission").unwrap();
        assert_eq!(asked["action"], "read");
        assert_eq!(asked["resources"], json!(["shot.png"]));
        let done = ev.last().unwrap();
        assert!(
            done["error"]
                .as_str()
                .unwrap()
                .contains("attachment shot.png"),
            "{done}"
        );
        assert!(!ev.iter().any(|e| e["type"] == "text"));

        // Approved, it reaches the model.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"path": "shot.png"}]}),
            json!({"reply": "once"}),
        )
        .await;
        assert!(said(&ev).contains(&url), "{ev:?}");

        // A denied path is refused without asking.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"path": "secret.png"}]}),
            json!({"reply": "once"}),
        )
        .await;
        assert!(!ev.iter().any(|e| e["type"] == "permission"), "{ev:?}");
        assert!(ev.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("attachment secret.png"));

        // Outside the project, external_directory is asked first.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"path": far}]}),
            json!({"reply": "reject"}),
        )
        .await;
        let asked: Vec<&Value> = ev.iter().filter(|e| e["type"] == "permission").collect();
        assert_eq!(asked.len(), 1, "{ev:?}");
        assert_eq!(asked[0]["action"], "external_directory");
        assert!(ev.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("attachment"));

        // Text files are not attachments; bad bodies are refused up front.
        let ev = turn_with(
            &c,
            &base,
            &id,
            json!({"text": "look", "attachments": [{"path": "notes.txt"}]}),
            json!({"reply": "once"}),
        )
        .await;
        assert!(ev.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("not an image, PDF or video"));
        for body in [
            json!({"text": "x", "attachments": [{"data": "bm90IGFuIGltYWdl"}]}),
            json!({"text": "x", "attachments": [{"data": "%%%"}]}),
            json!({"text": "x", "attachments": [{}]}),
            json!({"text": "x", "attachments": "shot.png"}),
        ] {
            let r = c
                .post(format!("{base}/sessions/{id}/messages"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{body}");
        }
    }
}
