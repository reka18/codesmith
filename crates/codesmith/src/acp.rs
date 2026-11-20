//! `codesmith acp`: the Agent Client Protocol (v1) over stdio, so editors
//! such as Zed can drive CodeSmith sessions. Newline-delimited JSON-RPC 2.0
//! on stdin/stdout; logs go to stderr.
//!
//! The wire shapes follow OpenCode's ACP agent (`packages/cli/src/acp`):
//! its error codes and messages, tool call lifecycle, permission options,
//! edit diffs, replay, config options and elicitation.

use anyhow::{anyhow, Result};
use codesmith_ai::{Message, Part, Role, Usage};
use codesmith_core::config::Loaded;
use codesmith_core::mcp::{Mcp, ServerConfig};
use codesmith_core::permission::{Asker, Effect, Reply, Request, Rule};
use codesmith_core::question::{Prompt as Question, Questioner};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::{self, Record, Store};
use codesmith_core::tool::todo::{Status, Todo};
use codesmith_core::{agent, command, config};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const PROTOCOL_VERSION: u64 = 1;

const AUTH_METHOD: &str = "codesmith-login";
const RETRY_META: &str = "codesmith/retry";
const COMPACTION_META: &str = "codesmith/compaction";

/// A JSON-RPC error: code, message and data.
type RpcError = (i64, String, Value);

fn invalid(message: impl std::fmt::Display, data: Value) -> RpcError {
    (-32602, format!("Invalid params: {message}"), data)
}

fn internal(message: impl std::fmt::Display, data: Value) -> RpcError {
    (-32603, format!("Internal error: {message}"), data)
}

fn session_not_found(id: &str) -> RpcError {
    invalid(format!("session not found: {id}"), json!({"sessionId": id}))
}

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
    fn error(&self, id: &Value, (code, message, data): &RpcError) {
        let mut error = json!({"code": code, "message": message});
        if !data.is_null() {
            error["data"] = data.clone();
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": error}));
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
    /// Withdraw a session's open requests: the client is told with
    /// `$/cancel_request` and the waiters see a cancellation.
    fn drop_pending(&self, session: &str) {
        let dropped: Vec<u64> = {
            let mut pending = self.pending.lock().unwrap();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, (s, _))| s == session)
                .map(|(id, _)| *id)
                .collect();
            for id in &ids {
                pending.remove(id);
            }
            ids
        };
        for id in dropped {
            self.send(json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}}));
        }
    }
}

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let mix = (u64::from(t.subsec_nanos()) ^ (n << 8) ^ u64::from(std::process::id())) & 0xffffff;
    format!("{prefix}_{:x}{mix:06x}{:x}", t.as_millis(), n & 0xf)
}

fn reasoning_id(message: &str, ordinal: usize) -> String {
    format!("{message}:reasoning:{ordinal}")
}

/// A tool's own name: subagent tools arrive as "agent › tool".
fn base_name(name: &str) -> &str {
    name.rsplit(" › ").next().unwrap_or(name)
}

pub fn tool_kind(name: &str) -> &'static str {
    match base_name(name) {
        "read" | "skill" => "read",
        "write" | "edit" => "edit",
        "glob" | "grep" => "search",
        "shell" => "execute",
        "webfetch" | "websearch" => "fetch",
        "todowrite" | "subagent" => "think",
        _ => "other",
    }
}

fn absolute(cwd: &Path, p: &str) -> PathBuf {
    if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        codesmith_core::tool::normalize(&cwd.join(p))
    }
}

/// The file an edit, write or read names (`filePath` in older sessions).
fn file_path(input: &Value) -> Option<&str> {
    input["path"].as_str().or(input["filePath"].as_str())
}

/// OpenCode's `toLocations`.
fn locations(cwd: &Path, name: &str, input: &Value) -> Value {
    let path = match base_name(name) {
        "shell" => Some(
            input["workdir"]
                .as_str()
                .or(input["cwd"].as_str())
                .map(|p| absolute(cwd, p))
                .unwrap_or_else(|| cwd.to_path_buf()),
        ),
        "read" | "edit" | "write" => file_path(input).map(|p| absolute(cwd, p)),
        "grep" | "glob" => input["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| absolute(cwd, p)),
        _ => None,
    };
    match path {
        Some(p) => json!([{"path": p}]),
        None => json!([]),
    }
}

fn tool_title(name: &str, input: &Value) -> String {
    if base_name(name) == "shell" {
        if let Some(c) = input["command"].as_str() {
            return c.to_string();
        }
    }
    format!("{name} {}", crate::render::describe(name, input))
        .trim()
        .to_string()
}

/// Shell calls carry the directory they run in.
fn raw_input(cwd: &Path, name: &str, input: &Value) -> Value {
    let mut v = input.clone();
    if base_name(name) == "shell" && input["workdir"].is_null() && input["cwd"].is_null() {
        if let Some(o) = v.as_object_mut() {
            o.insert("cwd".into(), json!(cwd));
        }
    }
    v
}

fn tool_call(cwd: &Path, id: &str, name: &str, input: &Value, status: &str) -> Value {
    json!({
        "toolCallId": id,
        "name": name,
        "title": tool_title(name, input),
        "kind": tool_kind(name),
        "status": status,
        "locations": locations(cwd, name, input),
        "rawInput": raw_input(cwd, name, input),
    })
}

fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": clip(text, 20_000)}})
}

fn diff(path: &Path, old: Option<String>, new: String) -> Value {
    json!({"type": "diff", "path": path, "oldText": old, "newText": new})
}

/// The `tool_call_update` that ends a call.
fn result_update(
    cwd: &Path,
    id: &str,
    name: &str,
    input: &Value,
    output: &str,
    failed: bool,
) -> Value {
    if failed {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "failed",
            "kind": tool_kind(name),
            "title": tool_title(name, input),
            "locations": locations(cwd, name, input),
            "rawInput": raw_input(cwd, name, input),
            "content": [text_content(output)],
            "rawOutput": {"error": clip(output, 20_000)},
        })
    } else {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "completed",
            "locations": locations(cwd, name, input),
            "content": [text_content(output)],
        })
    }
}

/// What an edit or write would change, shown with its permission request.
fn preview(cwd: &Path, name: &str, input: &Value) -> Option<Value> {
    let path = absolute(cwd, file_path(input)?);
    let old = std::fs::read_to_string(&path).ok();
    let new = match base_name(name) {
        "write" => input["content"].as_str()?.to_string(),
        "edit" => {
            codesmith_core::tool::apply_edit(
                old.as_deref()?,
                input["oldString"].as_str()?,
                input["newString"].as_str()?,
                input["replaceAll"].as_bool().unwrap_or(false),
            )
            .ok()?
            .0
        }
        _ => return None,
    };
    Some(diff(&path, old, new))
}

/// OpenCode's `permissionTitle`; `None` falls back to CodeSmith's own.
fn permission_title(name: &str, input: &Value) -> Option<String> {
    let s = |k: &str| input[k].as_str().map(str::to_string);
    match base_name(name) {
        "shell" => s("command"),
        "webfetch" => s("url"),
        "websearch" => s("query"),
        "grep" | "glob" => s("pattern"),
        "read" | "edit" | "write" => file_path(input).map(str::to_string),
        _ => None,
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

fn chunk(kind: &str, message: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "messageId": message, "content": {"type": "text", "text": text}})
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

fn total(u: &Usage) -> u64 {
    u.input + u.output + u.cache_read + u.cache_write
}

/// A turn's usage as a `PromptResponse.usage`.
fn usage_json(u: &Usage) -> Value {
    let mut v = json!({"inputTokens": u.input, "outputTokens": u.output, "totalTokens": total(u)});
    if u.cache_read > 0 {
        v["cachedReadTokens"] = json!(u.cache_read);
    }
    if u.cache_write > 0 {
        v["cachedWriteTokens"] = json!(u.cache_write);
    }
    v
}

/// OpenCode's `resolveStopReason`, over provider finish reasons.
fn stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length" | "max_tokens" | "max_output_tokens" | "MAX_TOKENS") => "max_tokens",
        Some("content_filter" | "content-filter" | "refusal" | "safety" | "SAFETY") => "refusal",
        _ => "end_turn",
    }
}

/// What the running turn has used, shared so a cancel can report it.
#[derive(Default)]
struct TurnStats {
    usage: Option<Usage>,
    last: Option<Usage>,
    finish: Option<String>,
}

/// The tool call that is running, so permission requests can point at it.
type Current = Arc<Mutex<Option<(String, String, Value)>>>;

/// A file an edit is about to change, and its text (`None`: new file).
type Before = (PathBuf, Option<String>);

struct AcpSink {
    conn: Conn,
    session: String,
    cwd: PathBuf,
    current: Current,
    stats: Arc<Mutex<TurnStats>>,
    /// The client renders `compaction_update`.
    compaction: bool,
    /// The assistant message being streamed.
    message: String,
    /// Reasoning parts started in this message, and whether one is open.
    reasoning: usize,
    in_reasoning: bool,
    /// Calls in flight: name, input and, for edits, the file before.
    tools: HashMap<String, (String, Value, Option<Before>)>,
    retrying: bool,
}

impl AcpSink {
    fn new(conn: Conn, session: &str, slot: &Slot, compaction: bool) -> Self {
        AcpSink {
            conn,
            session: session.to_string(),
            cwd: slot.cwd.clone(),
            current: slot.current.clone(),
            stats: slot.stats.clone(),
            compaction,
            message: unique("msg"),
            reasoning: 0,
            in_reasoning: false,
            tools: HashMap::new(),
            retrying: false,
        }
    }

    /// The step a retry waited for has started: clear the retry marker.
    fn settle_retry(&mut self) {
        if std::mem::take(&mut self.retrying) {
            self.conn.update(
                &self.session,
                json!({"sessionUpdate": "session_info_update", "_meta": {RETRY_META: null}}),
            );
        }
    }
}

