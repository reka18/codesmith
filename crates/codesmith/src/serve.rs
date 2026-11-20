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
//! - `POST   /sessions/{id}/messages {text}` → SSE: text, reasoning,
//!   tool_start, tool_end, step_end, retry, compacted, todos, notice,
//!   permission, question, done
//! - `POST   /sessions/{id}/abort`
//! - `POST   /sessions/{id}/permissions/{rid} {reply: once|always|reject, message?}`
//! - `POST   /sessions/{id}/questions/{qid} {answers: [[...]]}` or `{dismiss: true}`
//! - `POST   /sessions/{id}/undo` · `POST /sessions/{id}/redo`

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Request as HttpRequest, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codesmith_ai::Usage;
use codesmith_core::config::Loaded;
use codesmith_core::permission::{Asker, Fixed, Reply, Request};
use codesmith_core::question::{Prompt, Questioner};
use codesmith_core::session::{self, Options, Session, Sink};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::Store;
use codesmith_core::tool::todo::Todo;
use futures_util::Stream;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

type Tx = mpsc::UnboundedSender<Value>;

enum Pending {
    Permission(oneshot::Sender<Reply>),
    Question(oneshot::Sender<Option<Vec<Vec<String>>>>),
}

/// Routes a session's permission requests and questions to whichever client
/// is streaming its current turn.
#[derive(Default)]
struct Prompter {
    events: Mutex<Option<Tx>>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Prompter {
    fn set(&self, tx: Option<Tx>) {
        *self.events.lock().unwrap() = tx;
    }
    fn send(&self, v: Value) -> bool {
        self.events
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(v).is_ok())
    }
    /// Drop every open prompt (each is then rejected or dismissed).
    fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
    /// An unguessable id, so a client can only answer prompts it was shown.
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", codesmith_core::paths::random_token(16))
    }
}

impl Asker for Prompter {
    fn ask(&self, req: &Request) -> Reply {
        let (tx, rx) = oneshot::channel();
        let id = self.id("perm");
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending::Permission(tx));
        let ev = json!({"type": "permission", "id": id, "action": req.action, "resources": req.resources, "save": req.save, "title": req.title});
        if !self.send(ev) {
            self.pending.lock().unwrap().remove(&id);
            return Reply::Reject(Some("No client is connected to approve this.".into()));
        }
        rx.blocking_recv().unwrap_or(Reply::Reject(None))
    }
}

impl Questioner for Prompter {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>> {
        let (tx, rx) = oneshot::channel();
        let id = self.id("q");
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending::Question(tx));
        if !self.send(json!({"type": "question", "id": id, "prompts": prompts})) {
            self.pending.lock().unwrap().remove(&id);
            return None;
        }
        rx.blocking_recv().ok().flatten()
    }
}

struct JsonSink(Tx);

impl Sink for JsonSink {
    fn text(&mut self, delta: &str) {
        let _ = self.0.send(json!({"type": "text", "delta": delta}));
    }
    fn reasoning(&mut self, delta: &str) {
        let _ = self.0.send(json!({"type": "reasoning", "delta": delta}));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        let _ = self
            .0
            .send(json!({"type": "tool_start", "id": id, "name": name, "input": input}));
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        let _ = self.0.send(json!({"type": "tool_end", "id": id, "name": name, "output": output, "error": is_error}));
    }
    fn step_end(&mut self, reason: &str, usage: &Usage) {
        let _ = self
            .0
            .send(json!({"type": "step_end", "reason": reason, "usage": usage}));
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        let _ = self.0.send(json!({"type": "retry", "attempt": attempt, "error": error, "wait_ms": wait.as_millis() as u64}));
    }
    fn compacted(&mut self, before: u64, after: u64) {
        let _ = self
            .0
            .send(json!({"type": "compacted", "before": before, "after": after}));
    }
    fn todos(&mut self, todos: &[Todo]) {
        let _ = self.0.send(json!({"type": "todos", "todos": todos}));
    }
    fn notice(&mut self, message: &str) {
        let _ = self.0.send(json!({"type": "notice", "message": message}));
    }
}

