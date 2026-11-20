//! `codesmith acp`: the Agent Client Protocol (v1) over stdio, so editors
//! such as Zed can drive CodeSmith sessions. Newline-delimited JSON-RPC 2.0
//! on stdin/stdout; logs go to stderr.

use anyhow::{anyhow, Result};
use codesmith_ai::{Part, Role, Usage};
use codesmith_core::config::Loaded;
use codesmith_core::mcp::{Mcp, ServerConfig};
use codesmith_core::permission::{Asker, Reply, Request};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use codesmith_core::tool::todo::{Status, Todo};
use codesmith_core::{agent, command, config};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const PROTOCOL_VERSION: u64 = 1;

type Waiter = oneshot::Sender<Result<Value, Value>>;

/// The connection: outgoing messages and our requests awaiting the client.
#[derive(Clone)]
struct Conn {
    out: mpsc::UnboundedSender<Value>,
    /// id → (session, reply)
    pending: Arc<Mutex<HashMap<u64, (String, Waiter)>>>,
    next: Arc<AtomicU64>,
}

impl Conn {
    fn send(&self, v: Value) {
        let _ = self.out.send(v);
    }
    fn respond(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }
    fn update(&self, session: &str, update: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}}));
    }
    fn request(
        &self,
        session: &str,
        method: &str,
        params: Value,
    ) -> oneshot::Receiver<Result<Value, Value>> {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, (session.to_string(), tx));
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx
    }
    /// Drop a session's open requests (their waiters see a cancellation).
    fn drop_pending(&self, session: &str) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, (s, _)| s != session);
    }
}

pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "skill" => "read",
        "write" | "edit" => "edit",
        "glob" | "grep" => "search",
        "shell" => "execute",
        "webfetch" | "websearch" => "fetch",
        "todowrite" | "subagent" => "think",
        _ => "other",
    }
}

fn locations(cwd: &Path, input: &Value) -> Value {
    match input["path"].as_str().or(input["file"].as_str()) {
        Some(p) => {
            let abs = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                cwd.join(p)
            };
            json!([{"path": abs}])
        }
        None => json!([]),
    }
}

fn plan(todos: &[Todo]) -> Value {
    let entries: Vec<Value> = todos
        .iter()
        .filter(|t| t.status != Status::Cancelled)
        .map(|t| {
            let status = match t.status {
                Status::InProgress => "in_progress",
                Status::Completed => "completed",
                _ => "pending",
            };
            json!({"content": t.content, "priority": "medium", "status": status})
        })
        .collect();
    json!({"sessionUpdate": "plan", "entries": entries})
}

fn text_chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = (0..=max)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}\n… ({} more characters)", &s[..cut], s.len() - cut)
}

/// The tool call that is running, so permission requests can point at it.
type Current = Arc<Mutex<Option<(String, String, Value)>>>;

struct AcpSink {
    conn: Conn,
    session: String,
    cwd: PathBuf,
    current: Current,
}