impl Sink for AcpSink {
    fn text(&mut self, delta: &str) {
        self.settle_retry();
        self.in_reasoning = false;
        self.conn.update(
            &self.session,
            chunk("agent_message_chunk", &self.message, delta),
        );
    }
    fn reasoning(&mut self, delta: &str) {
        self.settle_retry();
        if !self.in_reasoning {
            self.in_reasoning = true;
            self.reasoning += 1;
        }
        let id = reasoning_id(&self.message, self.reasoning - 1);
        self.conn
            .update(&self.session, chunk("agent_thought_chunk", &id, delta));
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        self.settle_retry();
        self.in_reasoning = false;
        let before = matches!(base_name(name), "edit" | "write")
            .then(|| file_path(input).map(|p| absolute(&self.cwd, p)))
            .flatten()
            .map(|p| {
                let old = std::fs::read_to_string(&p).ok();
                (p, old)
            });
        self.tools
            .insert(id.into(), (name.into(), input.clone(), before));
        *self.current.lock().unwrap() = Some((id.to_string(), name.to_string(), input.clone()));
        let mut call = tool_call(&self.cwd, id, name, input, "pending");
        call["sessionUpdate"] = json!("tool_call");
        self.conn.update(&self.session, call);
        let mut running = tool_call(&self.cwd, id, name, input, "in_progress");
        running["sessionUpdate"] = json!("tool_call_update");
        running.as_object_mut().unwrap().remove("name");
        self.conn.update(&self.session, running);
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        *self.current.lock().unwrap() = None;
        let (name, input, before) = self
            .tools
            .remove(id)
            .unwrap_or_else(|| (name.to_string(), json!({}), None));
        let mut update = result_update(&self.cwd, id, &name, &input, output, is_error);
        if let (false, Some((path, old))) = (is_error, before) {
            if let Ok(new) = std::fs::read_to_string(&path) {
                if old.as_deref() != Some(new.as_str()) {
                    update["content"]
                        .as_array_mut()
                        .unwrap()
                        .push(diff(&path, old, new));
                }
            }
        }
        self.conn.update(&self.session, update);
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.conn.update(&self.session, plan(todos));
    }
    fn step_end(&mut self, reason: &str, usage: &Usage) {
        self.settle_retry();
        {
            let mut st = self.stats.lock().unwrap();
            let sum = st.usage.get_or_insert_with(Usage::default);
            sum.input += usage.input;
            sum.output += usage.output;
            sum.cache_read += usage.cache_read;
            sum.cache_write += usage.cache_write;
            st.last = Some(*usage);
            st.finish = Some(reason.to_string());
        }
        self.message = unique("msg");
        self.reasoning = 0;
        self.in_reasoning = false;
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        let at = chrono::Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default();
        self.retrying = true;
        self.conn.update(
            &self.session,
            json!({"sessionUpdate": "session_info_update", "_meta": {RETRY_META: {
                "attempt": attempt,
                "nextRetryAt": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "error": {"message": error},
            }}}),
        );
    }
    fn compacted(&mut self, before: u64, after: u64) {
        let update = if self.compaction {
            json!({"sessionUpdate": "compaction_update", "compactionId": unique("cmp"), "status": "completed"})
        } else {
            json!({"sessionUpdate": "session_info_update", "_meta": {COMPACTION_META: {"tokensBefore": before, "tokensAfter": after}}})
        };
        self.conn.update(&self.session, update);
    }
}

/// Permission requests go to the client as `session/request_permission`.
struct AcpAsker {
    conn: Conn,
    session: String,
    cwd: PathBuf,
    current: Current,
}

impl Asker for AcpAsker {
    fn ask(&self, req: &Request) -> Reply {
        let current = self.current.lock().unwrap().clone();
        let mut tc = match &current {
            Some((id, name, input)) => {
                let mut tc = tool_call(&self.cwd, id, name, input, "pending");
                if let Some(t) = permission_title(name, input) {
                    tc["title"] = json!(t);
                } else {
                    tc["title"] = json!(req.title);
                }
                if let Some(d) = preview(&self.cwd, name, input) {
                    tc["content"] = json!([d]);
                }
                tc
            }
            None => {
                let paths: BTreeSet<String> = req
                    .resources
                    .iter()
                    .filter(|_| {
                        matches!(req.action.as_str(), "read" | "edit" | "external_directory")
                    })
                    .map(|r| r.strip_suffix("/*").unwrap_or(r))
                    .filter(|r| !r.is_empty() && !r.contains(['*', '?']))
                    .map(|r| absolute(&self.cwd, r).display().to_string())
                    .collect();
                let locations: Vec<Value> = paths.iter().map(|p| json!({"path": p})).collect();
                json!({
                    "toolCallId": format!("perm_{}", req.action),
                    "title": req.title,
                    "kind": tool_kind(&req.action),
                    "status": "pending",
                    "locations": locations,
                })
            }
        };
        tc.as_object_mut().unwrap().retain(|_, v| !v.is_null());
        let params = json!({
            "sessionId": self.session,
            "toolCall": tc,
            "options": [
                {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
                {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
                {"optionId": "reject", "kind": "reject_once", "name": "Reject"},
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
                (Some("selected"), Some("once")) => Reply::Once,
                (Some("selected"), Some("always")) => Reply::Always,
                _ => Reply::Reject(None),
            },
            _ => Reply::Reject(None),
        }
    }
}

/// Text that suggests a secret: form elicitation must not collect those
/// (OpenCode's `Credential` pattern).
fn credential_like(text: &str) -> bool {
    let t = text.to_lowercase();
    ["password", "passphrase", "secret", "token", "credential"]
        .iter()
        .any(|w| t.contains(w))
        || ["api", "private"].iter().any(|p| {
            ["_key", "-key", " key", "key"]
                .iter()
                .any(|k| t.contains(&format!("{p}{k}")))
        })
}

fn question_title(q: &Question) -> &str {
    if q.header.trim().is_empty() {
        &q.question
    } else {
        &q.header
    }
}

/// The `requestedSchema` for the `question` tool's questions: `q<i>` is the
/// choice and `q<i>_custom` a typed answer.
pub fn question_schema(questions: &[Question]) -> Value {
    let mut properties = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        let title = question_title(q);
        let options: Vec<Value> = q
            .options
            .iter()
            .map(|o| json!({"const": o.label, "title": o.label, "description": o.description}))
            .collect();
        let field = if options.is_empty() {
            json!({"type": "string", "title": title, "description": q.question})
        } else if q.multiple {
            json!({"type": "array", "title": title, "description": q.question, "items": {"anyOf": options}})
        } else {
            json!({"type": "string", "title": title, "description": q.question, "oneOf": options})
        };
        properties.insert(format!("q{i}"), field);
        if !q.options.is_empty() {
            let hint = if q.multiple {
                "Add your own answer"
            } else {
                "Type your own answer"
            };
            properties.insert(
                format!("q{i}_custom"),
                json!({"type": "string", "title": format!("{title} (other)"), "description": hint}),
            );
        }
    }
    json!({"type": "object", "properties": properties, "required": []})
}

/// The answers in an `elicitation/create` response; `None` when the user
/// declined or cancelled, or the content does not fit the schema.
pub fn question_answers(questions: &[Question], response: &Value) -> Option<Vec<Vec<String>>> {
    if response["action"] != "accept" {
        return None;
    }
    let content = response["content"].as_object().cloned().unwrap_or_default();
    let mut answers = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let mut picked: Vec<String> = match content.get(&format!("q{i}")) {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) if s.trim().is_empty() => vec![],
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
            Some(_) => return None,
        };
        match content.get(&format!("q{i}_custom")) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.trim().is_empty() => {}
            Some(Value::String(s)) if q.multiple => picked.push(s.trim().to_string()),
            Some(Value::String(s)) => picked = vec![s.trim().to_string()],
            Some(_) => return None,
        }
        answers.push(picked);
    }
    Some(answers)
}

/// The `question` tool, answered through `elicitation/create` when the
/// client supports form elicitation.
struct AcpQuestioner {
    conn: Conn,
    session: String,
    current: Current,
}

impl Questioner for AcpQuestioner {
    fn ask(&self, questions: &[Question]) -> Option<Vec<Vec<String>>> {
        let secret = questions.iter().any(|q| {
            credential_like(&q.question)
                || credential_like(&q.header)
                || q.options.iter().any(|o| credential_like(&o.label))
        });
        if secret {
            return None;
        }
        let message = match questions {
            [q] => q.question.clone(),
            _ => "Questions".to_string(),
        };
        let mut params = json!({
            "mode": "form",
            "sessionId": self.session,
            "message": message,
            "requestedSchema": question_schema(questions),
        });
        if let Some((id, ..)) = self.current.lock().unwrap().as_ref() {
            params["toolCallId"] = json!(id);
        }
        let rx = self
            .conn
            .request(&self.session, "elicitation/create", params);
        let response = rx.blocking_recv().ok()?.ok()?;
        question_answers(questions, &response)
    }
}

struct Slot {
    id: String,
    session: tokio::sync::Mutex<Option<Session>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The JSON-RPC id of the running `session/prompt`.
    prompt: Mutex<Option<Value>>,
    current: Current,
    stats: Arc<Mutex<TurnStats>>,
    loaded: Loaded,
    cwd: PathBuf,
    mcp: Arc<Mcp>,
    /// The agent and model chosen through ACP (`None`: the session's own).
    mode: Mutex<Option<String>>,
    model: Mutex<Option<String>>,
    /// The selection changed during a turn: rebuild the session when it ends.
    stale: AtomicBool,
    /// The model and agent of the latest session built.
    shown: Mutex<(String, String)>,
}

pub type Loader = Arc<dyn Fn(&Path) -> Result<Loaded> + Send + Sync>;

pub struct Agent {
    conn: Conn,
    db: PathBuf,
    load: Loader,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Slot>>>,
    /// What the client advertised in `initialize`.
    client: Mutex<Value>,
}

/// Editor-supplied MCP servers, in CodeSmith's config shape. Like OpenCode,
/// only stdio and HTTP servers are accepted.
pub fn mcp_servers(list: &Value) -> Result<BTreeMap<String, ServerConfig>, RpcError> {
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
        let cfg = match s["type"].as_str() {
            Some("http") => {
                json!({"type": "remote", "url": s["url"], "headers": pairs(&s["headers"])})
            }
            Some(_) => {
                return Err(invalid(
                    "Only stdio and HTTP MCP servers are supported",
                    json!({"field": "mcpServers"}),
                ))
            }
            None => {
                let mut cmd = vec![s["command"].clone()];
                cmd.extend(s["args"].as_array().cloned().unwrap_or_default());
                json!({"type": "local", "command": cmd, "environment": pairs(&s["env"])})
            }
        };
        let Some(name) = s["name"].as_str() else {
            continue;
        };
        if let Ok(c) = serde_json::from_value::<ServerConfig>(cfg) {
            out.insert(name.to_string(), c);
        }
    }
    Ok(out)
}

fn parse_cwd(v: &Value) -> Result<PathBuf, RpcError> {
    let raw = v.as_str().unwrap_or("");
    Some(PathBuf::from(raw))
        .filter(|c| c.is_absolute())
        .ok_or_else(|| {
            invalid(
                format!("cwd must be an absolute path: {raw}"),
                json!({"field": "cwd"}),
            )
        })
}