/// A session held in memory. `session` is `None` while a turn runs.
struct Live {
    session: Option<Session>,
    task: Option<tokio::task::JoinHandle<()>>,
    prompter: Arc<Prompter>,
}

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
        Arc::new(Server {
            loaded,
            cwd,
            db,
            auto,
            token,
            mcp,
            hosts: OnceLock::new(),
            live: Default::default(),
        })
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
        let prompter = Arc::new(Prompter::default());
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
            session: Some(s),
            task: None,
            prompter,
        })
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
    Router::new()
        .route("/health", get(health))
        .route("/sessions", get(list).post(create))
        .route("/sessions/{id}", get(show).delete(remove))
        .route("/sessions/{id}/messages", post(message))
        .route("/sessions/{id}/abort", post(abort))
        .route("/sessions/{id}/permissions/{rid}", post(answer_permission))
        .route("/sessions/{id}/questions/{qid}", post(answer_question))
        .route("/sessions/{id}/undo", post(undo))
        .route("/sessions/{id}/redo", post(redo))
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
            v["messages"] = json!(s.messages);
            v["todos"] = json!(s.todos);
            v["busy"] = json!(false);
            Ok(Json(v))
        }
        None => {
            let info = st.store()?.get(&id)?.ok_or_else(|| not_found(&id))?;
            Ok(Json(
                json!({"id": info.id, "title": info.title, "agent": info.agent, "model": info.model, "usage": info.usage, "messages": st.store()?.history(&id)?, "busy": true}),
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
    if let Some(l) = st.live.lock().await.remove(&id) {
        let mut g = l.lock().await;
        if let Some(t) = g.task.take() {
            g.prompter.clear();
            t.abort();
        }
    }
    let deleted = st.store()?.delete(&id)?;
    Ok(Json(json!({"deleted": deleted})))
}

async fn message(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<Value>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let text = b["text"]
        .as_str()
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "\"text\" is required".into()))?;
    let live = st.live(&id).await?;
    let mut g = live.lock().await;
    let mut s = g.session.take().ok_or_else(busy)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    g.prompter.set(Some(tx.clone()));
    let (live2, prompter) = (live.clone(), g.prompter.clone());
    g.task = Some(tokio::spawn(async move {
        let mut sink = JsonSink(tx.clone());
        let r = s.prompt(&text, &mut sink).await;
        let done = json!({"type": "done", "error": r.err().map(|e| format!("{e:#}")), "usage": s.usage, "title": s.title});
        let mut g = live2.lock().await;
        g.session = Some(s);
        g.task = None;
        prompter.set(None);
        drop(g);
        let _ = tx.send(done);
    }));
    drop(g);
    let stream = async_stream::stream! {
        while let Some(v) = rx.recv().await {
            let kind = v["type"].as_str().unwrap_or("message").to_string();
            yield Ok(Event::default().event(kind).data(v.to_string()));
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn abort(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let live = st.live(&id).await?;
    let mut g = live.lock().await;
    let Some(t) = g.task.take() else {
        return Ok(Json(json!({"aborted": false})));
    };
    t.abort();
    g.prompter.clear();
    g.prompter.send(json!({"type": "done", "error": "aborted"}));
    g.prompter.set(None);
    // Every message is already stored; reload the session from there.
    let fresh = st
        .make(Some(id.clone()), None, None)
        .or_else(|_| st.make(None, None, None))?;
    g.session = fresh.session;
    g.prompter = fresh.prompter;
    Ok(Json(json!({"aborted": true})))
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
        Some(Pending::Permission(tx)) => {
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
        Some(Pending::Question(tx)) => {
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
    let fresh = st.make(Some(id.clone()), None, None)?;
    g.session = fresh.session;
    g.prompter = fresh.prompter;
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
    axum::serve(listener, router(st)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// An OpenAI-compatible model: asks to run `echo served`, then reports the result.
    async fn fake_model() -> String {
        async fn chat(Json(b): Json<Value>) -> Response {
            let last = b["messages"].as_array().unwrap().last().unwrap().clone();
            let chunks: Vec<Value> = if b.get("tools").is_none() {
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
        let base = fake_model().await;
        let config = serde_json::from_value(json!({
            "model": "fake/m",
            "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]
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
        let resp = c
            .post(format!("{base}/sessions/{id}/messages"))
            .json(&json!({"text": text}))
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
}