impl Sink for AcpSink {
    fn text(&mut self, delta: &str) {
        self.conn
            .update(&self.session, text_chunk("agent_message_chunk", delta));
    }
    fn reasoning(&mut self, delta: &str) {
        self.conn
            .update(&self.session, text_chunk("agent_thought_chunk", delta));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        *self.current.lock().unwrap() = Some((id.to_string(), name.to_string(), input.clone()));
        let title = format!("{name} {}", crate::render::describe(name, input))
            .trim()
            .to_string();
        self.conn.update(&self.session, json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": tool_kind(name), "status": "in_progress", "rawInput": input, "locations": locations(&self.cwd, input)}));
    }
    fn tool_end(&mut self, id: &str, _name: &str, output: &str, is_error: bool) {
        *self.current.lock().unwrap() = None;
        self.conn.update(&self.session, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": if is_error { "failed" } else { "completed" }, "content": [{"type": "content", "content": {"type": "text", "text": clip(output, 20_000)}}]}));
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.conn.update(&self.session, plan(todos));
    }
    fn step_end(&mut self, _reason: &str, _usage: &Usage) {}
}

/// Permission requests go to the client as `session/request_permission`.
struct AcpAsker {
    conn: Conn,
    session: String,
    current: Current,
}

impl Asker for AcpAsker {
    fn ask(&self, req: &Request) -> Reply {
        let (id, name, input) = self.current.lock().unwrap().clone().unwrap_or_else(|| {
            (
                format!("perm_{}", req.action),
                req.action.clone(),
                json!({}),
            )
        });
        let params = json!({
            "sessionId": self.session,
            "toolCall": {"toolCallId": id, "title": req.title, "kind": tool_kind(&name), "status": "pending", "rawInput": input},
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
            ],
        });
        let rx = self
            .conn
            .request(&self.session, "session/request_permission", params);
        match rx.blocking_recv() {
            Ok(Ok(v)) => match (
                v["outcome"]["outcome"].as_str(),
                v["outcome"]["optionId"].as_str(),
            ) {
                (Some("selected"), Some("allow_once")) => Reply::Once,
                (Some("selected"), Some("allow_always")) => Reply::Always,
                _ => Reply::Reject(None),
            },
            _ => Reply::Reject(None),
        }
    }
}

struct Slot {
    session: tokio::sync::Mutex<Option<Session>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The JSON-RPC id of the running `session/prompt`.
    prompt: Mutex<Option<Value>>,
    current: Current,
    loaded: Loaded,
    cwd: PathBuf,
    mcp: Arc<Mcp>,
}

pub type Loader = Arc<dyn Fn(&Path) -> Result<Loaded> + Send + Sync>;

pub struct Agent {
    conn: Conn,
    db: PathBuf,
    load: Loader,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Slot>>>,
}

/// Editor-supplied MCP servers, in CodeSmith's config shape.
pub fn mcp_servers(list: &Value) -> BTreeMap<String, ServerConfig> {
    let pairs = |v: &Value| -> BTreeMap<String, String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some((
                    e["name"].as_str()?.to_string(),
                    e["value"].as_str()?.to_string(),
                ))
            })
            .collect()
    };
    let mut out = BTreeMap::new();
    for s in list.as_array().into_iter().flatten() {
        let Some(name) = s["name"].as_str() else {
            continue;
        };
        let cfg = match s["type"].as_str() {
            Some("http") => {
                json!({"type": "remote", "url": s["url"], "headers": pairs(&s["headers"])})
            }
            Some(_) => continue,
            None => {
                let mut cmd = vec![s["command"].clone()];
                cmd.extend(s["args"].as_array().cloned().unwrap_or_default());
                json!({"type": "local", "command": cmd, "environment": pairs(&s["env"])})
            }
        };
        if let Ok(c) = serde_json::from_value::<ServerConfig>(cfg) {
            out.insert(name.to_string(), c);
        }
    }
    out
}

/// Prompt content blocks as one message.
pub fn prompt_text(blocks: &Value) -> String {
    let mut parts = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or("").to_string()),
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                match r["text"].as_str() {
                    Some(t) => parts.push(format!("<file uri=\"{uri}\">\n{t}\n</file>")),
                    None => parts.push(format!("[attached resource {uri} omitted: binary]")),
                }
            }
            Some("resource_link") => parts.push(format!(
                "Referenced: {} ({})",
                b["name"].as_str().unwrap_or(""),
                b["uri"]
                    .as_str()
                    .unwrap_or("")
                    .trim_start_matches("file://")
            )),
            Some(other) => parts.push(format!("[{other} content omitted: not supported]")),
            None => {}
        }
    }
    parts.join("\n\n")
}

fn modes(s: &Session, config: &config::Config) -> Value {
    let available: Vec<Value> = agent::list(config)
        .into_iter()
        .filter_map(|id| agent::resolve(config, &id))
        .filter(|a| a.mode != "subagent")
        .map(|a| json!({"id": a.id, "name": a.name, "description": a.description}))
        .collect();
    json!({"currentModeId": s.agent.id, "availableModes": available})
}

fn is_prompt(t: &str) -> bool {
    let t = t.trim_start();
    !["<shell ", "<system-reminder>", "<conversation-checkpoint>"]
        .iter()
        .any(|p| t.starts_with(p))
}

impl Agent {
    fn store(&self) -> Option<Store> {
        Store::open(&self.db).ok()
    }