/// `additionalDirectories`: absolute, without glob characters, deduplicated
/// and without the cwd itself (OpenCode's `ACPDirectories.parse`).
pub fn additional_directories(cwd: &Path, v: &Value) -> Result<Vec<PathBuf>, RpcError> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let root = canon(cwd);
    let mut out: Vec<PathBuf> = Vec::new();
    for d in v.as_array().into_iter().flatten() {
        let raw = d.as_str().unwrap_or("");
        if !Path::new(raw).is_absolute() || raw.contains(['*', '?']) {
            return Err(invalid(
                format!(
                    "additional directory must be an absolute path without glob characters: {raw}"
                ),
                json!({"additionalDirectory": raw}),
            ));
        }
        let p = codesmith_core::tool::normalize(Path::new(raw));
        if canon(&p) != root && !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Rules letting tools reach `dirs` without an `external_directory` ask.
/// Both spellings of a symlinked root are granted, since tools compare the
/// path as written.
pub fn grants(dirs: &[PathBuf]) -> Vec<Rule> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for d in dirs {
        for p in [
            d.clone(),
            std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()),
        ] {
            if !roots.contains(&p) {
                roots.push(p);
            }
        }
    }
    roots
        .iter()
        .flat_map(|d| {
            [
                Rule::new(
                    "external_directory",
                    &d.display().to_string(),
                    Effect::Allow,
                ),
                Rule::new(
                    "external_directory",
                    &format!("{}/*", d.display()),
                    Effect::Allow,
                ),
            ]
        })
        .collect()
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

fn primary_agents(config: &config::Config) -> Vec<agent::Agent> {
    agent::list(config)
        .into_iter()
        .filter_map(|id| agent::resolve(config, &id))
        .filter(|a| a.mode != "subagent")
        .collect()
}

fn modes(s: &Session, config: &config::Config) -> Value {
    let available: Vec<Value> = primary_agents(config)
        .into_iter()
        .map(|a| json!({"id": a.id, "name": a.name, "description": a.description}))
        .collect();
    json!({"currentModeId": s.agent.id, "availableModes": available})
}

/// Models worth offering: the configured default, agents' models, every
/// model listed under a provider, and the session's own.
fn models(config: &config::Config, current: &str) -> Vec<(String, String)> {
    let mut specs: BTreeMap<String, String> = BTreeMap::new();
    for (pid, p) in &config.providers {
        for (mid, m) in &p.models {
            let name = m.name.clone().unwrap_or_else(|| mid.clone());
            specs.insert(format!("{pid}/{mid}"), format!("{pid}/{name}"));
        }
    }
    let extra = config
        .model
        .iter()
        .chain(config.agents.values().filter_map(|a| a.model.as_ref()))
        .chain(std::iter::once(&current.to_string()))
        .cloned()
        .collect::<Vec<_>>();
    for spec in extra {
        specs.entry(spec.clone()).or_insert(spec);
    }
    specs.into_iter().collect()
}

/// `configOptions`: model and mode selectors (OpenCode's `configOptions`;
/// CodeSmith models have no effort variants).
fn config_options(config: &config::Config, model: &str, mode: &str) -> Value {
    let models: Vec<Value> = models(config, model)
        .into_iter()
        .map(|(value, name)| json!({"value": value, "name": name}))
        .collect();
    let modes: Vec<Value> = primary_agents(config)
        .into_iter()
        .map(|a| {
            let mut m = json!({"value": a.id, "name": a.name});
            if !a.description.is_empty() {
                m["description"] = json!(a.description);
            }
            m
        })
        .collect();
    json!([
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model, "options": models},
        {"id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": mode, "options": modes},
    ])
}