    async fn open(
        &self,
        cwd: &Path,
        servers: &Value,
        resume: Option<String>,
        agent_id: Option<String>,
    ) -> Result<Arc<Slot>> {
        let mut loaded = (self.load)(cwd)?;
        loaded.config.mcp.servers.extend(mcp_servers(servers));
        let mcp = Mcp::connect(&loaded.config.mcp, &loaded.root).await;
        let current: Current = Default::default();
        let s = self.make(&loaded, cwd, &mcp, &current, resume, agent_id, None)?;
        Ok(Arc::new(Slot {
            session: tokio::sync::Mutex::new(Some(s)),
            task: Mutex::new(None),
            prompt: Mutex::new(None),
            current,
            loaded,
            cwd: cwd.to_path_buf(),
            mcp,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn make(
        &self,
        loaded: &Loaded,
        cwd: &Path,
        mcp: &Arc<Mcp>,
        current: &Current,
        resume: Option<String>,
        agent_id: Option<String>,
        keep_id: Option<&str>,
    ) -> Result<Session> {
        let mut id = keep_id.map(str::to_string);
        let asker = Arc::new(AcpAsker {
            conn: self.conn.clone(),
            session: String::new(),
            current: current.clone(),
        });
        let mut s = Session::new(
            loaded,
            cwd.to_path_buf(),
            Options {
                agent: agent_id,
                model: None,
                resume,
                title: None,
                parent: None,
                questioner: None,
                mcp: Some(mcp.clone()),
            },
            asker,
            self.store(),
        )?;
        if let Some(id) = id.take() {
            s.id = id;
        }
        // The asker needs the final session id.
        let asker = Arc::new(AcpAsker {
            conn: self.conn.clone(),
            session: s.id.clone(),
            current: current.clone(),
        });
        s.set_asker(asker);
        Ok(s)
    }

    async fn slot(&self, id: &str) -> Result<Arc<Slot>, (i64, String)> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or((-32602, format!("unknown session {id}; load it first")))
    }

    fn commands(&self, slot: &Slot) -> Value {
        let cmds: Vec<Value> = crate::all_commands(&slot.cwd, &slot.loaded)
            .into_iter()
            .map(|(name, c)| json!({"name": name, "description": c.description.unwrap_or_default(), "input": {"hint": "arguments"}}))
            .collect();
        json!({"sessionUpdate": "available_commands_update", "availableCommands": cmds})
    }

    fn replay(&self, id: &str, s: &Session) {
        for m in &s.messages {
            for p in &m.parts {
                match (&m.role, p) {
                    (Role::User, Part::Text(t)) if is_prompt(t) => {
                        self.conn.update(id, text_chunk("user_message_chunk", t))
                    }
                    (Role::Assistant, Part::Text(t)) => {
                        self.conn.update(id, text_chunk("agent_message_chunk", t))
                    }
                    (Role::Assistant, Part::Reasoning(t)) => {
                        self.conn.update(id, text_chunk("agent_thought_chunk", t))
                    }
                    (
                        Role::Assistant,
                        Part::ToolCall {
                            id: cid,
                            name,
                            input,
                        },
                    ) => {
                        let title = format!("{name} {}", crate::render::describe(name, input))
                            .trim()
                            .to_string();
                        self.conn.update(id, json!({"sessionUpdate": "tool_call", "toolCallId": cid, "title": title, "kind": tool_kind(name), "status": "in_progress", "rawInput": input, "locations": locations(&s.cwd, input)}));
                    }
                    (
                        Role::User,
                        Part::ToolResult {
                            id: cid,
                            content,
                            is_error,
                            ..
                        },
                    ) => {
                        self.conn.update(id, json!({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": if *is_error { "failed" } else { "completed" }, "content": [{"type": "content", "content": {"type": "text", "text": clip(content, 20_000)}}]}));
                    }
                    _ => {}
                }
            }
        }
    }

    async fn handle(self: &Arc<Self>, id: Value, method: &str, p: Value) {
        let r: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                    "mcpCapabilities": {"http": true, "sse": false},
                    "sessionCapabilities": {"list": {}, "delete": {}, "resume": {}, "close": {}},
                },
                "authMethods": [],
                "agentInfo": {"name": "codesmith", "title": "CodeSmith", "version": env!("CARGO_PKG_VERSION")},
            })),
            "authenticate" | "logout" => Ok(json!({})),
            "session/new" | "session/load" | "session/resume" => {
                self.open_session(method, &p).await
            }
            "session/list" => {
                let cwd = p["cwd"].as_str();
                let list = self
                    .store()
                    .map(|db| db.list(cwd, false).unwrap_or_default())
                    .unwrap_or_default();
                let sessions: Vec<Value> = list
                    .iter()
                    .map(|i| json!({"sessionId": i.id, "cwd": i.directory, "title": i.title, "updatedAt": chrono::DateTime::from_timestamp_millis(i.updated).map(|t| t.to_rfc3339())}))
                    .collect();
                Ok(json!({"sessions": sessions}))
            }
            "session/delete" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Some(slot) = self.sessions.lock().await.remove(&sid) {
                    self.stop(&sid, &slot, false).await;
                }
                self.store()
                    .map(|db| db.delete(&sid))
                    .transpose()
                    .map_err(|e| (-32603, format!("{e:#}")))
                    .map(|_| json!({}))
            }
            "session/close" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Some(slot) = self.sessions.lock().await.remove(&sid) {
                    self.stop(&sid, &slot, false).await;
                }
                Ok(json!({}))
            }
            "session/set_mode" => self.set_mode(&p).await,
            "session/prompt" => {
                // Answered when the turn ends.
                if let Err((code, msg)) = self.prompt(id.clone(), &p).await {
                    self.conn.error(&id, code, &msg);
                }
                return;
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        match r {
            Ok(v) => self.conn.respond(&id, v),
            Err((code, msg)) => self.conn.error(&id, code, &msg),
        }
    }

    async fn open_session(&self, method: &str, p: &Value) -> Result<Value, (i64, String)> {
        let cwd = p["cwd"]
            .as_str()
            .map(PathBuf::from)
            .filter(|c| c.is_absolute())
            .ok_or((-32602, "cwd must be an absolute path".to_string()))?;
        let resume = match method {
            "session/new" => None,
            _ => Some(
                p["sessionId"]
                    .as_str()
                    .ok_or((-32602, "sessionId is required".to_string()))?
                    .to_string(),
            ),
        };
        if let (Some(r), Some(db)) = (&resume, self.store()) {
            if db.get(r).ok().flatten().is_none() {
                return Err((-32602, format!("no session {r}")));
            }
        }
        let slot = self
            .open(&cwd, &p["mcpServers"], resume, None)
            .await
            .map_err(|e| (-32603, format!("{e:#}")))?;
        let (sid, modes) = {
            let g = slot.session.lock().await;
            let s = g.as_ref().unwrap();
            if method == "session/load" {
                self.replay(&s.id, s);
            }
            (s.id.clone(), modes(s, &slot.loaded.config))
        };
        self.sessions.lock().await.insert(sid.clone(), slot.clone());
        // Sent right after the response (the writer keeps order).
        let (conn, update) = (self.conn.clone(), self.commands(&slot));
        let sid2 = sid.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            conn.update(&sid2, update);
        });
        let mut out = json!({"modes": modes});
        if method == "session/new" {
            out["sessionId"] = json!(sid);
        }
        Ok(out)
    }

    async fn set_mode(&self, p: &Value) -> Result<Value, (i64, String)> {
        let sid = p["sessionId"].as_str().unwrap_or("");
        let mode = p["modeId"]
            .as_str()
            .ok_or((-32602, "modeId is required".to_string()))?;
        let slot = self.slot(sid).await?;
        let mut g = slot.session.lock().await;
        if g.is_none() {
            return Err((-32603, "a turn is running in this session".into()));
        }
        let stored = self
            .store()
            .and_then(|db| db.get(sid).ok().flatten())
            .is_some();
        let s = self
            .make(
                &slot.loaded,
                &slot.cwd,
                &slot.mcp,
                &slot.current,
                stored.then(|| sid.to_string()),
                Some(mode.to_string()),
                Some(sid),
            )
            .map_err(|e| (-32602, format!("{e:#}")))?;
        *g = Some(s);
        self.conn.update(
            sid,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": mode}),
        );
        Ok(json!({}))
    }

    async fn prompt(self: &Arc<Self>, rpc: Value, p: &Value) -> Result<(), (i64, String)> {
        let sid = p["sessionId"].as_str().unwrap_or("").to_string();
        let slot = self.slot(&sid).await?;
        let mut s = slot.session.lock().await.take().ok_or((
            -32603,
            "a turn is already running in this session".to_string(),
        ))?;
        *slot.prompt.lock().unwrap() = Some(rpc.clone());
        let text = prompt_text(&p["prompt"]);
        let (me, slot2) = (self.clone(), slot.clone());
        let task = tokio::spawn(async move {
            let mut sink = AcpSink {
                conn: me.conn.clone(),
                session: sid.clone(),
                cwd: slot2.cwd.clone(),
                current: slot2.current.clone(),
            };
            let title_before = s.title.clone();
            let result = me.run_turn(&mut s, &slot2, &text, &mut sink).await;
            if s.title != title_before {
                me.conn.update(
                    &sid,
                    json!({"sessionUpdate": "session_info_update", "title": s.title}),
                );
            }
            *slot2.session.lock().await = Some(s);
            slot2.task.lock().unwrap().take();
            let Some(rpc) = slot2.prompt.lock().unwrap().take() else {
                return;
            };
            match result {
                Ok(()) => me.conn.respond(&rpc, json!({"stopReason": "end_turn"})),
                Err(e) if e.to_string().contains("without a final answer") => me
                    .conn
                    .respond(&rpc, json!({"stopReason": "max_turn_requests"})),
                Err(e) => me.conn.error(&rpc, -32603, &format!("{e:#}")),
            }
        });
        *slot.task.lock().unwrap() = Some(task);
        Ok(())
    }

    async fn run_turn(
        &self,
        s: &mut Session,
        slot: &Slot,
        text: &str,
        sink: &mut AcpSink,
    ) -> Result<()> {
        if let Some((name, args)) = command::parse_invocation(text) {
            let all = crate::all_commands(&slot.cwd, &slot.loaded);
            if let Some(c) = all.get(&name) {
                if c.native {
                    let env = codesmith_core::review::Env::new(
                        &slot.loaded,
                        s.cwd.clone(),
                        Some(s.model_spec.clone()),
                        Some(self.db.clone()),
                        Some(s.id.clone()),
                        None,
                    );
                    let (conn, sid) = (self.conn.clone(), s.id.clone());
                    let progress = move |m: String| {
                        conn.update(&sid, text_chunk("agent_thought_chunk", &format!("{m}\n")))
                    };
                    let (report, usage) =
                        codesmith_core::review::run_command(&name, &args, &env, &progress).await?;
                    sink.text(&report);
                    return s.append_exchange(text, &report, &usage);
                }
                return s.prompt(&command::expand(&c.template, &args), sink).await;
            }
        }
        s.prompt(text, sink).await
    }

    /// Stop a running turn; the session reloads from the store.
    async fn stop(&self, sid: &str, slot: &Slot, reload: bool) {
        let task = slot.task.lock().unwrap().take();
        let Some(task) = task else { return };
        self.conn.drop_pending(sid);
        task.abort();
        let _ = task.await;
        if let Some(rpc) = slot.prompt.lock().unwrap().take() {
            self.conn.respond(&rpc, json!({"stopReason": "cancelled"}));
        }
        if reload {
            let stored = self
                .store()
                .and_then(|db| db.get(sid).ok().flatten())
                .is_some();
            if let Ok(s) = self.make(
                &slot.loaded,
                &slot.cwd,
                &slot.mcp,
                &slot.current,
                stored.then(|| sid.to_string()),
                None,
                Some(sid),
            ) {
                *slot.session.lock().await = Some(s);
            }
        }
    }

    async fn notification(self: &Arc<Self>, method: &str, p: Value) {
        if method == "session/cancel" {
            let sid = p["sessionId"].as_str().unwrap_or("").to_string();
            if let Ok(slot) = self.slot(&sid).await {
                self.stop(&sid, &slot, true).await;
            }
        }
    }
}

/// Serve ACP on any reader/writer pair (stdin/stdout in production).
pub async fn serve<R, W>(reader: R, mut writer: W, db: PathBuf, load: Loader) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let conn = Conn {
        out: tx,
        pending: Default::default(),
        next: Default::default(),
    };
    let agent = Arc::new(Agent {
        conn: conn.clone(),
        db,
        load,
        sessions: Default::default(),
    });
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            conn.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}));
            continue;
        };
        let method = m["method"].as_str().map(str::to_string);
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        match (m.get("id").cloned().filter(|v| !v.is_null()), method) {
            (Some(id), Some(method)) => {
                let a = agent.clone();
                tokio::spawn(async move { a.handle(id, &method, params).await });
            }
            (None, Some(method)) => agent.notification(&method, params).await,
            (Some(id), None) => {
                let waiter = id
                    .as_u64()
                    .and_then(|n| conn.pending.lock().unwrap().remove(&n));
                if let Some((_, w)) = waiter {
                    let _ = w.send(match m.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
                    });
                }
            }
            (None, None) => {}
        }
    }
    // Input closed: stop running turns, flush what is queued.
    for (sid, slot) in agent.sessions.lock().await.drain() {
        agent.stop(&sid, &slot, false).await;
    }
    drop(agent);
    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer_task).await;
    Ok(())
}