fn is_prompt(t: &str) -> bool {
    let t = t.trim_start();
    !["<shell ", "<system-reminder>", "<conversation-checkpoint>"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// A replayed history item: a message or the checkpoint of a compaction.
enum Item {
    Message(String, Message),
    Checkpoint(String, String),
}

impl Agent {
    fn store(&self) -> Option<Store> {
        Store::open(&self.db).ok()
    }

    fn client_has(&self, pointer: &str) -> bool {
        self.client
            .lock()
            .unwrap()
            .pointer(pointer)
            .is_some_and(Value::is_object)
    }

    /// (Re)build a slot's session from the store and its selection.
    fn build(&self, slot: &Slot) -> Result<Session> {
        let stored = self
            .store()
            .and_then(|db| db.get(&slot.id).ok().flatten())
            .is_some();
        let asker = Arc::new(AcpAsker {
            conn: self.conn.clone(),
            session: slot.id.clone(),
            cwd: slot.cwd.clone(),
            current: slot.current.clone(),
        });
        let questioner = self.client_has("/elicitation/form").then(|| {
            Arc::new(AcpQuestioner {
                conn: self.conn.clone(),
                session: slot.id.clone(),
                current: slot.current.clone(),
            }) as Arc<dyn Questioner>
        });
        let mut s = Session::new(
            &slot.loaded,
            slot.cwd.clone(),
            Options {
                agent: slot.mode.lock().unwrap().clone(),
                model: slot.model.lock().unwrap().clone(),
                resume: stored.then(|| slot.id.clone()),
                title: None,
                parent: None,
                questioner,
                mcp: Some(slot.mcp.clone()),
            },
            asker,
            self.store(),
        )?;
        s.id = slot.id.clone();
        *slot.shown.lock().unwrap() = (s.model_spec.clone(), s.agent.id.clone());
        if stored {
            if let Some(db) = self.store() {
                let _ = db.set_selection(&s.id, &s.agent.id, &s.model_spec);
            }
        }
        Ok(s)
    }

    async fn open(
        &self,
        id: &str,
        cwd: &Path,
        servers: BTreeMap<String, ServerConfig>,
        dirs: &[PathBuf],
    ) -> Result<Arc<Slot>> {
        let mut loaded = (self.load)(cwd)?;
        loaded.config.mcp.servers.extend(servers);
        // The grants override a configured ask, but configured denials of
        // external paths still win.
        let denials: Vec<Rule> = loaded
            .config
            .permissions
            .iter()
            .filter(|r| {
                r.effect == Effect::Deny
                    && codesmith_core::wildcard::matches("external_directory", &r.action, false)
            })
            .cloned()
            .collect();
        loaded.config.permissions.extend(grants(dirs));
        loaded.config.permissions.extend(denials);
        let mcp = Mcp::connect(&loaded.config.mcp, &loaded.root).await;
        let slot = Arc::new(Slot {
            id: id.to_string(),
            session: tokio::sync::Mutex::new(None),
            task: Mutex::new(None),
            prompt: Mutex::new(None),
            current: Default::default(),
            stats: Default::default(),
            loaded,
            cwd: cwd.to_path_buf(),
            mcp,
            mode: Mutex::new(None),
            model: Mutex::new(None),
            stale: AtomicBool::new(false),
            shown: Default::default(),
        });
        let s = self.build(&slot)?;
        *slot.session.lock().await = Some(s);
        Ok(slot)
    }

    async fn slot(&self, id: &str) -> Result<Arc<Slot>, RpcError> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| session_not_found(id))
    }

    fn commands(&self, slot: &Slot) -> Value {
        let all = crate::all_commands_with(&slot.cwd, &slot.loaded, &slot.mcp);
        let mut cmds: Vec<Value> = all
            .iter()
            .map(|(name, c)| json!({"name": name, "description": c.description.clone().unwrap_or_default(), "input": {"hint": "arguments"}}))
            .collect();
        if !all.contains_key("compact") {
            cmds.push(json!({"name": "compact", "description": "Compact the session"}));
        }
        json!({"sessionUpdate": "available_commands_update", "availableCommands": cmds})
    }

    /// Send a loaded session's history: every visible message of its log,
    /// including what a compaction summarised, with the summary marked.
    fn replay(&self, id: &str, s: &Session) {
        let entries = self
            .store()
            .and_then(|db| db.entries(id).ok())
            .unwrap_or_default();
        let hidden = store::hidden_ranges(&entries);
        let items: Vec<Item> = if entries.is_empty() {
            s.messages
                .iter()
                .enumerate()
                .map(|(i, m)| Item::Message(format!("{id}:{i}"), m.clone()))
                .collect()
        } else {
            entries
                .iter()
                .filter(|e| !hidden.iter().any(|(f, t)| e.seq >= *f && e.seq < *t))
                .filter_map(|e| match &e.record {
                    Record::Message { message } => {
                        Some(Item::Message(format!("{id}:{}", e.seq), message.clone()))
                    }
                    Record::Checkpoint { summary, .. } => {
                        Some(Item::Checkpoint(format!("{id}:{}", e.seq), summary.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        // Each call's result is the first one with its id after it (ids can
        // repeat across turns).
        let results: Vec<(usize, &str, &str, bool)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                Item::Message(_, m) => Some((i, m)),
                _ => None,
            })
            .flat_map(|(i, m)| m.parts.iter().map(move |p| (i, p)))
            .filter_map(|(i, p)| match p {
                Part::ToolResult {
                    id,
                    content,
                    is_error,
                    ..
                } => Some((i, id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        let compaction = self.client_has("/session/compaction");
        for (index, item) in items.iter().enumerate() {
            let (mid, m) = match item {
                Item::Checkpoint(cid, summary) => {
                    let update = if compaction {
                        json!({"sessionUpdate": "compaction_update", "compactionId": cid, "status": "completed", "summary": [{"type": "text", "text": summary}]})
                    } else {
                        json!({"sessionUpdate": "session_info_update", "_meta": {COMPACTION_META: {"messageId": cid}}})
                    };
                    self.conn.update(id, update);
                    continue;
                }
                Item::Message(mid, m) => (mid, m),
            };
            let mut reasoning = 0;
            for p in &m.parts {
                match (&m.role, p) {
                    (Role::User, Part::Text(t)) if is_prompt(t) => {
                        self.conn.update(id, chunk("user_message_chunk", mid, t))
                    }
                    (Role::Assistant, Part::Text(t)) => {
                        self.conn.update(id, chunk("agent_message_chunk", mid, t))
                    }
                    (Role::Assistant, Part::Reasoning(t)) => {
                        let rid = reasoning_id(mid, reasoning);
                        reasoning += 1;
                        self.conn.update(id, chunk("agent_thought_chunk", &rid, t))
                    }
                    (
                        Role::Assistant,
                        Part::ToolCall {
                            id: cid,
                            name,
                            input,
                        },
                    ) => {
                        let mut call = tool_call(&s.cwd, cid, name, input, "pending");
                        call["sessionUpdate"] = json!("tool_call");
                        self.conn.update(id, call);
                        let result = results.iter().find(|(i, rid, ..)| *i > index && rid == cid);
                        if let Some((_, _, output, failed)) = result {
                            self.conn.update(
                                id,
                                result_update(&s.cwd, cid, name, input, output, *failed),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    async fn handle(self: &Arc<Self>, id: Value, method: &str, p: Value) {
        let r: Result<Value, RpcError> = match method {
            "initialize" => {
                *self.client.lock().unwrap() = p["clientCapabilities"].clone();
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                        "mcpCapabilities": {"http": true, "sse": false},
                        "sessionCapabilities": {"additionalDirectories": {}, "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {}},
                    },
                    "authMethods": [{"id": AUTH_METHOD, "name": "Login with codesmith", "description": "Run `codesmith auth login <provider>` in the terminal"}],
                    "agentInfo": {"name": "codesmith", "title": "CodeSmith", "version": env!("CARGO_PKG_VERSION")},
                }))
            }
            "authenticate" => match p["methodId"].as_str() {
                Some(AUTH_METHOD) => Ok(json!({})),
                other => {
                    let m = other.unwrap_or("");
                    Err(invalid(
                        format!("unknown auth method: {m}"),
                        json!({"methodId": m}),
                    ))
                }
            },
            "logout" => Ok(json!({})),
            "session/new" | "session/load" | "session/resume" | "session/fork" => {
                self.open_session(method, &p).await
            }
            "session/list" => self.list(&p),
            "session/delete" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Some(slot) = self.sessions.lock().await.remove(&sid) {
                    self.stop(&sid, &slot, false).await;
                }
                self.store()
                    .map(|db| db.delete(&sid))
                    .transpose()
                    .map_err(|e| internal(format!("{e:#}"), Value::Null))
                    .map(|_| json!({}))
            }
            "session/close" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Some(slot) = self.sessions.lock().await.remove(&sid) {
                    self.stop(&sid, &slot, false).await;
                }
                Ok(json!({}))
            }
            "session/set_mode" => {
                let sid = p["sessionId"].as_str().unwrap_or("");
                match p["modeId"].as_str() {
                    Some(mode) => self.select(sid, "mode", mode).await.map(|_| json!({})),
                    None => Err(invalid("modeId is required", json!({"field": "modeId"}))),
                }
            }
            "session/set_config_option" => {
                let sid = p["sessionId"].as_str().unwrap_or("");
                let config_id = p["configId"].as_str().unwrap_or("");
                match p["value"].as_str() {
                    Some(value) => self
                        .select(sid, config_id, value)
                        .await
                        .map(|options| json!({"configOptions": options})),
                    None => Err(invalid(
                        format!("unknown config option: {config_id}"),
                        json!({"configId": config_id}),
                    )),
                }
            }
            "session/prompt" => {
                // Answered when the turn ends.
                if let Err(e) = self.prompt(id.clone(), &p).await {
                    self.conn.error(&id, &e);
                }
                return;
            }
            _ => Err((
                -32601,
                format!("Method not found: {method}"),
                json!({"method": method}),
            )),
        };
        match r {
            Ok(v) => self.conn.respond(&id, v),
            Err(e) => self.conn.error(&id, &e),
        }
    }

    fn list(&self, p: &Value) -> Result<Value, RpcError> {
        let cwd = match &p["cwd"] {
            Value::Null => None,
            v => Some(parse_cwd(v)?.display().to_string()),
        };
        let list = self
            .store()
            .map(|db| db.list(cwd.as_deref(), false).unwrap_or_default())
            .unwrap_or_default();
        let sessions: Vec<Value> = list
            .iter()
            .map(|i| json!({"sessionId": i.id, "cwd": i.directory, "title": i.title, "updatedAt": chrono::DateTime::from_timestamp_millis(i.updated).map(|t| t.to_rfc3339())}))
            .collect();
        Ok(json!({"sessions": sessions}))
    }

    async fn open_session(&self, method: &str, p: &Value) -> Result<Value, RpcError> {
        let cwd = parse_cwd(&p["cwd"])?;
        let dirs = additional_directories(&cwd, &p["additionalDirectories"])?;
        let servers = mcp_servers(&p["mcpServers"])?;
        let mut sid = match method {
            "session/new" => unique("ses"),
            _ => {
                let sid = p["sessionId"]
                    .as_str()
                    .ok_or_else(|| invalid("sessionId is required", json!({"field": "sessionId"})))?
                    .to_string();
                let info = self
                    .store()
                    .and_then(|db| db.get(&sid).ok().flatten())
                    .ok_or_else(|| session_not_found(&sid))?;
                let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                if canon(Path::new(&info.directory)) != canon(&cwd) {
                    return Err(invalid(
                        format!("session {sid} does not belong to cwd: {}", cwd.display()),
                        json!({"sessionId": sid, "cwd": cwd}),
                    ));
                }
                sid
            }
        };
        if method == "session/fork" {
            let fork = unique("ses");
            let mut db = self
                .store()
                .ok_or_else(|| internal("the session store is unavailable", Value::Null))?;
            let mut e = db
                .export(&sid)
                .map_err(|e| internal(format!("{e:#}"), Value::Null))?;
            e.info.id = fork.clone();
            e.info.parent_id = None;
            e.info.updated = store::now();
            db.import(&e)
                .map_err(|e| internal(format!("{e:#}"), Value::Null))?;
            sid = fork;
        }
        let slot = self
            .open(&sid, &cwd, servers, &dirs)
            .await
            .map_err(|e| internal(format!("{e:#}"), Value::Null))?;
        let (modes, options) = {
            let g = slot.session.lock().await;
            let s = g.as_ref().unwrap();
            if method == "session/load" {
                self.replay(&s.id, s);
            }
            (
                modes(s, &slot.loaded.config),
                config_options(&slot.loaded.config, &s.model_spec, &s.agent.id),
            )
        };
        let replaced = self.sessions.lock().await.insert(sid.clone(), slot.clone());
        if let Some(old) = replaced {
            self.stop(&sid, &old, false).await;
        }
        // Sent right after the response (the writer keeps order).
        let (conn, update) = (self.conn.clone(), self.commands(&slot));
        let sid2 = sid.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            conn.update(&sid2, update);
        });
        let mut out = json!({"modes": modes, "configOptions": options});
        if matches!(method, "session/new" | "session/fork") {
            out["sessionId"] = json!(sid);
        }
        Ok(out)
    }

    /// Change the model or mode; returns the new `configOptions`.
    async fn select(&self, sid: &str, config_id: &str, value: &str) -> Result<Value, RpcError> {
        let slot = self.slot(sid).await?;
        let config = &slot.loaded.config;
        let target = match config_id {
            "model" => {
                config.resolve_model(value).map_err(|_| {
                    invalid(
                        format!("model not found: {value}"),
                        json!({"providerId": value.split('/').next(), "modelId": value}),
                    )
                })?;
                &slot.model
            }
            "mode" => {
                agent::resolve(config, value)
                    .filter(|a| a.mode != "subagent")
                    .ok_or_else(|| {
                        invalid(format!("mode not found: {value}"), json!({"mode": value}))
                    })?;
                &slot.mode
            }
            _ => {
                return Err(invalid(
                    format!("unknown config option: {config_id}"),
                    json!({"configId": config_id}),
                ))
            }
        };
        let previous = target.lock().unwrap().replace(value.to_string());
        let mut g = slot.session.lock().await;
        match g.as_mut() {
            // A turn is running: it picks the change up when it ends.
            None => slot.stale.store(true, Ordering::SeqCst),
            Some(current) => match self.build(&slot) {
                Ok(s) => *current = s,
                Err(e) => {
                    *target.lock().unwrap() = previous;
                    return Err(invalid(format!("{e:#}"), Value::Null));
                }
            },
        }
        drop(g);
        let (model, mode) = slot.shown.lock().unwrap().clone();
        let model = slot.model.lock().unwrap().clone().unwrap_or(model);
        let mode = slot.mode.lock().unwrap().clone().unwrap_or(mode);
        let options = config_options(config, &model, &mode);
        if config_id == "mode" {
            self.conn.update(
                sid,
                json!({"sessionUpdate": "current_mode_update", "currentModeId": value}),
            );
        }
        Ok(options)
    }

    async fn prompt(self: &Arc<Self>, rpc: Value, p: &Value) -> Result<(), RpcError> {
        let sid = p["sessionId"].as_str().unwrap_or("").to_string();
        let slot = self.slot(&sid).await?;
        let mut s = slot.session.lock().await.take().ok_or_else(|| {
            internal(
                format!("Session already has an active ACP prompt: {sid}"),
                json!({"service": "session"}),
            )
        })?;
        *slot.prompt.lock().unwrap() = Some(rpc.clone());
        *slot.stats.lock().unwrap() = TurnStats::default();
        let text = prompt_text(&p["prompt"]);
        let compaction = self.client_has("/session/compaction");
        let (me, slot2) = (self.clone(), slot.clone());
        let task = tokio::spawn(async move {
            let mut sink = AcpSink::new(me.conn.clone(), &sid, &slot2, compaction);
            let title_before = s.title.clone();
            let result = me.run_turn(&mut s, &slot2, &text, &mut sink).await;
            // A cancel took the request: it answers, and reloads the session.
            let Some(rpc) = slot2.prompt.lock().unwrap().take() else {
                return;
            };
            if s.title != title_before {
                me.conn.update(
                    &sid,
                    json!({"sessionUpdate": "session_info_update", "title": s.title}),
                );
            }
            let stats = std::mem::take(&mut *slot2.stats.lock().unwrap());
            me.usage_update(&sid, &s, &slot2, &stats);
            if slot2.stale.swap(false, Ordering::SeqCst) {
                drop(s);
                match me.build(&slot2) {
                    Ok(fresh) => *slot2.session.lock().await = Some(fresh),
                    Err(e) => eprintln!("acp: could not apply the new selection: {e:#}"),
                }
            } else {
                *slot2.session.lock().await = Some(s);
            }
            slot2.task.lock().unwrap().take();
            let response = |reason: &str| {
                let mut r = json!({"stopReason": reason});
                if let Some(u) = &stats.usage {
                    r["usage"] = usage_json(u);
                }
                me.conn.respond(&rpc, r)
            };
            match result {
                Ok(()) => response(stop_reason(stats.finish.as_deref())),
                Err(e) if e.to_string().contains("without a final answer") => {
                    response("max_turn_requests")
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let err = if msg.contains("HTTP 401") || msg.contains("HTTP 403") {
                        (
                            -32000,
                            "Authentication required: provider authentication required".into(),
                            json!({}),
                        )
                    } else {
                        internal(msg, json!({"service": "session"}))
                    };
                    me.conn.error(&rpc, &err)
                }
            }
        });
        *slot.task.lock().unwrap() = Some(task);
        Ok(())
    }

    /// Context use after a turn, when the model's window is known.
    fn usage_update(&self, sid: &str, s: &Session, slot: &Slot, stats: &TurnStats) {
        let (Some(size), Some(last)) = (s.model.context, stats.last) else {
            return;
        };
        if total(&last) == 0 {
            return;
        }
        let mut update =
            json!({"sessionUpdate": "usage_update", "used": total(&last), "size": size});
        if let Some(c) = slot.loaded.config.cost(&s.model_spec) {
            let u = &s.usage;
            let amount = (u.input as f64 * c.input
                + u.output as f64 * c.output
                + u.cache_read as f64 * c.cache_read
                + u.cache_write as f64 * c.cache_write)
                / 1e6;
            update["cost"] = json!({"amount": amount, "currency": "USD"});
        }
        self.conn.update(sid, update);
    }

    async fn run_turn(
        &self,
        s: &mut Session,
        slot: &Slot,
        text: &str,
        sink: &mut AcpSink,
    ) -> Result<()> {
        if let Some((name, args)) = command::parse_invocation(text) {
            let all = crate::all_commands_with(&slot.cwd, &slot.loaded, &slot.mcp);
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
                        conn.update(
                            &sid,
                            chunk("agent_thought_chunk", "progress", &format!("{m}\n")),
                        )
                    };
                    let (report, usage) =
                        codesmith_core::review::run_command(&name, &args, &env, &progress).await?;
                    sink.text(&report);
                    return s.append_exchange(text, &report, &usage);
                }
                let text = s.expand_command(c, &args).await?;
                return s.prompt(&text, sink).await;
            }
            if name == "compact" {
                if let Err(e) = s.compact(sink).await {
                    sink.text(&format!("{e:#}"));
                }
                return Ok(());
            }
        }
        s.prompt(text, sink).await
    }

    /// Stop a running turn and answer its prompt with `cancelled`. The
    /// request is taken first, so a turn finishing meanwhile cannot answer
    /// or put its session back; with `reload` the session reloads from the
    /// store.
    async fn stop(&self, sid: &str, slot: &Slot, reload: bool) {
        let task = slot.task.lock().unwrap().take();
        let Some(task) = task else { return };
        let rpc = slot.prompt.lock().unwrap().take();
        task.abort();
        // Unblocks a tool waiting on the client; the abort lands at its next await.
        self.conn.drop_pending(sid);
        let _ = task.await;
        // The turn had already ended and answered.
        let Some(rpc) = rpc else { return };
        if let Some((id, name, input)) = slot.current.lock().unwrap().take() {
            self.conn.update(
                sid,
                result_update(&slot.cwd, &id, &name, &input, "Cancelled", true),
            );
        }
        let stats = std::mem::take(&mut *slot.stats.lock().unwrap());
        let mut response = json!({"stopReason": "cancelled"});
        if let Some(u) = &stats.usage {
            response["usage"] = usage_json(u);
        }
        if reload {
            slot.stale.store(false, Ordering::SeqCst);
            match self.build(slot) {
                Ok(s) => *slot.session.lock().await = Some(s),
                Err(e) => eprintln!("acp: could not reload session {sid}: {e:#}"),
            }
        }
        self.conn.respond(&rpc, response);
    }

    async fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "session/cancel" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Ok(slot) = self.slot(&sid).await {
                    self.stop(&sid, &slot, true).await;
                }
            }
            // Cancelling the prompt request cancels its turn.
            "$/cancel_request" => {
                let slots: Vec<(String, Arc<Slot>)> = self
                    .sessions
                    .lock()
                    .await
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (sid, slot) in slots {
                    let ours = slot.prompt.lock().unwrap().as_ref() == Some(&p["requestId"]);
                    if ours {
                        self.stop(&sid, &slot, true).await;
                    }
                }
            }
            _ => {}
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
        client: Mutex::new(Value::Null),
    });
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(m) = serde_json::from_str::<Value>(&line) else {
            conn.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
            continue;
        };
        let method = m["method"].as_str().map(str::to_string);
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        match (m.get("id").cloned().filter(|v| !v.is_null()), method) {
            (Some(id), Some(method)) => {
                let a = agent.clone();
                tokio::spawn(async move { a.handle(id, &method, params).await });
            }
            (None, Some(method)) => {
                let a = agent.clone();
                tokio::spawn(async move { a.notification(&method, params).await });
            }
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
    let slots: Vec<(String, Arc<Slot>)> = agent.sessions.lock().await.drain().collect();
    for (sid, slot) in slots {
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

/// ACP tests, ported from OpenCode's `packages/cli/test/acp` where the
/// behaviour maps to CodeSmith (`oc_acp_*`), against a fake model over a
/// JSON-RPC test client.
///
/// Not ported, because CodeSmith lacks or deliberately omits the feature:
/// - image and audio prompt content (CodeSmith prompts are text), and
///   OpenCode's readable-file attachment of `resource_link`s (CodeSmith
///   references them in the text);
/// - effort variants (`effort` config option) and `parseModelSelection`'s
///   variant suffixes: CodeSmith models have no variants;
/// - child-session updates (`opencode/child-session-updates`), background
///   turns woken by shells or subagents after `end_turn`, and
///   `_session/steering`: subagents run inside the parent's turn and their
///   tool calls stream into it;
/// - terminal auth (`--login`) and the auth-required mapping from a server
///   event (provider HTTP 401/403 errors map to -32000 instead);
/// - streamed compaction (`in_progress` and `compaction_summary_chunk`):
///   CodeSmith reports a compaction once it is done;
/// - full-file diffs on replay: CodeSmith does not record a call's
///   before/after text, so replayed edits show their output only;
/// - live and replayed `messageId`s differ (replay ids come from the log),
///   and a step's reasoning replays as one part (`:reasoning:0`), since
///   CodeSmith stores it that way;
/// - permissions settled elsewhere (no other client shares a session's
///   asks), the `patch` tool (CodeSmith has none), stored
///   `additionalDirectories` in `session/list`, and session/list paging;
/// - the server-unavailable and catalog error boundaries (no separate server).
#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn sse(chunks: Vec<Value>) -> axum::response::Response {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        ([("content-type", "text/event-stream")], body).into_response()
    }

    fn reply(text: &str) -> Vec<Value> {
        vec![json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]})]
    }

    fn call(n: usize, name: &str, args: Value) -> Vec<Value> {
        vec![
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("c{n}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]}),
        ]
    }

    static FLAKY: AtomicUsize = AtomicUsize::new(0);

    /// A scripted model: the latest user message picks the script, and the
    /// tool results since it pick the step.
    async fn fake_model() -> String {
        async fn chat(Json(b): Json<Value>) -> axum::response::Response {
            let msgs = b["messages"].as_array().unwrap();
            if b.get("tools").is_none() {
                let system = msgs[0]["content"].as_str().unwrap_or("");
                return sse(if system.contains("summaries") {
                    reply("## Objective\nShip it.")
                } else {
                    reply("A title")
                });
            }
            let at = msgs.iter().rposition(|m| m["role"] == "user").unwrap();
            let prompt = msgs[at]["content"].as_str().unwrap_or("").to_string();
            let done = msgs[at..].iter().filter(|m| m["role"] == "tool").count();
            let last = msgs.last().unwrap()["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let after = format!("done: {last}");
            let chunks = if prompt.contains("think") {
                vec![
                    json!({"choices": [{"delta": {"reasoning_content": "think-1"}}]}),
                    json!({"choices": [{"delta": {"reasoning_content": " continued"}}]}),
                    json!({"choices": [{"delta": {"content": "hello"}}]}),
                    json!({"choices": [{"delta": {"reasoning_content": "think-2"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "length"}], "usage": {"prompt_tokens": 111, "completion_tokens": 40, "prompt_tokens_details": {"cached_tokens": 11}}}),
                ]
            } else if prompt.contains("edit files") {
                match done {
                    0 => call(1, "write", json!({"path": "new.txt", "content": "two\n"})),
                    1 => call(
                        2,
                        "edit",
                        json!({"path": "old.txt", "oldString": "one", "newString": "uno"}),
                    ),
                    _ => reply("edited"),
                }
            } else if prompt.contains("ask me") {
                match done {
                    0 => call(
                        1,
                        "question",
                        json!({"questions": [{"question": "Which runtime?", "header": "Runtime", "options": [{"label": "Bun", "description": "Fast"}, {"label": "Node", "description": "Stable"}]}]}),
                    ),
                    _ => reply(&after),
                }
            } else if let Some(path) = prompt.strip_prefix("read ") {
                match done {
                    0 => call(1, "read", json!({"path": path})),
                    _ => reply(&after),
                }
            } else if prompt.contains("flaky") {
                if FLAKY.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
                }
                reply("recovered")
            } else {
                match done {
                    0 => call(
                        1,
                        "shell",
                        json!({"command": "echo acp", "description": "echo"}),
                    ),
                    _ => reply(&after),
                }
            };
            sse(chunks)
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
        async fn notify(&mut self, method: &str, params: Value) {
            self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .await;
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
        /// Read until a message matches, collecting everything up to it.
        async fn wait(&mut self, pred: impl Fn(&Value) -> bool) -> (Value, Vec<Value>) {
            let mut seen = Vec::new();
            loop {
                let m = self.recv().await;
                if pred(&m) {
                    return (m, seen);
                }
                seen.push(m);
            }
        }
        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.call(method, params).await;
            self.until(id, |_| None).await.0
        }
        async fn new_session(&mut self, cwd: &Path) -> String {
            let r = self
                .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
                .await;
            r["result"]["sessionId"].as_str().unwrap().to_string()
        }
    }

    fn start(dir: &Path, base: &str) -> Client {
        start_with(dir, base, json!({}))
    }

    /// `extra` permissions append to the default ones; other keys replace.
    fn start_with(dir: &Path, base: &str, extra: Value) -> Client {
        let (client_w, server_r) = tokio::io::duplex(1 << 20);
        let (server_w, client_r) = tokio::io::duplex(1 << 20);
        let mut cfg = json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"},
                "models": {"m": {"limit": {"context": 200000}, "cost": {"input": 1.0, "output": 2.0}}, "other": {"name": "Other"}}}},
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]
        });
        for (k, v) in extra.as_object().unwrap() {
            if k == "permissions" {
                cfg[k]
                    .as_array_mut()
                    .unwrap()
                    .extend(v.as_array().unwrap().clone());
            } else {
                cfg[k] = v.clone();
            }
        }
        let config: config::Config = serde_json::from_value(cfg).unwrap();
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

    fn kinds(seen: &[Value]) -> Vec<String> {
        seen.iter()
            .map(|m| match m["method"].as_str() {
                Some("session/update") => m["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                Some(other) => other.to_string(),
                None => "response".into(),
            })
            .collect()
    }

    fn allow_once(_: &Value) -> Option<Value> {
        Some(json!({"outcome": {"outcome": "selected", "optionId": "once"}}))
    }

    fn option(r: &Value, id: &str) -> Value {
        r["result"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == id)
            .cloned()
            .unwrap()
    }

    async fn tmp() -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        (dir, cwd, fake_model().await)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_from_initialize_to_load() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let init = c
            .request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await;
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);

        let new = c
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
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
                allow_once(m)
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
        let upd = updates(&seen, "tool_call_update");
        assert_eq!(upd.last().unwrap()["status"], "completed");
        assert!(upd.last().unwrap()["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("acp"));
        let text: String = updates(&seen, "agent_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert!(text.starts_with("done:"), "{text}");

        let id = c.call("session/list", json!({"cwd": cwd})).await;
        let (list, _) = c.until(id, |_| None).await;
        assert!(list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["sessionId"] == sid.as_str()));

        let err = c.request("nope/method", json!({})).await;
        assert_eq!(err["error"]["code"], -32601);
        assert_eq!(err["error"]["data"]["method"], "nope/method");
        c.w.write_all(b"not json\n").await.unwrap();
        assert_eq!(c.recv().await["error"]["code"], -32700);
    }

    // session.test.ts: "initialize advertises capabilities, negotiates the
    // version, and offers terminal auth only when asked"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_initialize_advertises_capabilities_and_auth() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let init = c
            .request(
                "initialize",
                json!({"protocolVersion": 99, "clientCapabilities": {}}),
            )
            .await;
        let r = &init["result"];
        assert_eq!(r["protocolVersion"], 1, "negotiates down to v1");
        assert_eq!(
            r["agentCapabilities"]["sessionCapabilities"],
            json!({"additionalDirectories": {}, "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {}})
        );
        assert_eq!(
            r["agentCapabilities"]["mcpCapabilities"],
            json!({"http": true, "sse": false})
        );
        assert_eq!(
            r["authMethods"],
            json!([{"id": "codesmith-login", "name": "Login with codesmith", "description": "Run `codesmith auth login <provider>` in the terminal"}])
        );
        let ok = c
            .request("authenticate", json!({"methodId": "codesmith-login"}))
            .await;
        assert_eq!(ok["result"], json!({}));
        let bad = c
            .request("authenticate", json!({"methodId": "missing"}))
            .await;
        assert_eq!(
            bad["error"],
            json!({"code": -32602, "message": "Invalid params: unknown auth method: missing", "data": {"methodId": "missing"}})
        );
    }

    // session.test.ts: "rejects relative cwds, MCP-over-ACP, and SSE servers
    // before creating or loading a session"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_rejects_relative_cwds_and_unsupported_mcp_servers() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let unsupported = json!({"code": -32602, "message": "Invalid params: Only stdio and HTTP MCP servers are supported", "data": {"field": "mcpServers"}});
        let relative = json!({"code": -32602, "message": "Invalid params: cwd must be an absolute path: workspace", "data": {"field": "cwd"}});
        let acp = json!([{"type": "acp", "name": "client", "serverId": "mcp_client"}]);
        let sse = json!([{"type": "sse", "name": "events", "url": "https://example.com/sse", "headers": []}]);
        for servers in [acp, sse] {
            let r = c
                .request("session/new", json!({"cwd": cwd, "mcpServers": servers}))
                .await;
            assert_eq!(r["error"], unsupported);
        }
        let r = c
            .request("session/new", json!({"cwd": "workspace", "mcpServers": []}))
            .await;
        assert_eq!(r["error"], relative);
        let r = c
            .request(
                "session/load",
                json!({"cwd": "workspace", "sessionId": "ses_x", "mcpServers": []}),
            )
            .await;
        assert_eq!(r["error"], relative);
        let r = c.request("session/list", json!({"cwd": "workspace"})).await;
        assert_eq!(r["error"], relative);
        let r = c.request("session/list", json!({"cwd": ""})).await;
        assert_eq!(r["error"]["data"], json!({"field": "cwd"}));
        let r = c
            .request(
                "session/load",
                json!({"cwd": cwd, "sessionId": "ses_missing", "mcpServers": []}),
            )
            .await;
        assert_eq!(
            r["error"],
            json!({"code": -32602, "message": "Invalid params: session not found: ses_missing", "data": {"sessionId": "ses_missing"}})
        );
        let r = c.request("session/list", json!({})).await;
        assert_eq!(r["result"]["sessions"], json!([]));
    }

    // session.test.ts: "loads with paginated replay while resume and fork do
    // not replay" and "publishes a session's commands after the response
    // that attaches it"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_load_replays_while_resume_and_fork_do_not() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        c.until(id, allow_once).await;

        let mut c2 = start(&cwd, &base);
        let params = json!({"cwd": cwd, "sessionId": sid, "mcpServers": []});
        let id = c2.call("session/load", params.clone()).await;
        let (loaded, seen) = c2.until(id, |_| None).await;
        assert!(loaded.get("error").is_none(), "{loaded}");
        let users = updates(&seen, "user_message_chunk");
        assert_eq!(users.len(), 1);
        assert_eq!(
            users[0]["content"],
            json!({"type": "text", "text": "run it"})
        );
        assert!(users[0]["messageId"].as_str().unwrap().starts_with(&sid));
        // The commands follow the response.
        let (cmds, before) = c2
            .wait(|m| m["params"]["update"]["sessionUpdate"] == "available_commands_update")
            .await;
        assert!(before.is_empty(), "{before:?}");
        assert!(cmds["params"]["update"]["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "compact"));

        let id = c2.call("session/resume", params.clone()).await;
        let (resumed, seen) = c2.until(id, |_| None).await;
        assert!(resumed["result"]["configOptions"].is_array());
        assert_eq!(kinds(&seen), Vec::<String>::new(), "resume does not replay");

        let id = c2.call("session/fork", params.clone()).await;
        let (forked, seen) = c2.until(id, |_| None).await;
        let fork = forked["result"]["sessionId"].as_str().unwrap().to_string();
        assert_ne!(fork, sid);
        assert!(updates(&seen, "user_message_chunk").is_empty());
        // The fork has the history and is its own session.
        let mut c3 = start(&cwd, &base);
        let id = c3
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": fork, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c3.until(id, |_| None).await;
        assert_eq!(updates(&seen, "user_message_chunk").len(), 1);

        let elsewhere = cwd.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        for method in ["session/load", "session/fork"] {
            let r = c2
                .request(
                    method,
                    json!({"cwd": elsewhere, "sessionId": sid, "mcpServers": []}),
                )
                .await;
            assert_eq!(r["error"]["code"], -32602);
            assert_eq!(
                r["error"]["data"],
                json!({"sessionId": sid, "cwd": elsewhere})
            );
        }
    }

    // session.test.ts: "close removes a new session that was never prompted
    // and keeps every other session"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_close_keeps_no_unprompted_session() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let unused = c.new_session(&cwd).await;
        let prompted = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": prompted, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        c.until(id, allow_once).await;
        for s in [&unused, &prompted] {
            let r = c.request("session/close", json!({"sessionId": s})).await;
            assert_eq!(r["result"], json!({}));
        }
        let list = c.request("session/list", json!({"cwd": cwd})).await;
        let ids: Vec<&str> = list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [prompted.as_str()]);
    }

    // prompt.test.ts: "streams an admitted turn and resolves with usage
    // after its terminal event"; translate.test.ts: "streams text and
    // reasoning with one message ID per reasoning ordinal", "ends a
    // length-limited step with max_tokens", "sums usage across steps"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_prompt_streams_message_ids_and_resolves_with_usage() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "think"}]}),
            )
            .await;
        let (done, seen) = c.until(id, |_| None).await;
        assert_eq!(
            done["result"],
            json!({"stopReason": "max_tokens", "usage": {"inputTokens": 100, "outputTokens": 40, "cachedReadTokens": 11, "totalTokens": 151}})
        );
        let streamed: Vec<(String, String)> = seen
            .iter()
            .filter(|m| {
                matches!(
                    m["params"]["update"]["sessionUpdate"].as_str(),
                    Some("agent_message_chunk" | "agent_thought_chunk")
                )
            })
            .map(|m| {
                let u = &m["params"]["update"];
                (
                    u["sessionUpdate"].as_str().unwrap().to_string(),
                    u["messageId"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let mid = &streamed[2].1;
        assert!(mid.starts_with("msg_"));
        let expected: Vec<(String, String)> = [
            ("agent_thought_chunk", format!("{mid}:reasoning:0")),
            ("agent_thought_chunk", format!("{mid}:reasoning:0")),
            ("agent_message_chunk", mid.clone()),
            ("agent_thought_chunk", format!("{mid}:reasoning:1")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(streamed, expected);
        let usage = updates(&seen, "usage_update");
        assert_eq!(usage.len(), 1);
        assert_eq!(
            (
                &usage[0]["used"],
                &usage[0]["size"],
                &usage[0]["cost"]["currency"]
            ),
            (&json!(151), &json!(200000), &json!("USD"))
        );
        let cost = usage[0]["cost"]["amount"].as_f64().unwrap();
        assert!((cost - 180e-6).abs() < 1e-9, "{cost}");
        // The title is published once generated.
        assert_eq!(
            updates(&seen, "session_info_update")
                .iter()
                .find(|u| u.get("title").is_some())
                .map(|u| u["title"].clone()),
            Some(json!("A title"))
        );
    }

    // prompt.test.ts (tool lifecycle part) and permission.test.ts:
    // "forwards allow-once and allow-always selections to the server"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_tool_calls_and_permission_selections() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let prompt =
            |text: &str| json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        let mut asked = Vec::new();
        let id = c.call("session/prompt", prompt("run it")).await;
        let (done, seen) = c
            .until(id, |m| {
                asked.push(m["params"].clone());
                Some(json!({"outcome": {"outcome": "selected", "optionId": "always"}}))
            })
            .await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert_eq!(asked.len(), 1);
        let p = &asked[0];
        assert_eq!(p["sessionId"], sid.as_str());
        assert_eq!(
            p["options"],
            json!([
                {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
                {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
                {"optionId": "reject", "kind": "reject_once", "name": "Reject"},
            ])
        );
        let tc = &p["toolCall"];
        assert_eq!(
            (&tc["toolCallId"], &tc["status"], &tc["title"], &tc["kind"]),
            (
                &json!("c1"),
                &json!("pending"),
                &json!("echo acp"),
                &json!("execute")
            )
        );
        assert_eq!(tc["locations"], json!([{"path": cwd}]));
        assert_eq!(tc["rawInput"]["cwd"], json!(cwd));
        let lifecycle: Vec<(String, String)> = seen
            .iter()
            .filter(|m| m["params"]["update"]["toolCallId"] == "c1")
            .map(|m| {
                let u = &m["params"]["update"];
                (
                    u["sessionUpdate"].as_str().unwrap().into(),
                    u["status"].as_str().unwrap().into(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = [
            ("tool_call", "pending"),
            ("tool_call_update", "in_progress"),
            ("tool_call_update", "completed"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(lifecycle, expected);
        let call = updates(&seen, "tool_call")[0];
        assert_eq!(
            (&call["name"], &call["title"], &call["rawInput"]["command"]),
            (&json!("shell"), &json!("echo acp"), &json!("echo acp"))
        );
        // "always" covers the next call.
        let id = c.call("session/prompt", prompt("run it")).await;
        let (_, seen) = c.until(id, |_| panic!("asked again")).await;
        assert_eq!(
            updates(&seen, "tool_call_update").last().unwrap()["status"],
            "completed"
        );

        // A rejected or cancelled permission fails the call, with its error.
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        for outcome in [
            json!({"outcome": "selected", "optionId": "reject"}),
            json!({"outcome": "cancelled"}),
        ] {
            let id = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
                )
                .await;
            let (done, seen) = c
                .until(id, |_| Some(json!({"outcome": outcome.clone()})))
                .await;
            assert_eq!(done["result"]["stopReason"], "end_turn");
            let failed = *updates(&seen, "tool_call_update").last().unwrap();
            assert_eq!(failed["status"], "failed");
            assert_eq!(failed["kind"], "execute");
            assert_eq!(failed["title"], "echo acp");
            assert_eq!(
                failed["rawOutput"]["error"],
                failed["content"][0]["content"]["text"]
            );
        }
    }

    // prompt.test.ts: "reports full-file diffs for a completed edit and
    // patch"; permission.test.ts: "previews added files as new"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_edits_report_full_file_diffs() {
        let (_d, cwd, base) = tmp().await;
        std::fs::write(cwd.join("old.txt"), "one\r\nthree\r\n").unwrap();
        let mut c = start_with(
            &cwd,
            &base,
            json!({"permissions": [{"action": "edit", "resource": "*", "effect": "ask"}]}),
        );
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "edit files"}]}),
            )
            .await;
        let mut previews = Vec::new();
        let (done, seen) = c
            .until(id, |m| {
                previews.push(m["params"]["toolCall"].clone());
                allow_once(m)
            })
            .await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        let (new, old) = (cwd.join("new.txt"), cwd.join("old.txt"));
        assert_eq!(previews.len(), 2);
        assert_eq!(previews[0]["title"], "new.txt");
        assert_eq!(
            previews[0]["content"],
            json!([{"type": "diff", "path": new, "oldText": null, "newText": "two\n"}])
        );
        assert_eq!(
            previews[1]["content"],
            json!([{"type": "diff", "path": old, "oldText": "one\r\nthree\r\n", "newText": "uno\r\nthree\r\n"}])
        );
        assert_eq!(previews[1]["locations"], json!([{"path": old}]));
        let completed: Vec<&Value> = updates(&seen, "tool_call_update")
            .into_iter()
            .filter(|u| u["status"] == "completed")
            .collect();
        assert_eq!(completed[0]["content"][1], previews[0]["content"][0]);
        assert_eq!(completed[1]["content"][1], previews[1]["content"][0]);
        assert_eq!(completed[0]["content"][0]["type"], "content");
    }

    // prompt.test.ts: "session/cancel forwards the server's wind-down before
    // resolving cancelled" and "stops waiting for a wind-down that never
    // ends and fails the tools left running"; permission.test.ts:
    // "cancelling the turn cancels its pending session/request_permission"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_cancel_withdraws_the_permission_and_fails_the_tool() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let pid = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        let (ask, _) = c
            .wait(|m| m["method"] == "session/request_permission")
            .await;
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        let (done, seen) = c.until(pid, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled");
        assert!(seen.contains(
            &json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": ask["id"]}})
        ));
        let failed = updates(&seen, "tool_call_update");
        assert_eq!(failed.last().unwrap()["status"], "failed", "{seen:?}");
        assert_eq!(failed.last().unwrap()["toolCallId"], "c1");
        // The session works again afterwards.
        let r = c
            .request(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "plan"}),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "think"}]}),
            )
            .await;
        let (done, _) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "max_tokens");
    }

    /// Regression: a turn that finished while it was being cancelled used to
    /// answer `end_turn` (or put back a stale session).
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_turn_waiting_for_permission() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        for i in 0..50 {
            let pid = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
                )
                .await;
            c.wait(|m| m["method"] == "session/request_permission")
                .await;
            c.notify("session/cancel", json!({"sessionId": sid})).await;
            let (done, seen) = c.until(pid, |_| None).await;
            assert_eq!(done["result"]["stopReason"], "cancelled", "iteration {i}");
            assert_eq!(
                seen.iter()
                    .filter(|m| m.get("method").is_none() && m["id"] == json!(pid))
                    .count(),
                0
            );
        }
        // Exactly one answer per prompt: nothing else is pending.
        let r = c
            .request(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "plan"}),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
    }

    // prompt.test.ts: "$/cancel_request on the prompt request cancels the
    // turn like session/cancel"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_cancel_request_on_the_prompt_cancels_the_turn() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let pid = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        c.wait(|m| m["method"] == "session/request_permission")
            .await;
        c.notify("$/cancel_request", json!({"requestId": pid}))
            .await;
        let (done, _) = c.until(pid, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled");
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "think"}]}),
            )
            .await;
        assert_eq!(
            c.until(id, |_| None).await.0["result"]["stopReason"],
            "max_tokens"
        );
    }

    // prompt.test.ts: "session/close and session/delete settle the active
    // turn before responding and detach only that session"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_close_and_delete_settle_the_active_turn() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        for method in ["session/close", "session/delete"] {
            let sid = c.new_session(&cwd).await;
            let other = c.new_session(&cwd).await;
            let prompt =
                |s: &str, t: &str| json!({"sessionId": s, "prompt": [{"type": "text", "text": t}]});
            let pid = c.call("session/prompt", prompt(&sid, "run it")).await;
            c.wait(|m| m["method"] == "session/request_permission")
                .await;
            // A second prompt while one runs is refused.
            let busy = c.request("session/prompt", prompt(&sid, "again")).await;
            assert_eq!(
                busy["error"]["message"],
                format!("Internal error: Session already has an active ACP prompt: {sid}")
            );
            let cid = c.call(method, json!({"sessionId": sid})).await;
            let (closed, seen) = c.until(cid, |_| None).await;
            assert_eq!(closed["result"], json!({}));
            let answered = seen
                .iter()
                .find(|m| m["id"] == json!(pid) && m.get("method").is_none())
                .expect("the prompt is answered before the close");
            assert_eq!(answered["result"]["stopReason"], "cancelled");
            let r = c.request("session/prompt", prompt(&sid, "again")).await;
            assert_eq!(r["error"]["code"], -32602);
            assert_eq!(r["error"]["data"], json!({"sessionId": sid}));
            let id = c.call("session/prompt", prompt(&other, "think")).await;
            assert_eq!(
                c.until(id, |_| None).await.0["result"]["stopReason"],
                "max_tokens"
            );
            let listed = c.request("session/list", json!({"cwd": cwd})).await;
            let kept = listed["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["sessionId"] == sid.as_str());
            assert_eq!(kept, method == "session/close");
        }
    }

    // catalog.test.ts: "switches model, effort, and mode against the warm
    // catalog"; config-option.test.ts; error.ts config errors
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_config_options_switch_model_and_mode() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let new = c
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let model = option(&new, "model");
        assert_eq!(
            (&model["currentValue"], &model["category"], &model["type"]),
            (&json!("fake/m"), &json!("model"), &json!("select"))
        );
        assert_eq!(
            model["options"],
            json!([{"value": "fake/m", "name": "fake/m"}, {"value": "fake/other", "name": "fake/Other"}])
        );
        let mode = option(&new, "mode");
        assert_eq!(mode["currentValue"], "build");
        assert!(mode["options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["value"] != "explore"));

        let set =
            |id: &str, value: Value| json!({"sessionId": sid, "configId": id, "value": value});
        let r = c
            .request(
                "session/set_config_option",
                set("model", json!("fake/other")),
            )
            .await;
        assert_eq!(option(&r, "model")["currentValue"], "fake/other");
        let id = c
            .call("session/set_config_option", set("mode", json!("plan")))
            .await;
        let (r, seen) = c.until(id, |_| None).await;
        assert_eq!(option(&r, "mode")["currentValue"], "plan");
        assert_eq!(
            updates(&seen, "current_mode_update")[0]["currentModeId"],
            "plan"
        );
        for (params, error) in [
            (
                set("model", json!("nope/x")),
                json!({"code": -32602, "message": "Invalid params: model not found: nope/x", "data": {"providerId": "nope", "modelId": "nope/x"}}),
            ),
            (
                set("mode", json!("explore")),
                json!({"code": -32602, "message": "Invalid params: mode not found: explore", "data": {"mode": "explore"}}),
            ),
            (
                set("effort", json!("high")),
                json!({"code": -32602, "message": "Invalid params: unknown config option: effort", "data": {"configId": "effort"}}),
            ),
            (
                set("model", json!(true)),
                json!({"code": -32602, "message": "Invalid params: unknown config option: model", "data": {"configId": "model"}}),
            ),
        ] {
            assert_eq!(
                c.request("session/set_config_option", params).await["error"],
                error
            );
        }
        // A switch during a turn applies once it ends, and is stored.
        let pid = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "think"}]}),
            )
            .await;
        let sm = c
            .call(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "build"}),
            )
            .await;
        let mut answered = 0;
        while answered < 2 {
            let m = c.recv().await;
            if m.get("method").is_none() && (m["id"] == json!(pid) || m["id"] == json!(sm)) {
                assert!(m.get("error").is_none(), "{m}");
                answered += 1;
            }
        }
        let mut c2 = start(&cwd, &base);
        let r = c2
            .request(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        assert_eq!(option(&r, "model")["currentValue"], "fake/other");
        assert_eq!(option(&r, "mode")["currentValue"], "build");
        assert_eq!(r["result"]["modes"]["currentModeId"], "build");
    }

    // additional-directories.test.ts: "rejects %p before creating a
    // session"; additional-directories.subprocess.test.ts: "tools read files
    // in an additional directory without an external directory ask"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_additional_directories() {
        let (_d, cwd, base) = tmp().await;
        let shared = tempfile::tempdir().unwrap();
        let shared = shared.path().canonicalize().unwrap();
        std::fs::write(shared.join("lib.txt"), "shared text\n").unwrap();
        let mut c = start_with(
            &cwd,
            &base,
            json!({"permissions": [{"action": "external_directory", "resource": "*", "effect": "ask"}]}),
        );
        for bad in ["shared/lib", "", "/shared/*", "/shared/lib?"] {
            let r = c
                .request(
                    "session/new",
                    json!({"cwd": cwd, "additionalDirectories": ["/shared/ok", bad], "mcpServers": []}),
                )
                .await;
            assert_eq!(r["error"]["code"], -32602);
            assert_eq!(r["error"]["data"], json!({"additionalDirectory": bad}));
        }
        let file = shared.join("lib.txt").display().to_string();
        let read = json!([{"type": "text", "text": format!("read {file}")}]);
        let r = c
            .request(
                "session/new",
                json!({"cwd": cwd, "additionalDirectories": [shared], "mcpServers": []}),
            )
            .await;
        let granted = r["result"]["sessionId"].as_str().unwrap().to_string();
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": granted, "prompt": read}),
            )
            .await;
        let (_, seen) = c
            .until(id, |m| panic!("asked for a granted directory: {m}"))
            .await;
        assert!(updates(&seen, "agent_message_chunk")
            .iter()
            .any(|u| u["content"]["text"]
                .as_str()
                .unwrap()
                .contains("shared text")));
        // Without the grant the same read asks.
        let plain = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": plain, "prompt": read}),
            )
            .await;
        let mut asked = 0;
        c.until(id, |m| {
            asked += 1;
            assert_eq!(m["params"]["toolCall"]["title"], file);
            allow_once(m)
        })
        .await;
        assert_eq!(asked, 1);
    }

    // elicitation.test.ts: "answers a question form through elicitation and
    // continues the turn" and "cancels the form when the client declines"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_question_tool_through_elicitation() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        c.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"elicitation": {"form": {}}}}),
        )
        .await;
        let sid = c.new_session(&cwd).await;
        let ask = json!({"sessionId": sid, "prompt": [{"type": "text", "text": "ask me"}]});
        let id = c.call("session/prompt", ask.clone()).await;
        let mut forms = Vec::new();
        let (done, seen) = c
            .until(id, |m| {
                assert_eq!(m["method"], "elicitation/create");
                forms.push(m["params"].clone());
                Some(json!({"action": "accept", "content": {"q0": "Bun", "q0_custom": ""}}))
            })
            .await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert_eq!(forms.len(), 1);
        let f = &forms[0];
        assert_eq!(
            (&f["mode"], &f["sessionId"], &f["toolCallId"], &f["message"]),
            (
                &json!("form"),
                &json!(sid),
                &json!("c1"),
                &json!("Which runtime?")
            )
        );
        assert_eq!(
            f["requestedSchema"]["properties"]["q0"]["oneOf"][0],
            json!({"const": "Bun", "title": "Bun", "description": "Fast"})
        );
        let text: String = updates(&seen, "agent_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert!(text.contains("\"Which runtime?\"=\"Bun\""), "{text}");

        let id = c.call("session/prompt", ask).await;
        let (_, seen) = c.until(id, |_| Some(json!({"action": "decline"}))).await;
        let failed = updates(&seen, "tool_call_update");
        assert_eq!(failed.last().unwrap()["status"], "failed");
        assert!(failed.last().unwrap()["rawOutput"]["error"]
            .as_str()
            .unwrap()
            .contains("dismissed"));
    }

    // elicitation.test.ts: "cancels forms from unsupported clients, unknown
    // flows, and credential-looking fields" and the custom-answer merge
    #[test]
    fn oc_acp_elicitation_mapping() {
        let q = |question: &str, multiple: bool| Question {
            question: question.into(),
            header: String::new(),
            options: vec![codesmith_core::question::Choice {
                label: "Fast".into(),
                description: "Speed".into(),
            }],
            multiple,
        };
        let qs = [q("Runtime?", false), q("Goals?", true)];
        let schema = question_schema(&qs);
        assert_eq!(schema["properties"]["q1"]["type"], "array");
        assert_eq!(schema["properties"]["q1_custom"]["title"], "Goals? (other)");
        let accept = |content: Value| json!({"action": "accept", "content": content});
        assert_eq!(
            question_answers(
                &qs,
                &accept(
                    json!({"q0": "Fast", "q0_custom": "Bun", "q1": ["Fast"], "q1_custom": "Small"})
                )
            ),
            Some(vec![
                vec!["Bun".to_string()],
                vec!["Fast".into(), "Small".into()]
            ])
        );
        assert_eq!(
            question_answers(&qs, &accept(json!({}))),
            Some(vec![vec![], vec![]])
        );
        assert_eq!(
            question_answers(&qs, &accept(json!({"q0": {"nested": true}}))),
            None
        );
        assert_eq!(question_answers(&qs, &json!({"action": "decline"})), None);
        assert_eq!(question_answers(&qs, &json!({"action": "cancel"})), None);
        for secret in [
            "api_key",
            "GitHub token",
            "Password",
            "Paste your API key",
            "private key",
        ] {
            assert!(credential_like(secret), "{secret}");
        }
        assert!(!credential_like("Which runtime?"));
    }

    // replay.test.ts: "replays user, text, reasoning, and tool messages in
    // order on session/load"; translate.test.ts: "replays reasoning with the
    // same message IDs as live reasoning ordinals"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_replay_shape() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        for text in ["think", "read missing.txt"] {
            let id = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
                )
                .await;
            c.until(id, allow_once).await;
        }
        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c2.until(id, |_| None).await;
        assert_eq!(
            kinds(&seen),
            [
                "user_message_chunk",
                "agent_thought_chunk",
                "agent_message_chunk",
                "user_message_chunk",
                "tool_call",
                "tool_call_update",
                "agent_message_chunk",
            ]
        );
        let u: Vec<&Value> = seen.iter().map(|m| &m["params"]["update"]).collect();
        let mid = u[2]["messageId"].as_str().unwrap();
        assert_eq!(u[1]["messageId"], format!("{mid}:reasoning:0"));
        // CodeSmith stores a step's reasoning as one part.
        assert_eq!(u[1]["content"]["text"], "think-1 continuedthink-2");
        assert_eq!(
            (&u[4]["toolCallId"], &u[4]["status"], &u[4]["kind"]),
            (&json!("c1"), &json!("pending"), &json!("read"))
        );
        assert_eq!(
            u[4]["locations"],
            json!([{"path": cwd.join("missing.txt")}])
        );
        assert_eq!(
            (&u[5]["toolCallId"], &u[5]["status"]),
            (&json!("c1"), &json!("failed"))
        );
    }

    // compaction.test.ts: "streams a /compact turn's summary and completes
    // it" and "treats a null compaction capability as unsupported";
    // translate.test.ts: "replays a completed compaction as one terminal
    // update" and "replays a compaction as a marker when the client lacks
    // the capability"
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_compaction_updates() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start_with(&cwd, &base, json!({"compaction": {"keep": {"tokens": 0}}}));
        c.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"session": {"compaction": {}}}}),
        )
        .await;
        let sid = c.new_session(&cwd).await;
        for text in ["think", "think again", "/compact"] {
            let id = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
                )
                .await;
            let (done, seen) = c.until(id, |_| None).await;
            assert!(done.get("error").is_none(), "{done}");
            if text == "/compact" {
                let u = updates(&seen, "compaction_update");
                assert_eq!(u.len(), 1, "{seen:?}");
                assert_eq!(u[0]["status"], "completed");
            }
        }
        for (caps, kind) in [
            (json!({"session": {"compaction": {}}}), "compaction_update"),
            (
                json!({"session": {"compaction": null}}),
                "session_info_update",
            ),
        ] {
            let mut c2 = start(&cwd, &base);
            c2.request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": caps}),
            )
            .await;
            let id = c2
                .call(
                    "session/load",
                    json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
                )
                .await;
            let (_, seen) = c2.until(id, |_| None).await;
            // Everything is replayed, the summarised turns included.
            assert_eq!(updates(&seen, "user_message_chunk").len(), 2);
            let marker = updates(&seen, kind);
            assert_eq!(marker.len(), 1, "{seen:?}");
            if kind == "compaction_update" {
                assert_eq!(marker[0]["summary"][0]["text"], "## Objective\nShip it.");
            } else {
                assert!(marker[0]["_meta"][COMPACTION_META]["messageId"].is_string());
            }
        }
    }

    // translate.test.ts / prompt.test.ts retry metadata: a scheduled retry is
    // published on session_info_update and cleared when the next step starts.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_retry_is_published_and_cleared() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "flaky"}]}),
            )
            .await;
        let (done, seen) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        let retries: Vec<&Value> = updates(&seen, "session_info_update")
            .into_iter()
            .filter_map(|u| u["_meta"].get(RETRY_META))
            .collect();
        assert_eq!(retries.len(), 2, "{seen:?}");
        assert_eq!(retries[0]["attempt"], 1);
        assert!(retries[0]["nextRetryAt"].is_string());
        assert!(retries[1].is_null());
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
        // session.test.ts: "converts MCP configs"
        let s = mcp_servers(&json!([
            {"name": "fs", "command": "npx", "args": ["-y", "srv"], "env": [{"name": "K", "value": "v"}]},
            {"type": "http", "name": "web", "url": "https://m/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]},
        ]))
        .unwrap();
        assert!(
            matches!(&s["fs"], ServerConfig::Local { command, environment, .. } if command == &["npx", "-y", "srv"] && environment["K"] == "v")
        );
        assert!(
            matches!(&s["web"], ServerConfig::Remote { headers, .. } if headers["Authorization"] == "Bearer t")
        );
        assert!(mcp_servers(
            &json!([{"type": "sse", "name": "old", "url": "https://m/sse", "headers": []}])
        )
        .is_err());
        assert_eq!(tool_kind("edit"), "edit");
        assert_eq!(tool_kind("explore › grep"), "search");
        assert_eq!(tool_kind("github_search"), "other");
        assert_eq!(stop_reason(Some("max_tokens")), "max_tokens");
        assert_eq!(stop_reason(Some("content_filter")), "refusal");
        assert_eq!(stop_reason(Some("stop")), "end_turn");
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