pub async fn run() -> Result<()> {
    let load: Loader = Arc::new(|cwd: &Path| config::load(cwd));
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(l) = config::load(&cwd) {
            codesmith_core::automation::spawn_scheduler(&l);
        }
    }
    serve(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        codesmith_core::store::default_path(),
        load,
    )
    .await
    .map_err(|e| anyhow!("acp: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn fake_model() -> String {
        async fn chat(Json(b): Json<Value>) -> axum::response::Response {
            let last = b["messages"].as_array().unwrap().last().unwrap().clone();
            let chunk = if b.get("tools").is_none() {
                json!({"choices": [{"delta": {"content": "A title"}, "finish_reason": "stop"}]})
            } else if last["role"] == "tool" {
                json!({"choices": [{"delta": {"content": format!("done: {}", last["content"].as_str().unwrap_or(""))}, "finish_reason": "stop"}]})
            } else {
                json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"echo acp\", \"description\": \"echo\"}"}}]}, "finish_reason": "tool_calls"}]})
            };
            (
                [("content-type", "text/event-stream")],
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
                .into_response()
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

    struct Client {
        w: tokio::io::DuplexStream,
        r: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        next: u64,
    }

    impl Client {
        async fn send(&mut self, v: Value) {
            self.w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        }
        async fn call(&mut self, method: &str, params: Value) -> u64 {
            self.next += 1;
            let id = self.next;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            id
        }
        async fn recv(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), self.r.next_line())
                .await
                .expect("timed out")
                .unwrap()
                .unwrap();
            serde_json::from_str(&line).unwrap()
        }
        /// Read until the response to `id`, collecting everything before it.
        async fn until(
            &mut self,
            id: u64,
            mut on_request: impl FnMut(&Value) -> Option<Value>,
        ) -> (Value, Vec<Value>) {
            let mut seen = Vec::new();
            loop {
                let m = self.recv().await;
                if m["id"] == json!(id) && m.get("method").is_none() {
                    return (m, seen);
                }
                if m.get("method").is_some() && m.get("id").is_some() {
                    if let Some(result) = on_request(&m) {
                        self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result": result}))
                            .await;
                    }
                }
                seen.push(m);
            }
        }
    }

    fn start(dir: &Path, base: &str) -> Client {
        let (client_w, server_r) = tokio::io::duplex(1 << 20);
        let (server_w, client_r) = tokio::io::duplex(1 << 20);
        let config: config::Config = serde_json::from_value(json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]
        }))
        .unwrap();
        let load: Loader = Arc::new(move |cwd: &Path| {
            Ok(Loaded {
                config: config.clone(),
                files: vec![],
                root: cwd.to_path_buf(),
            })
        });
        let db = dir.join("acp.db");
        tokio::spawn(async move {
            serve(BufReader::new(server_r), server_w, db, load)
                .await
                .unwrap()
        });
        Client {
            w: client_w,
            r: BufReader::new(client_r).lines(),
            next: 0,
        }
    }

    fn updates<'a>(seen: &'a [Value], kind: &str) -> Vec<&'a Value> {
        seen.iter()
            .filter(|m| {
                m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == kind
            })
            .map(|m| &m["params"]["update"])
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_from_initialize_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let base = fake_model().await;
        let mut c = start(&cwd, &base);
        let id = c
            .call(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await;
        let (init, _) = c.until(id, |_| None).await;
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);

        let id = c
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let (new, _) = c.until(id, |_| None).await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        assert_eq!(new["result"]["modes"]["currentModeId"], "build");
        assert!(new["result"]["modes"]["availableModes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "plan"));

        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        let mut asked = 0;
        let (done, seen) = c
            .until(id, |m| {
                assert_eq!(m["method"], "session/request_permission");
                assert_eq!(
                    m["params"]["toolCall"]["toolCallId"], "c1",
                    "the request points at the running tool call"
                );
                asked += 1;
                Some(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}))
            })
            .await;
        assert_eq!(asked, 1);
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert!(
            updates(&seen, "available_commands_update")[0]["availableCommands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "review")
        );
        let call = updates(&seen, "tool_call")[0];
        assert_eq!(
            (call["kind"].as_str(), call["status"].as_str()),
            (Some("execute"), Some("in_progress"))
        );
        let upd = updates(&seen, "tool_call_update")[0];
        assert_eq!(upd["status"], "completed");
        assert!(upd["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("acp"));
        let text: String = updates(&seen, "agent_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert!(text.starts_with("done:"), "{text}");

        // A rejected permission makes the tool call fail.
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "again"}]}),
            )
            .await;
        let (done, seen) = c
            .until(id, |_| {
                Some(json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}}))
            })
            .await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert_eq!(updates(&seen, "tool_call_update")[0]["status"], "failed");

        let id = c.call("session/list", json!({"cwd": cwd})).await;
        let (list, _) = c.until(id, |_| None).await;
        assert!(list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["sessionId"] == sid.as_str()));

        // A second connection loads the session and gets its history replayed.
        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        let (loaded, seen) = c2.until(id, |_| None).await;
        assert!(loaded.get("error").is_none(), "{loaded}");
        let users: Vec<&str> = updates(&seen, "user_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(users, ["run it", "again"]);
        assert_eq!(updates(&seen, "tool_call").len(), 2);

        let id = c.call("nope/method", json!({})).await;
        let (err, _) = c.until(id, |_| None).await;
        assert_eq!(err["error"]["code"], -32601);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_turn_waiting_for_permission() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut c = start(&cwd, &fake_model().await);
        let id = c
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let (new, _) = c.until(id, |_| None).await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let pid = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        loop {
            let m = c.recv().await;
            if m["method"] == "session/request_permission" {
                break;
            }
        }
        c.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}))
            .await;
        let (done, _) = c.until(pid, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled");
        // The session works again afterwards.
        let id = c
            .call(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "plan"}),
            )
            .await;
        let (r, seen) = c.until(id, |_| None).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(
            updates(&seen, "current_mode_update")[0]["currentModeId"],
            "plan"
        );
    }

    #[test]
    fn content_and_servers() {
        let t = prompt_text(&json!([
            {"type": "text", "text": "look"},
            {"type": "resource", "resource": {"uri": "file:///p/a.rs", "text": "fn a() {}"}},
            {"type": "resource_link", "name": "b.rs", "uri": "file:///p/b.rs"},
            {"type": "image", "data": "..."}
        ]));
        assert_eq!(t, "look\n\n<file uri=\"file:///p/a.rs\">\nfn a() {}\n</file>\n\nReferenced: b.rs (/p/b.rs)\n\n[image content omitted: not supported]");
        let s = mcp_servers(&json!([
            {"name": "fs", "command": "npx", "args": ["-y", "srv"], "env": [{"name": "K", "value": "v"}]},
            {"type": "http", "name": "web", "url": "https://m/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]},
            {"type": "sse", "name": "old", "url": "https://m/sse", "headers": []}
        ]));
        assert!(
            matches!(&s["fs"], ServerConfig::Local { command, environment, .. } if command == &["npx", "-y", "srv"] && environment["K"] == "v")
        );
        assert!(
            matches!(&s["web"], ServerConfig::Remote { headers, .. } if headers["Authorization"] == "Bearer t")
        );
        assert!(!s.contains_key("old"), "SSE servers are not supported");
        assert_eq!(tool_kind("edit"), "edit");
        assert_eq!(tool_kind("github_search"), "other");
        let p = plan(&[
            Todo {
                content: "a".into(),
                status: Status::InProgress,
            },
            Todo {
                content: "b".into(),
                status: Status::Cancelled,
            },
        ]);
        assert_eq!(
            p["entries"],
            json!([{"content": "a", "priority": "medium", "status": "in_progress"}])
        );
    }
}
