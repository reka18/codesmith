//! `codesmith acp`: the Agent Client Protocol (v1) over stdio, so editors
//! such as Zed can drive CodeSmith sessions. Newline-delimited JSON-RPC 2.0
//! on stdin/stdout; logs go to stderr.
//!
//! The wire shapes follow OpenCode's ACP agent (`packages/cli/src/acp`):
//! its error codes and messages, tool call lifecycle, permission options,
//! edit diffs, replay, config options and elicitation.

use anyhow::{anyhow, Result};
use codesmith_ai::{FinishReason, Media, Message, Part, Reason, Role, Usage};
use codesmith_core::config::Loaded;
use codesmith_core::mcp::{Mcp, ServerConfig};
use codesmith_core::media::{self, ImageLimits};
use codesmith_core::permission::{Approvals, Asker, Effect, Reply, Request, Rule};
use codesmith_core::question::{Prompt as Question, Questioner};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::{self, Record, Store};
use codesmith_core::tool::shell::Jobs;
use codesmith_core::tool::todo::{Status, Todo};
use codesmith_core::{agent, command, config};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
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

/// A tool's own name: subagent tools arrive as "agent › tool". The patch
/// tool's former name, `apply_patch`, is `patch` (OpenCode's V1 alias).
fn base_name(name: &str) -> &str {
    match name.rsplit(" › ").next().unwrap_or(name) {
        "apply_patch" => "patch",
        n => n,
    }
}

pub fn tool_kind(name: &str) -> &'static str {
    match base_name(name) {
        "read" | "skill" => "read",
        "write" | "edit" | "patch" => "edit",
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

/// The files a patch call changes, each with the file its text comes from.
fn patch_files(cwd: &Path, input: &Value) -> Vec<(PathBuf, PathBuf)> {
    codesmith_core::tool::apply_patch::touched(cwd, input["patchText"].as_str().unwrap_or_default())
}

/// The files a call is about to change, each with its text now, so the
/// change can be shown when it ends.
fn files_before(cwd: &Path, name: &str, input: &Value) -> Vec<Before> {
    let files = match base_name(name) {
        "edit" | "write" => file_path(input)
            .map(|p| absolute(cwd, p))
            .map(|p| vec![(p.clone(), p)])
            .unwrap_or_default(),
        "patch" => patch_files(cwd, input),
        _ => vec![],
    };
    files
        .into_iter()
        .map(|(path, source)| {
            let old = std::fs::read_to_string(&source).ok();
            (path, old)
        })
        .collect()
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
        // Every file a patch names, a move's source and destination both.
        "patch" => {
            let files = patch_files(cwd, input);
            let mut paths: Vec<PathBuf> = Vec::new();
            for (dest, source) in files {
                for p in [source, dest] {
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
            return json!(paths.iter().map(|p| json!({"path": p})).collect::<Vec<_>>());
        }
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
    media: &[Media],
) -> Value {
    let mut content = vec![text_content(output)];
    content.extend(tool_media_content(media));
    if failed {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "failed",
            "kind": tool_kind(name),
            "title": tool_title(name, input),
            "locations": locations(cwd, name, input),
            "rawInput": raw_input(cwd, name, input),
            "content": content,
            "rawOutput": {"error": clip(output, 20_000)},
        })
    } else {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": "completed",
            "locations": locations(cwd, name, input),
            "content": content,
        })
    }
}

/// What an edit, write or patch would change, shown with its permission
/// request: a diff per file (per hunk, for a patch).
fn previews(cwd: &Path, name: &str, input: &Value) -> Vec<Value> {
    if base_name(name) == "patch" {
        let text = input["patchText"].as_str().unwrap_or_default();
        return codesmith_core::tool::apply_patch::preview(cwd, text)
            .into_iter()
            .map(|p| diff(&p.path, p.old, p.new.unwrap_or_default()))
            .collect();
    }
    preview(cwd, name, input).into_iter().collect()
}

/// What an edit or write would change.
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
        "patch" => {
            let files = patch_files(Path::new(""), input);
            (!files.is_empty()).then(|| {
                files
                    .iter()
                    .map(|(p, _)| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }
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

/// OpenCode's `resolveStopReason`, over the last step's finish reason.
fn stop_reason(finish: Option<Reason>) -> &'static str {
    match finish {
        Some(Reason::Length) => "max_tokens",
        Some(Reason::ContentFilter) => "refusal",
        _ => "end_turn",
    }
}

/// What the running turn has used, shared so a cancel can report it.
#[derive(Default)]
struct TurnStats {
    usage: Option<Usage>,
    last: Option<Usage>,
    finish: Option<Reason>,
}

/// The client asks for subagent activity apart, as OpenCode's
/// `opencode/session/child_update` notifications.
const CHILD_UPDATES: &str = "opencode/child-session-updates";
const CHILD_UPDATE_METHOD: &str = "opencode/session/child_update";
const CHILD_META: &str = "opencode/child-session";

/// A subagent session whose activity streams within the turn.
#[derive(Clone, Debug, PartialEq)]
struct Child {
    id: String,
    parent: String,
    depth: u32,
    title: String,
    /// Its message being streamed.
    message: String,
}

impl Child {
    /// OpenCode's `ACPChild.meta`.
    fn meta(&self) -> Value {
        let mut m = json!({"id": self.id, "parentID": self.parent, "depth": self.depth});
        if !self.title.is_empty() {
            m["title"] = json!(self.title);
        }
        json!({CHILD_META: m})
    }

    /// OpenCode's `ACPChild.project`: the update marked as the child's,
    /// its tool call ids made unique and titles prefixed.
    fn project(&self, mut update: Value) -> Value {
        let meta = self.meta();
        match update.get_mut("_meta").and_then(Value::as_object_mut) {
            Some(m) => m.extend(meta.as_object().unwrap().clone()),
            None => update["_meta"] = meta,
        }
        if matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            if let Some(id) = update["toolCallId"].as_str() {
                update["toolCallId"] = json!(format!("{}:{id}", self.id));
            }
            if let (Some(t), false) = (update["title"].as_str(), self.title.is_empty()) {
                update["title"] = json!(format!("{}: {t}", self.title));
            }
        }
        update
    }

    /// OpenCode's `ACPChild.update`: a `child_update` notification's params.
    fn notification(&self, root: &str, event: Value) -> Value {
        let mut p = json!({
            "rootSessionId": root,
            "childSessionId": self.id,
            "parentSessionId": self.parent,
            "depth": self.depth,
        });
        if !self.title.is_empty() {
            p["title"] = json!(self.title);
        }
        p.as_object_mut()
            .unwrap()
            .extend(event.as_object().unwrap().clone());
        p
    }
}

/// Send `update` for session `sid`, or for its subagent `child`: apart,
/// when the client takes child updates, else projected into the session's
/// own stream (OpenCode's `route`; every child here runs within the turn).
fn route(conn: &Conn, sid: &str, child: Option<&Child>, apart: bool, update: Value) {
    match child {
        None => conn.update(sid, update),
        Some(c) if apart => conn.notify(
            CHILD_UPDATE_METHOD,
            c.notification(sid, json!({"type": "update", "update": c.project(update)})),
        ),
        Some(c) => conn.update(sid, c.project(update)),
    }
}

/// The tool call that is running, so permission requests can point at it.
#[derive(Clone, Debug)]
struct Call {
    /// The id the client knows it by, before a child's projection.
    id: String,
    name: String,
    input: Value,
    /// The subagent that made it.
    child: Option<Child>,
}

impl Call {
    /// The id the client sees.
    fn client_id(&self) -> String {
        match &self.child {
            Some(c) => format!("{}:{}", c.id, self.id),
            None => self.id.clone(),
        }
    }
}

type Current = Arc<Mutex<Option<Call>>>;

/// A file an edit is about to change, and its text (`None`: new file).
type Before = (PathBuf, Option<String>);

/// The compaction that is running (its id and reason), so a cancel can
/// settle it.
type Compacting = Arc<Mutex<Option<(String, &'static str)>>>;

/// A compaction's `session/update`, as OpenCode's `ACPCompaction.update`
/// sends it: `compaction_update` when the client renders those, else a
/// `session_info_update` marker. `status` is `started`, `completed`,
/// `cancelled` or `failed`; `detail` is the summary or the error.
fn compaction_update(
    standard: bool,
    id: &str,
    reason: &str,
    status: &str,
    detail: Option<&str>,
) -> Value {
    if !standard {
        let mut marker = json!({"status": status, "messageId": id, "reason": reason});
        match status {
            "cancelled" => {
                marker["status"] = json!("failed");
                marker["error"] = json!({"type": "aborted", "message": "Compaction cancelled"});
            }
            "failed" => {
                marker["error"] =
                    json!({"type": "compaction.failed", "message": detail.unwrap_or_default()})
            }
            _ => {}
        }
        return json!({"sessionUpdate": "session_info_update", "_meta": {COMPACTION_META: marker}});
    }
    let mut u = json!({"sessionUpdate": "compaction_update", "compactionId": id});
    match status {
        "started" => u["status"] = json!("in_progress"),
        "completed" => {
            u["status"] = json!("completed");
            u["summary"] = match detail.filter(|t| !t.is_empty()) {
                Some(t) => json!([{"type": "text", "text": t}]),
                None => Value::Null,
            };
        }
        _ => {
            u["status"] = json!(status);
            u["summary"] = Value::Null;
            if status == "failed" {
                // OpenCode shows a multi-line cause as a plain failure.
                let msg = detail.filter(|m| !m.is_empty() && !m.contains('\n'));
                u["error"] = json!(msg.unwrap_or("Compaction failed"));
            }
        }
    }
    u
}

struct AcpSink {
    conn: Conn,
    session: String,
    cwd: PathBuf,
    current: Current,
    stats: Arc<Mutex<TurnStats>>,
    /// The client renders `compaction_update`.
    compaction: bool,
    /// The compaction running, shared with the slot for a cancel.
    compacting: Compacting,
    /// Compactions run now are the user's `/compact` (else automatic).
    manual: bool,
    /// The summary a compaction just kept, for its completion.
    summary: Option<String>,
    /// The assistant message being streamed.
    message: String,
    /// Reasoning parts started in this message, and whether one is open.
    reasoning: usize,
    in_reasoning: bool,
    /// Calls in flight: name, input and, for edits, the files before.
    tools: HashMap<String, (String, Value, Vec<Before>)>,
    /// Media of the call about to end, from `tool_media`.
    media: HashMap<String, Vec<Media>>,
    /// Calls announced `pending` while they streamed and not started yet,
    /// by the id the client knows them by.
    pending: BTreeSet<String>,
    /// The id the client knows a call by, when it differs from the call's
    /// own (the streamed id, before Gemini via Chat appended its
    /// signature).
    aliases: HashMap<String, String>,
    retrying: bool,
    /// The client takes subagent activity apart (`child_update`).
    child_updates: bool,
    /// The subagents running, innermost last, with the call that was
    /// running when each started.
    children: Vec<(Child, Option<Call>)>,
}

impl AcpSink {
    fn new(conn: Conn, session: &str, slot: &Slot, compaction: bool, child_updates: bool) -> Self {
        AcpSink {
            conn,
            session: session.to_string(),
            cwd: slot.cwd.clone(),
            current: slot.current.clone(),
            stats: slot.stats.clone(),
            compaction,
            compacting: slot.compacting.clone(),
            manual: false,
            summary: None,
            message: unique("msg"),
            reasoning: 0,
            in_reasoning: false,
            tools: HashMap::new(),
            media: HashMap::new(),
            pending: BTreeSet::new(),
            aliases: HashMap::new(),
            retrying: false,
            child_updates,
            children: Vec::new(),
        }
    }

    /// The subagent whose activity this is, if any.
    fn child(&self) -> Option<&Child> {
        self.children.last().map(|(c, _)| c)
    }

    /// Send an update of the session or of the subagent running.
    fn emit(&self, update: Value) {
        route(
            &self.conn,
            &self.session,
            self.child(),
            self.child_updates,
            update,
        );
    }

    /// Where a call's state is kept: a subagent's calls by their projected
    /// id, so they cannot meet the session's own.
    fn key(&self, id: &str) -> String {
        match self.child() {
            Some(c) => format!("{}:{id}", c.id),
            None => id.to_string(),
        }
    }

    /// A subagent's status, for clients that take child updates.
    fn child_status(&self, child: &Child, status: &str, error: Option<&str>) {
        if !self.child_updates {
            return;
        }
        let mut event = json!({"type": "status", "status": status});
        if let Some(e) = error {
            event["error"] = json!({"type": "unknown", "message": e});
        }
        self.conn.notify(
            CHILD_UPDATE_METHOD,
            child.notification(&self.session, event),
        );
    }

    /// The id the client knows call `id` by.
    fn client_id(&self, id: &str) -> String {
        self.aliases
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// A call announced while it streamed will not run: it failed.
    fn fail_pending(&mut self, id: &str) {
        if self.pending.remove(id) {
            self.conn.update(
                &self.session,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed"}),
            );
        }
    }

    /// The turn is over: calls announced while they streamed that never
    /// ran (the turn stopped first) failed.
    fn settle(&mut self) {
        for id in self.pending.clone() {
            self.fail_pending(&id);
        }
        self.aliases.clear();
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
    fn tool_input(&mut self, id: &str, name: &str) {
        self.settle_retry();
        self.in_reasoning = false;
        // OpenCode's ACP announces a call as soon as its input starts.
        let mut call = tool_call(&self.cwd, id, name, &json!({}), "pending");
        call["sessionUpdate"] = json!("tool_call");
        self.conn.update(&self.session, call);
        self.pending.insert(id.to_string());
    }
    fn tool_input_end(&mut self, id: &str, call: Option<&str>) {
        match call {
            Some(call) if call != id => {
                self.aliases.insert(call.to_string(), id.to_string());
            }
            Some(_) => {}
            None => self.fail_pending(id),
        }
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &Value) {
        self.settle_retry();
        self.in_reasoning = false;
        // A subagent's calls arrive as "agent › tool": its projection names it.
        let name = if self.child().is_some() {
            base_name(name)
        } else {
            name
        };
        let shown = self.client_id(id);
        let before = files_before(&self.cwd, name, input);
        self.tools
            .insert(self.key(id), (name.into(), input.clone(), before));
        *self.current.lock().unwrap() = Some(Call {
            id: shown.clone(),
            name: name.to_string(),
            input: input.clone(),
            child: self.child().cloned(),
        });
        // A call announced while it streamed is already known to the client.
        if self.child().is_some() || !self.pending.remove(&shown) {
            let mut call = tool_call(&self.cwd, &shown, name, input, "pending");
            call["sessionUpdate"] = json!("tool_call");
            self.emit(call);
        }
        let mut running = tool_call(&self.cwd, &shown, name, input, "in_progress");
        running["sessionUpdate"] = json!("tool_call_update");
        running.as_object_mut().unwrap().remove("name");
        self.emit(running);
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        // Within a subagent, the call that started it is running again.
        *self.current.lock().unwrap() = self.children.last().and_then(|(_, outer)| outer.clone());
        let key = self.key(id);
        let (name, input, before) = self
            .tools
            .remove(&key)
            .unwrap_or_else(|| (base_name(name).to_string(), json!({}), vec![]));
        let media = self.media.remove(&key).unwrap_or_default();
        let shown = match self.child() {
            Some(_) => id.to_string(),
            None => self.aliases.remove(id).unwrap_or_else(|| id.to_string()),
        };
        let mut update = result_update(&self.cwd, &shown, &name, &input, output, is_error, &media);
        // A patch that failed midway changed the files it wrote first.
        if !is_error || base_name(&name) == "patch" {
            for (path, old) in before {
                let new = match std::fs::read_to_string(&path) {
                    Ok(new) => new,
                    // Deleted (or moved away) by a patch.
                    Err(_) if old.is_some() && !path.exists() => String::new(),
                    Err(_) => continue,
                };
                if old.as_deref() != Some(new.as_str()) {
                    update["content"]
                        .as_array_mut()
                        .unwrap()
                        .push(diff(&path, old, new));
                }
            }
        }
        self.emit(update);
        if let Some((c, _)) = self.children.last_mut() {
            c.message = unique("msg");
        }
    }
    fn tool_media(&mut self, id: &str, media: &[Media]) {
        self.media.insert(self.key(id), media.to_vec());
    }
    fn child_start(&mut self, child: &codesmith_core::session::ChildSession) {
        let c = Child {
            id: child.id.clone(),
            parent: child.parent_id.clone(),
            depth: child.depth,
            title: child.title.clone(),
            message: unique("msg"),
        };
        self.child_status(&c, "created", None);
        self.child_status(&c, "running", None);
        let outer = self.current.lock().unwrap().clone();
        self.children.push((c, outer));
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        let Some(at) = self.children.iter().rposition(|(c, _)| c.id == id) else {
            return;
        };
        let (c, outer) = self.children.remove(at);
        *self.current.lock().unwrap() = outer;
        self.child_status(&c, status, error);
    }
    fn child_text(&mut self, delta: &str) {
        if let Some(c) = self.child() {
            self.emit(chunk("agent_message_chunk", &c.message, delta));
        }
    }
    fn child_reasoning(&mut self, delta: &str) {
        if let Some(c) = self.child() {
            let id = reasoning_id(&c.message, 0);
            self.emit(chunk("agent_thought_chunk", &id, delta));
        }
    }
    fn todos(&mut self, todos: &[Todo]) {
        self.conn.update(&self.session, plan(todos));
    }
    fn step_end(&mut self, reason: &FinishReason, usage: &Usage) {
        self.settle_retry();
        {
            let mut st = self.stats.lock().unwrap();
            st.usage.get_or_insert_with(Usage::default).add(usage);
            st.last = Some(*usage);
            st.finish = Some(reason.normalized);
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
    fn compaction_start(&mut self) {
        self.settle_retry();
        self.in_reasoning = false;
        let (id, reason) = (unique("cmp"), if self.manual { "manual" } else { "auto" });
        self.conn.update(
            &self.session,
            compaction_update(self.compaction, &id, reason, "started", None),
        );
        *self.compacting.lock().unwrap() = Some((id, reason));
    }
    fn compaction_delta(&mut self, delta: &str) {
        let Some((id, _)) = self.compacting.lock().unwrap().clone() else {
            return;
        };
        if self.compaction && !delta.is_empty() {
            self.conn.update(
                &self.session,
                json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": id, "content": {"type": "text", "text": delta}}),
            );
        }
    }
    fn compaction_end(&mut self, outcome: Result<&str, &str>) {
        match outcome {
            Ok(summary) => self.summary = Some(summary.to_string()),
            Err(e) => {
                if let Some((id, reason)) = self.compacting.lock().unwrap().take() {
                    self.conn.update(
                        &self.session,
                        compaction_update(self.compaction, &id, reason, "failed", Some(e)),
                    );
                }
            }
        }
    }
    fn compacted(&mut self, before: u64, after: u64) {
        let summary = self.summary.take();
        let Some((id, reason)) = self.compacting.lock().unwrap().take() else {
            return;
        };
        let mut update = compaction_update(
            self.compaction,
            &id,
            reason,
            "completed",
            summary.as_deref(),
        );
        if !self.compaction {
            let marker = &mut update["_meta"][COMPACTION_META];
            marker["tokensBefore"] = json!(before);
            marker["tokensAfter"] = json!(after);
        }
        self.conn.update(&self.session, update);
    }
}

/// The permission action a call of tool `name` asks for itself.
fn own_action(name: &str) -> &str {
    match base_name(name) {
        "write" | "patch" => "edit",
        n => n,
    }
}

/// The files and directories a path permission request is for, as
/// `locations`: the resources the engine checks, not the model's input.
fn request_locations(cwd: &Path, req: &Request) -> Vec<Value> {
    let paths: BTreeSet<String> = req
        .resources
        .iter()
        .filter(|_| matches!(req.action.as_str(), "read" | "edit" | "external_directory"))
        .map(|r| r.strip_suffix("/*").unwrap_or(r))
        .filter(|r| !r.is_empty() && !r.contains(['*', '?']))
        .map(|r| absolute(cwd, r).display().to_string())
        .collect();
    paths.iter().map(|p| json!({"path": p})).collect()
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
            Some(Call {
                id,
                name,
                input,
                child,
            }) => {
                let mut tc = tool_call(&self.cwd, id, name, input, "pending");
                // A request for something other than the call's own action
                // (`external_directory` for a symlink's target) names what
                // it grants itself: the model's input would hide it.
                let own = req.action == own_action(name);
                let named = file_path(input).is_some_and(|p| {
                    let p = absolute(&self.cwd, p);
                    req.resources.iter().all(|r| absolute(&self.cwd, r) == p)
                });
                match permission_title(name, input).filter(|_| own || named) {
                    Some(t) => tc["title"] = json!(t),
                    None => tc["title"] = json!(req.title),
                }
                let mut locations = if own {
                    tc["locations"].as_array().cloned().unwrap_or_default()
                } else {
                    Vec::new()
                };
                for l in request_locations(&self.cwd, req) {
                    if !locations.contains(&l) {
                        locations.push(l);
                    }
                }
                tc["locations"] = json!(locations);
                let mut content = Vec::new();
                if !own {
                    content.push(text_content(&format!(
                        "{}\n{}",
                        req.resources.join("\n"),
                        codesmith_core::permission::always_summary(req)
                    )));
                } else {
                    content.extend(previews(&self.cwd, name, input));
                }
                if !content.is_empty() {
                    tc["content"] = json!(content);
                }
                if let Some(c) = child {
                    tc["sessionUpdate"] = json!("tool_call");
                    tc = c.project(tc);
                    tc.as_object_mut().unwrap().remove("sessionUpdate");
                }
                tc
            }
            None => json!({
                "toolCallId": format!("perm_{}", req.action),
                "title": req.title,
                "kind": tool_kind(&req.action),
                "status": "pending",
                "locations": request_locations(&self.cwd, req),
            }),
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
        if let Some(call) = self.current.lock().unwrap().as_ref() {
            params["toolCallId"] = json!(call.client_id());
        }
        let rx = self
            .conn
            .request(&self.session, "elicitation/create", params);
        let response = rx.blocking_recv().ok()?.ok()?;
        question_answers(questions, &response)
    }
}

/// What holds a slot's session while a turn runs.
#[derive(Debug, PartialEq)]
enum Running {
    /// A `session/prompt`, by JSON-RPC id: answered when the turn ends.
    Prompt(Value),
    /// A turn the agent started itself, for background commands that
    /// finished while the session was idle: it has no request to answer.
    Resumed,
}

struct Slot {
    id: String,
    session: tokio::sync::Mutex<Option<Session>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Whose turn `task` runs; taken by whoever settles it (the turn
    /// ending, or a cancel), so only one of them answers.
    running: Mutex<Option<Running>>,
    /// While idle: waits for a background command to finish, to resume
    /// the session. A prompt cancels it and reports the command itself.
    waiter: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// A `session/prompt` waiting for a resumed turn to end.
    queued: Mutex<Option<Value>>,
    /// Notified when a turn gives the session back or is stopped.
    idle: tokio::sync::Notify,
    /// Closed, deleted or replaced: never resume it again.
    closed: AtomicBool,
    current: Current,
    compacting: Compacting,
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
    /// The "always" approvals given in this session, kept in force across
    /// every rebuild (cancel, mode and model switches, a reload).
    approvals: Approvals,
    /// Its background commands, given to every rebuild (cancel, mode and
    /// model switches, a reload, reopening it), so they outlive the session
    /// a stopped turn dropped. Stopped by `session/close`,
    /// `session/delete` and the end of input.
    jobs: Jobs,
    /// Finished commands a user's cancel left unreported: the session is
    /// not woken for them, only once another one finishes.
    held: AtomicUsize,
    /// Resumed turns that failed in a row, and until when the next waits.
    failures: AtomicU32,
    backoff: Mutex<Option<tokio::time::Instant>>,
}

/// How long a session waits to resume again after `failures` resumed
/// turns failed in a row: a second, doubling up to a minute.
fn resume_backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

pub type Loader = Arc<dyn Fn(&Path) -> Result<Loaded> + Send + Sync>;

/// Sessions per `session/list` page, as OpenCode lists them.
const LIST_PAGE: usize = 100;
/// The session meta key keeping the directories granted through ACP.
const DIRS_META: &str = "acp.additionalDirectories";

/// The directories granted to session `id` when it was last opened.
fn stored_directories(db: &Store, id: &str) -> Vec<String> {
    db.meta(id, DIRS_META)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

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

/// The file name at the end of a URI, when it has one.
fn uri_name(uri: &str) -> Option<String> {
    if uri.starts_with("data:") {
        return None;
    }
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    path.rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// Base64 prompt data as media when its bytes are an image, PDF or video
/// (images fitted to `limits`); `Ok(None)` for other data. Bad base64 or an image that
/// cannot be fitted is an invalid prompt.
fn prompt_media(
    data: &str,
    uri: Option<&str>,
    limits: &ImageLimits,
) -> Result<Option<Media>, RpcError> {
    let data = data.trim();
    let bytes = media::unbase64(data).ok_or_else(|| {
        invalid(
            "prompt content is not valid base64",
            json!({"field": "prompt"}),
        )
    })?;
    let Some(mime) = media::detect(&bytes) else {
        return Ok(None);
    };
    let name = uri.and_then(uri_name);
    let mut m = Media::base64(mime, data);
    if mime.starts_with("image/") {
        let shown = name.as_deref().unwrap_or("image");
        match media::normalize(mime, data, limits) {
            Ok(None) => {}
            Ok(Some((mime, data))) => m = Media::base64(mime, data),
            Err(e) => {
                return Err(invalid(
                    e.message(shown, limits),
                    json!({"field": "prompt"}),
                ))
            }
        }
    }
    Ok(Some(match name {
        Some(n) => m.named(n),
        None => m,
    }))
}

/// A link the model can follow (OpenCode's `linkReference`).
fn reference(name: &str, uri: &str) -> String {
    if name.is_empty() {
        uri.to_string()
    } else {
        format!("[{name}]({uri})")
    }
}

/// `%XX` escapes decoded (and `+` as a space in a query, `plus`).
fn percent_decode(s: &str, plus: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The local file a link names: a `file://` URL, or a Zed link with a
/// `path` (OpenCode's `resourceLinkToPart`).
fn linked_file(uri: &str) -> Option<PathBuf> {
    if uri.starts_with("file://") {
        let path = uri.split(['?', '#']).next().unwrap_or(uri);
        return crate::tui::attach::file_url(path).map(PathBuf::from);
    }
    let query = uri.strip_prefix("zed://")?.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("path="))
        .and_then(|p| percent_decode(p, true))
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
}

/// A path as a `file://` URL (OpenCode's `pathToFileURL`).
fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A prompt's content blocks, made into a message.
#[derive(Debug, Default, PartialEq)]
pub struct PromptContent {
    pub text: String,
    /// Images and PDFs given inline.
    pub media: Vec<Media>,
    /// Local files linked, to attach as the Read tool sees them (with its
    /// permission checks) when the turn starts.
    pub files: Vec<PathBuf>,
}

/// Prompt content blocks as one message and its attachments: text and
/// embedded text resources join the message, images and binary resources
/// that are images or PDFs become media (fitted to `limits`), local files
/// linked (resource links and images given only by a `file://` URI) are
/// attached when the turn starts, and other links are referenced in the
/// text.
pub fn prompt_content(blocks: &Value, limits: &ImageLimits) -> Result<PromptContent, RpcError> {
    let mut parts = Vec::new();
    let mut attached = Vec::new();
    let mut files = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or("").to_string()),
            Some("image") => {
                let data = b["data"].as_str().filter(|d| !d.is_empty());
                let uri = b["uri"].as_str().filter(|u| !u.is_empty());
                match (data, uri) {
                    (Some(d), _) => match prompt_media(d, uri, limits)? {
                        Some(m) => attached.push(m),
                        None => parts.push("[image content omitted: not a supported image]".into()),
                    },
                    (None, Some(u)) if u.starts_with("data:") => {
                        match Media::from_data_url(u).and_then(|m| m.inline().map(str::to_string)) {
                            Some(d) => match prompt_media(&d, None, limits)? {
                                Some(m) => attached.push(m),
                                None => parts
                                    .push("[image content omitted: not a supported image]".into()),
                            },
                            None => {
                                return Err(invalid(
                                    "image content has an invalid data URI",
                                    json!({"field": "prompt"}),
                                ))
                            }
                        }
                    }
                    (None, Some(u)) => match linked_file(u) {
                        Some(f) => files.push(f),
                        None => {
                            let name = uri_name(u).unwrap_or_else(|| "image".into());
                            parts.push(reference(&name, u));
                        }
                    },
                    (None, None) => {
                        return Err(invalid(
                            "image content has no data or uri",
                            json!({"field": "prompt"}),
                        ))
                    }
                }
            }
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                match (r["text"].as_str(), r["blob"].as_str()) {
                    (Some(t), _) => parts.push(format!("<file uri=\"{uri}\">\n{t}\n</file>")),
                    (None, Some(blob)) => match prompt_media(blob, Some(uri), limits)? {
                        Some(m) => attached.push(m),
                        None => parts.push(format!("[attached resource {uri} omitted: binary]")),
                    },
                    (None, None) => {
                        parts.push(format!("[attached resource {uri} omitted: binary]"))
                    }
                }
            }
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or("");
                match linked_file(uri) {
                    Some(f) => files.push(f),
                    None => parts.push(reference(b["name"].as_str().unwrap_or(""), uri)),
                }
            }
            Some(other) => parts.push(format!("[{other} content omitted: not supported]")),
            None => {}
        }
    }
    Ok(PromptContent {
        text: parts.join("\n\n"),
        media: attached,
        files,
    })
}

/// Attach the files a prompt linked, as the TUI attaches `@` mentions:
/// each goes through `read`'s permission checks (and `external_directory`
/// outside the project), then joins the message as the Read tool's output,
/// or as media for images and PDFs. A file that cannot be attached says
/// why instead.
fn attach_files(s: &Session, prompt: &mut PromptContent) {
    for f in std::mem::take(&mut prompt.files) {
        let shown = f.display().to_string();
        let attached = s.attach(&shown);
        let note = crate::tui::attach::attachment(&f, &shown, attached, &mut prompt.media);
        prompt.text.push_str(&note);
    }
}

/// A stored prompt split into what the user typed and the files attached
/// to it (each by the path the Read tool was called with).
fn prompt_attachments(text: &str) -> (&str, Vec<String>) {
    let mut pieces = text.split(crate::tui::attach::MARKER);
    let typed = pieces.next().unwrap_or("");
    let files = pieces
        .filter_map(|p| {
            let head = p.lines().next()?;
            let v: Value = serde_json::from_str(head).ok()?;
            v["filePath"].as_str().map(str::to_string)
        })
        .collect();
    (typed, files)
}

/// Media as ACP content: images as `image`, anything else as an embedded
/// binary resource. Remote media is skipped.
fn media_content(m: &Media) -> Option<Value> {
    let data = m.inline()?;
    let uri = m.filename.as_ref().map(|n| format!("file:///{n}"));
    Some(if m.kind() == codesmith_ai::MediaKind::Image {
        let mut v = json!({"type": "image", "mimeType": m.mime, "data": data});
        if let Some(u) = uri {
            v["uri"] = json!(u);
        }
        v
    } else {
        json!({"type": "resource", "resource": {"uri": uri.unwrap_or_else(|| "file:///file".into()), "mimeType": m.mime, "blob": data}})
    })
}

/// A tool result's media as `tool_call_update` content: images as image
/// content, as OpenCode sends them, and other media (PDFs) as embedded
/// binary resources, which ACP tool content also carries (OpenCode drops
/// them).
fn tool_media_content(media: &[Media]) -> Vec<Value> {
    media
        .iter()
        .filter_map(media_content)
        .map(|c| json!({"type": "content", "content": c}))
        .collect()
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
/// model listed under a provider, and the session's own (all without their
/// `#variant`).
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
        .map(String::as_str)
        .chain(std::iter::once(current))
        .map(|s| config::split_variant(s).0.to_string())
        .collect::<Vec<_>>();
    for spec in extra {
        specs.entry(spec.clone()).or_insert(spec);
    }
    specs.into_iter().collect()
}

/// The effort value that selects no variant (OpenCode's
/// `DEFAULT_VARIANT_VALUE`).
const DEFAULT_VARIANT: &str = "default";

/// The variants a `provider/model` offers (none when it does not resolve).
fn variants_of(config: &config::Config, base: &str) -> Vec<String> {
    config
        .resolve_model(base)
        .map(|m| m.variants)
        .unwrap_or_default()
}

/// `very-high` → `Very High`.
fn variant_label(v: &str) -> String {
    v.split(['-', '_'])
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The effort selector's current value (OpenCode's `selectVariant`).
fn current_variant(variant: Option<&str>, variants: &[String]) -> String {
    match variant {
        None | Some(DEFAULT_VARIANT) => DEFAULT_VARIANT.into(),
        Some(v) if variants.iter().any(|x| x == v) => v.into(),
        _ if variants.iter().any(|x| x == DEFAULT_VARIANT) => DEFAULT_VARIANT.into(),
        _ => variants
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_VARIANT.into()),
    }
}

/// `configOptions`: the model selector, an effort selector when the model
/// has variants, and the mode selector (OpenCode's `configOptions`). `model`
/// is the session's `provider/model[#variant]`.
fn config_options(config: &config::Config, model: &str, mode: &str) -> Value {
    let (base, variant) = config::split_variant(model);
    let models: Vec<Value> = models(config, base)
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
    let mut out = vec![
        json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": base, "options": models}),
    ];
    let variants = variants_of(config, base);
    if !variants.is_empty() {
        let mut values = variants.clone();
        if !values.iter().any(|v| v == DEFAULT_VARIANT) {
            values.push(DEFAULT_VARIANT.into());
        }
        let options: Vec<Value> = values
            .iter()
            .map(|v| json!({"value": v, "name": variant_label(v)}))
            .collect();
        out.push(json!({
            "id": "effort", "name": "Effort", "description": "Available effort levels for this model",
            "category": "thought_level", "type": "select",
            "currentValue": current_variant(variant, &variants), "options": options,
        }));
    }
    out.push(json!({"id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": mode, "options": modes}));
    Value::Array(out)
}

fn effort_not_found(value: &str) -> RpcError {
    invalid(
        format!("effort not found: {value}"),
        json!({"effort": value}),
    )
}

/// The spec a `model` selection picks (OpenCode's `parseModelSelection` and
/// `requireModel`): `provider/model`, `provider/model#variant`, or
/// `provider/model/variant` when no listed model has that whole id. Without
/// a variant, the current one is kept if the model is the same.
fn model_selection(
    config: &config::Config,
    value: &str,
    current: &str,
) -> Result<String, RpcError> {
    let listed = models(config, current);
    let is_listed = |spec: &str| listed.iter().any(|(v, _)| v == spec);
    let (base, variant) = match config::split_variant(value) {
        (base, Some(v)) => (base, Some(v)),
        _ if is_listed(value) => (value, None),
        _ => match value.rsplit_once('/') {
            Some((base, v))
                if is_listed(base) && variants_of(config, base).iter().any(|x| x == v) =>
            {
                (base, Some(v))
            }
            _ => (value, None),
        },
    };
    config.resolve_model(base).map_err(|_| {
        invalid(
            format!("model not found: {value}"),
            json!({"providerId": value.split('/').next(), "modelId": value}),
        )
    })?;
    let variants = variants_of(config, base);
    let variant = match variant {
        Some(DEFAULT_VARIANT) => None,
        Some(v) if variants.iter().any(|x| x == v) => Some(v),
        Some(v) => return Err(effort_not_found(v)),
        None => match config::split_variant(current) {
            (b, Some(v)) if b == base && variants.iter().any(|x| x == v) => Some(v),
            _ => None,
        },
    };
    Ok(match variant {
        Some(v) => format!("{base}#{v}"),
        None => base.to_string(),
    })
}

fn is_prompt(t: &str) -> bool {
    let s = t.trim_start();
    !["<shell ", "<system-reminder>", "<conversation-checkpoint>"]
        .iter()
        .any(|p| s.starts_with(p))
        && !codesmith_core::runner::is_engine_message(t)
}

/// A replayed history item: a message or the checkpoint of a compaction.
enum Item {
    /// Its id, sequence number (`None`: unstored) and the message.
    Message(String, Option<i64>, Message),
    Checkpoint(String, String),
}

impl Agent {
    fn store(&self) -> Option<Store> {
        Store::open(&self.db).ok()
    }

    /// The client takes subagent activity apart (OpenCode's
    /// `opencode/child-session-updates` capability).
    fn child_updates(&self) -> bool {
        self.client.lock().unwrap()["_meta"][CHILD_UPDATES] == json!(true)
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
        s.set_id(slot.id.clone());
        s.share_approvals(slot.approvals.clone());
        s.adopt_jobs(slot.jobs.clone());
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
        (approvals, jobs): (Approvals, Jobs),
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
            running: Mutex::new(None),
            waiter: Mutex::new(None),
            queued: Mutex::new(None),
            idle: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
            current: Default::default(),
            compacting: Default::default(),
            stats: Default::default(),
            loaded,
            cwd: cwd.to_path_buf(),
            mcp,
            mode: Mutex::new(None),
            model: Mutex::new(None),
            stale: AtomicBool::new(false),
            shown: Default::default(),
            approvals,
            jobs,
            held: AtomicUsize::new(0),
            failures: AtomicU32::new(0),
            backoff: Mutex::new(None),
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
        // What edit and write calls changed, by call id and sequence number.
        let changes: Vec<(i64, &str, Value)> = entries
            .iter()
            .filter(|e| !hidden.iter().any(|(f, t)| e.seq >= *f && e.seq < *t))
            .filter_map(|e| match &e.record {
                Record::Change {
                    call,
                    path,
                    old,
                    new,
                } => Some((e.seq, call.as_str(), diff(path, old.clone(), new.clone()))),
                _ => None,
            })
            .collect();
        let items: Vec<Item> = if entries.is_empty() {
            s.messages
                .iter()
                .enumerate()
                .map(|(i, m)| Item::Message(format!("{id}:{i}"), None, m.clone()))
                .collect()
        } else {
            entries
                .iter()
                .filter(|e| !hidden.iter().any(|(f, t)| e.seq >= *f && e.seq < *t))
                .filter_map(|e| match &e.record {
                    Record::Message { message } => Some(Item::Message(
                        format!("{id}:{}", e.seq),
                        Some(e.seq),
                        message.clone(),
                    )),
                    Record::Checkpoint { summary, .. } => {
                        Some(Item::Checkpoint(format!("{id}:{}", e.seq), summary.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        // Each call's result is the first one with its id after it (ids can
        // repeat across turns).
        let results: Vec<(usize, &str, &str, bool, &[Media])> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                Item::Message(_, _, m) => Some((i, m)),
                _ => None,
            })
            .flat_map(|(i, m)| m.parts.iter().map(move |p| (i, p)))
            .filter_map(|(i, p)| match p {
                Part::ToolResult {
                    id,
                    content,
                    is_error,
                    media,
                    ..
                } => Some((
                    i,
                    id.as_str(),
                    content.as_str(),
                    *is_error,
                    media.as_slice(),
                )),
                _ => None,
            })
            .collect();
        let compaction = self.client_has("/session/compaction");
        for (index, item) in items.iter().enumerate() {
            let (mid, seq, m) = match item {
                Item::Checkpoint(cid, summary) => {
                    let update = if compaction {
                        json!({"sessionUpdate": "compaction_update", "compactionId": cid, "status": "completed", "summary": [{"type": "text", "text": summary}]})
                    } else {
                        json!({"sessionUpdate": "session_info_update", "_meta": {COMPACTION_META: {"status": "completed", "messageId": cid}}})
                    };
                    self.conn.update(id, update);
                    continue;
                }
                Item::Message(mid, seq, m) => (mid, *seq, m),
            };
            let mut reasoning = 0;
            for p in &m.parts {
                match (&m.role, p) {
                    // The engine's own notes are not the user's prompts.
                    (Role::System, _) => {}
                    (Role::User, Part::Text(t)) if is_prompt(t) => {
                        // Attached files replay as the links they came as.
                        let (typed, files) = prompt_attachments(t);
                        if !typed.is_empty() || files.is_empty() {
                            self.conn
                                .update(id, chunk("user_message_chunk", mid, typed));
                        }
                        for f in files {
                            let name = uri_name(&f).unwrap_or_else(|| "file".into());
                            self.conn.update(
                                id,
                                json!({"sessionUpdate": "user_message_chunk", "messageId": mid, "content": {"type": "resource_link", "uri": file_uri(&f), "name": name}}),
                            );
                        }
                    }
                    (Role::User, Part::Media(media)) => {
                        if let Some(content) = media_content(media) {
                            self.conn.update(
                                id,
                                json!({"sessionUpdate": "user_message_chunk", "messageId": mid, "content": content}),
                            )
                        }
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
                            ..
                        },
                    ) => {
                        let mut call = tool_call(&s.cwd, cid, name, input, "pending");
                        call["sessionUpdate"] = json!("tool_call");
                        self.conn.update(id, call);
                        let result = results.iter().find(|(i, rid, ..)| *i > index && rid == cid);
                        if let Some((_, _, output, failed, media)) = result {
                            let mut update =
                                result_update(&s.cwd, cid, name, input, output, *failed, media);
                            // The files the call changed, as it streamed live:
                            // the run of changes recorded for it.
                            let first = changes
                                .iter()
                                .position(|(at, call, _)| Some(*at) > seq && call == cid);
                            let patch = base_name(name) == "patch";
                            if let (true, Some(first)) = (!*failed || patch, first) {
                                let content = update["content"].as_array_mut().unwrap();
                                for (_, _, d) in changes[first..]
                                    .iter()
                                    .take_while(|(_, call, _)| call == cid)
                                {
                                    content.push(d.clone());
                                }
                            }
                            self.conn.update(id, update);
                        }
                    }
                    // A tool the provider ran: already settled.
                    (Role::Assistant, Part::Hosted(h)) => {
                        let mut call = tool_call(&s.cwd, &h.id, &h.name, &h.input, "pending");
                        call["sessionUpdate"] = json!("tool_call");
                        self.conn.update(id, call);
                        let (output, failed, media) = match &h.result {
                            Some(r) => (h.summary().unwrap_or_default(), r.is_error, &r.media[..]),
                            None => ("Tool result missing".to_string(), true, &[][..]),
                        };
                        self.conn.update(
                            id,
                            result_update(&s.cwd, &h.id, &h.name, &h.input, &output, failed, media),
                        );
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
                        "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                        "mcpCapabilities": {"http": true, "sse": false},
                        "sessionCapabilities": {"additionalDirectories": {}, "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {}},
                        "_meta": {CHILD_UPDATES: true},
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
                    self.end(&sid, &slot).await;
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
                    self.end(&sid, &slot).await;
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

    /// `session/list`: the newest sessions first, a page at a time (as
    /// OpenCode pages them), each with the directories granted to it.
    fn list(&self, p: &Value) -> Result<Value, RpcError> {
        let cwd = match &p["cwd"] {
            Value::Null => None,
            v => Some(parse_cwd(v)?.display().to_string()),
        };
        // The cursor is the last session of the previous page.
        let after = match &p["cursor"] {
            Value::Null => None,
            v => Some(
                v.as_str()
                    .and_then(|c| c.split_once(':'))
                    .and_then(|(t, id)| Some((t.parse::<i64>().ok()?, id.to_string())))
                    .ok_or_else(|| invalid("invalid cursor", json!({"field": "cursor"})))?,
            ),
        };
        let db = self.store();
        let mut list = db
            .as_ref()
            .map(|db| db.list(cwd.as_deref(), false).unwrap_or_default())
            .unwrap_or_default();
        list.sort_by(|a, b| (b.updated, &b.id).cmp(&(a.updated, &a.id)));
        let start = after.map_or(0, |(t, id)| {
            list.iter()
                .position(|i| (i.updated, &i.id) < (t, &id))
                .unwrap_or(list.len())
        });
        let page = &list[start..(start + LIST_PAGE).min(list.len())];
        let sessions: Vec<Value> = page
            .iter()
            .map(|i| {
                let mut v = json!({"sessionId": i.id, "cwd": i.directory, "title": i.title, "updatedAt": chrono::DateTime::from_timestamp_millis(i.updated).map(|t| t.to_rfc3339())});
                let dirs = db.as_ref().map(|db| stored_directories(db, &i.id)).unwrap_or_default();
                if !dirs.is_empty() {
                    v["additionalDirectories"] = json!(dirs);
                }
                v
            })
            .collect();
        let mut out = json!({"sessions": sessions});
        if start + LIST_PAGE < list.len() {
            let last = page.last().unwrap();
            out["nextCursor"] = json!(format!("{}:{}", last.updated, last.id));
        }
        Ok(out)
    }

    async fn open_session(self: &Arc<Self>, method: &str, p: &Value) -> Result<Value, RpcError> {
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
        // The directories asked for replace those granted before (none:
        // there are none now), as OpenCode keeps them with the session.
        if let Some(db) = self.store() {
            let value = (!dirs.is_empty()).then(|| json!(dirs).to_string());
            if let Err(e) = db.set_meta(&sid, DIRS_META, value.as_deref()) {
                eprintln!("acp: could not keep the directories of {sid}: {e:#}");
            }
        }
        // Reopening a session that is open keeps its approvals and its
        // background commands; a new or forked one starts without any.
        let kept = match method {
            "session/load" | "session/resume" => self
                .sessions
                .lock()
                .await
                .get(&sid)
                .map(|s| (s.approvals.clone(), s.jobs.clone())),
            _ => None,
        }
        .unwrap_or_default();
        let slot = self
            .open(&sid, &cwd, servers, &dirs, kept)
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
            // Wait for the commands the new slot took over (after the
            // response: the writer keeps order).
            let (me, slot) = (self.clone(), slot.clone());
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                me.wake(slot).await;
            });
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

    /// Change the model, effort or mode; returns the new `configOptions`.
    /// A change while a turn runs applies once it ends.
    async fn select(&self, sid: &str, config_id: &str, value: &str) -> Result<Value, RpcError> {
        let slot = self.slot(sid).await?;
        let config = &slot.loaded.config;
        let current = slot
            .model
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| slot.shown.lock().unwrap().0.clone());
        let (target, value) = match config_id {
            "model" => (&slot.model, model_selection(config, value, &current)?),
            "effort" => {
                let base = config::split_variant(&current).0;
                let spec = if value == DEFAULT_VARIANT {
                    base.to_string()
                } else if variants_of(config, base).iter().any(|v| v == value) {
                    format!("{base}#{value}")
                } else {
                    return Err(effort_not_found(value));
                };
                (&slot.model, spec)
            }
            "mode" => {
                agent::resolve(config, value)
                    .filter(|a| a.mode != "subagent")
                    .ok_or_else(|| {
                        invalid(format!("mode not found: {value}"), json!({"mode": value}))
                    })?;
                (&slot.mode, value.to_string())
            }
            _ => {
                return Err(invalid(
                    format!("unknown config option: {config_id}"),
                    json!({"configId": config_id}),
                ))
            }
        };
        let previous = target.lock().unwrap().replace(value.clone());
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
        self.conn.update(
            sid,
            json!({"sessionUpdate": "config_option_update", "configOptions": options}),
        );
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
        let mut content = prompt_content(&p["prompt"], &slot.loaded.config.media.image)?;
        let Some(mut s) = self.acquire(&slot, &rpc).await? else {
            // Cancelled while queued: the cancel answered.
            return Ok(());
        };
        *slot.stats.lock().unwrap() = TurnStats::default();
        let compaction = self.client_has("/session/compaction");
        let child_updates = self.child_updates();
        let (me, slot2) = (self.clone(), slot.clone());
        let task = tokio::spawn(async move {
            let mut sink = AcpSink::new(me.conn.clone(), &sid, &slot2, compaction, child_updates);
            let title_before = s.title.clone();
            attach_files(&s, &mut content);
            let result = me
                .run_turn(&mut s, &slot2, &content.text, content.media, &mut sink)
                .await;
            sink.settle();
            #[cfg(test)]
            tests::settle_pause(&sid).await;
            // The request is taken under the session lock, and the session
            // put back before it is released: a cancel that finds the
            // request taken knows the turn is settled. A cancel that took
            // it first answers, and reloads the session.
            let mut g = slot2.session.lock().await;
            let rpc = match slot2.running.lock().unwrap().take() {
                Some(Running::Prompt(rpc)) => rpc,
                _ => return,
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
                    Ok(fresh) => *g = Some(fresh),
                    Err(e) => eprintln!("acp: could not apply the new selection: {e:#}"),
                }
            } else {
                *g = Some(s);
            }
            drop(g);
            slot2.task.lock().unwrap().take();
            // Answered before a resumed turn can start streaming.
            let response = |reason: &str| {
                let mut r = json!({"stopReason": reason});
                if let Some(u) = &stats.usage {
                    r["usage"] = usage_json(u);
                }
                me.conn.respond(&rpc, r)
            };
            match result {
                Ok(()) => response(stop_reason(stats.finish)),
                Err(e)
                    if e.to_string()
                        .contains(codesmith_core::runner::OUTPUT_LIMIT_EXHAUSTED) =>
                {
                    response("max_tokens")
                }
                Err(e)
                    if e.to_string()
                        .contains(codesmith_core::runner::CONTENT_FILTERED) =>
                {
                    response("refusal")
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
            me.clone().wake(slot2).await;
        });
        *slot.task.lock().unwrap() = Some(task);
        Ok(())
    }

    /// Take the session for the prompt `rpc`. A prompt while the session
    /// waits for a background command cancels the wait (its turn reports
    /// the command); one while a resumed turn runs waits for that turn to
    /// end, then runs (OpenCode steers it into the running turn). `None`:
    /// cancelled while waiting, and already answered.
    async fn acquire(&self, slot: &Slot, rpc: &Value) -> Result<Option<Session>, RpcError> {
        if let Some(w) = slot.waiter.lock().unwrap().take() {
            w.abort();
        }
        let busy = || {
            internal(
                format!("Session already has an active ACP prompt: {}", slot.id),
                json!({"service": "session"}),
            )
        };
        let mut waited = false;
        loop {
            let idle = slot.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            {
                let mut g = slot.session.lock().await;
                let mut queued = slot.queued.lock().unwrap();
                let ours = queued.as_ref() == Some(rpc);
                if waited && !ours {
                    return Ok(None);
                }
                if queued.is_some() && !ours {
                    return Err(busy());
                }
                if let Some(s) = g.take() {
                    *queued = None;
                    // Its turn reports what finished.
                    slot.held.store(0, Ordering::SeqCst);
                    *slot.running.lock().unwrap() = Some(Running::Prompt(rpc.clone()));
                    return Ok(Some(s));
                }
                if *slot.running.lock().unwrap() != Some(Running::Resumed) {
                    return Err(busy());
                }
                *queued = Some(rpc.clone());
                waited = true;
            }
            idle.await;
        }
    }

    /// The session is idle: if one of its background commands finished,
    /// resume it as a turn of its own, streamed as `session/update`s with
    /// no request to answer (OpenCode wakes an idle session with each
    /// finished command's notice); if one still runs, wait for it and come
    /// back here.
    fn wake(self: Arc<Self>, slot: Arc<Slot>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let mut g = slot.session.lock().await;
            if slot.closed.load(Ordering::SeqCst) || slot.queued.lock().unwrap().is_some() {
                return;
            }
            let Some(s) = g.as_ref() else { return };
            let held = slot.held.load(Ordering::SeqCst);
            let backoff = slot
                .backoff
                .lock()
                .unwrap()
                .filter(|t| *t > tokio::time::Instant::now());
            if let (true, Some(until)) = (s.background_finished() > held, backoff) {
                // The last resumed turn failed: try again later.
                let slot2 = slot.clone();
                let waiter = tokio::spawn(async move {
                    tokio::time::sleep_until(until).await;
                    slot2.waiter.lock().unwrap().take();
                    self.wake(slot2).await;
                });
                if let Some(old) = slot.waiter.lock().unwrap().replace(waiter) {
                    old.abort();
                }
            } else if s.background_finished() > held {
                let s = g.take().unwrap();
                *slot.running.lock().unwrap() = Some(Running::Resumed);
                let task = tokio::spawn(self.clone().resumed(slot.clone(), s));
                *slot.task.lock().unwrap() = Some(task);
            } else if s.background_running() > 0 {
                let (reaper, slot2) = (s.reaper(), slot.clone());
                let waiter = tokio::spawn(async move {
                    reaper.wait_more(held).await;
                    // From here a prompt no longer cancels the wake: it
                    // waits for the turn this may start.
                    slot2.waiter.lock().unwrap().take();
                    self.wake(slot2).await;
                });
                if let Some(old) = slot.waiter.lock().unwrap().replace(waiter) {
                    old.abort();
                }
            }
        })
    }

    /// Run the turn that reports `s`'s finished background commands, then
    /// give the session back and wake it again for those still running.
    async fn resumed(self: Arc<Self>, slot: Arc<Slot>, mut s: Session) {
        *slot.stats.lock().unwrap() = TurnStats::default();
        let compaction = self.client_has("/session/compaction");
        let mut sink = AcpSink::new(
            self.conn.clone(),
            &slot.id,
            &slot,
            compaction,
            self.child_updates(),
        );
        let title_before = s.title.clone();
        slot.held.store(0, Ordering::SeqCst);
        #[cfg(test)]
        let resumed = match tests::resume_hook(&slot.id).await {
            Some(e) => Err(e),
            None => s.resume_background(&mut sink).await,
        };
        #[cfg(not(test))]
        let resumed = s.resume_background(&mut sink).await;
        match resumed {
            Ok(()) => {
                slot.failures.store(0, Ordering::SeqCst);
                *slot.backoff.lock().unwrap() = None;
            }
            Err(e) => {
                eprintln!("acp: resuming session {}: {e:#}", slot.id);
                let n = slot.failures.fetch_add(1, Ordering::SeqCst) + 1;
                *slot.backoff.lock().unwrap() =
                    Some(tokio::time::Instant::now() + resume_backoff(n));
            }
        }
        sink.settle();
        {
            let mut g = slot.session.lock().await;
            // A cancel took the turn: it reloads the session.
            if slot.running.lock().unwrap().take() != Some(Running::Resumed) {
                return;
            }
            if s.title != title_before {
                self.conn.update(
                    &slot.id,
                    json!({"sessionUpdate": "session_info_update", "title": s.title}),
                );
            }
            let stats = std::mem::take(&mut *slot.stats.lock().unwrap());
            self.usage_update(&slot.id, &s, &slot, &stats);
            if slot.stale.swap(false, Ordering::SeqCst) {
                drop(s);
                match self.build(&slot) {
                    Ok(fresh) => *g = Some(fresh),
                    Err(e) => eprintln!("acp: could not apply the new selection: {e:#}"),
                }
            } else {
                *g = Some(s);
            }
            slot.task.lock().unwrap().take();
        }
        slot.idle.notify_waiters();
        self.wake(slot).await;
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
        // What the providers billed, plus the unbilled steps' tokens at the
        // configured price.
        let amount = s
            .spend()
            .cost(slot.loaded.config.cost(&s.model_spec).as_ref());
        if let Some(amount) = amount {
            update["cost"] = json!({"amount": amount, "currency": "USD"});
        }
        self.conn.update(sid, update);
    }

    async fn run_turn(
        &self,
        s: &mut Session,
        slot: &Slot,
        text: &str,
        attached: Vec<Media>,
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
                return s.prompt_with(&text, attached, sink).await;
            }
            if name == "compact" {
                sink.manual = true;
                let compacted = s.compact(sink).await;
                sink.manual = false;
                if let Err(e) = compacted {
                    sink.text(&format!("{e:#}"));
                }
                return Ok(());
            }
        }
        s.prompt_with(text, attached, sink).await
    }

    /// Stop a running turn, a prompt or a resumed one, and answer its
    /// prompt (and one queued behind a resumed turn) with `cancelled`. The
    /// turn is taken first, so one finishing meanwhile cannot answer or put
    /// its session back; with `reload` the session reloads from the store.
    /// Without it (close, delete, replacement) the slot is done: its wait
    /// for background commands stops and it is never resumed again. The
    /// commands themselves keep running (a replacement took them over;
    /// [`Agent::end`] stops them). A cancel leaves an idle session's wait
    /// alone, as OpenCode's does, and the commands survive it.
    async fn stop(&self, sid: &str, slot: &Slot, reload: bool) {
        if !reload {
            slot.closed.store(true, Ordering::SeqCst);
            if let Some(w) = slot.waiter.lock().unwrap().take() {
                w.abort();
            }
        }
        let queued = slot.queued.lock().unwrap().take();
        // A wake holds the session while it starts a turn: let it finish,
        // so that turn is the one stopped here.
        drop(slot.session.lock().await);
        let task = slot.task.lock().unwrap().take();
        if let Some(task) = task {
            let running = slot.running.lock().unwrap().take();
            task.abort();
            // Unblocks a tool waiting on the client; the abort lands at its next await.
            self.conn.drop_pending(sid);
            let _ = task.await;
            // `None`: the turn had already ended (and answered).
            if let Some(running) = running {
                self.cancelled(sid, slot, running, reload).await;
            }
        }
        slot.idle.notify_waiters();
        // Last, so a client acting on it finds the session reloaded.
        if let Some(rpc) = queued {
            self.conn.respond(&rpc, json!({"stopReason": "cancelled"}));
        }
    }

    /// After a user's cancel: what finished and is unreported does not wake
    /// the session again, only another command finishing does (the next
    /// turn still reports them all).
    async fn hold_finished(&self, slot: &Slot) {
        if let Some(s) = slot.session.lock().await.as_ref() {
            slot.held.store(s.background_finished(), Ordering::SeqCst);
        }
    }

    /// The session is over (closed, deleted, or the input ended): stop its
    /// turn and its background commands.
    async fn end(&self, sid: &str, slot: &Slot) {
        self.stop(sid, slot, false).await;
        slot.jobs.stop_all().await;
    }

    /// Settle a stopped turn: with `reload` the session reloads, then its
    /// call in flight fails and its prompt is answered (a resumed turn's
    /// failed call is the last the client hears of it).
    async fn cancelled(&self, sid: &str, slot: &Slot, running: Running, reload: bool) {
        if reload {
            slot.stale.store(false, Ordering::SeqCst);
            match self.build(slot) {
                Ok(s) => *slot.session.lock().await = Some(s),
                Err(e) => eprintln!("acp: could not reload session {sid}: {e:#}"),
            }
        }
        if let Some(call) = slot.current.lock().unwrap().take() {
            let update = result_update(
                &slot.cwd,
                &call.id,
                &call.name,
                &call.input,
                "Cancelled",
                true,
                &[],
            );
            route(
                &self.conn,
                sid,
                call.child.as_ref(),
                self.child_updates(),
                update,
            );
        }
        if let Some((id, reason)) = slot.compacting.lock().unwrap().take() {
            let standard = self.client_has("/session/compaction");
            self.conn.update(
                sid,
                compaction_update(standard, &id, reason, "cancelled", None),
            );
        }
        let stats = std::mem::take(&mut *slot.stats.lock().unwrap());
        if let Running::Prompt(rpc) = running {
            let mut response = json!({"stopReason": "cancelled"});
            if let Some(u) = &stats.usage {
                response["usage"] = usage_json(u);
            }
            self.conn.respond(&rpc, response);
        }
    }

    async fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "session/cancel" => {
                let sid = p["sessionId"].as_str().unwrap_or("").to_string();
                if let Ok(slot) = self.slot(&sid).await {
                    self.stop(&sid, &slot, true).await;
                    // Its background commands survived: wait for them.
                    self.hold_finished(&slot).await;
                    self.clone().wake(slot).await;
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
                let id = &p["requestId"];
                for (sid, slot) in slots {
                    // A prompt queued behind a resumed turn just stops waiting.
                    let queued = {
                        let mut q = slot.queued.lock().unwrap();
                        q.take_if(|q| q == id)
                    };
                    if let Some(rpc) = queued {
                        self.conn.respond(&rpc, json!({"stopReason": "cancelled"}));
                        slot.idle.notify_waiters();
                        continue;
                    }
                    let ours = matches!(slot.running.lock().unwrap().as_ref(), Some(Running::Prompt(r)) if r == id);
                    if ours {
                        self.stop(&sid, &slot, true).await;
                        self.hold_finished(&slot).await;
                        self.clone().wake(slot).await;
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
        agent.end(&sid, &slot).await;
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
/// - audio prompt content;
/// - background subagents, and turns woken by them after `end_turn`:
///   subagents run inside the parent's turn (child updates and their
///   projection into the parent are ported; turns woken by background
///   shells are too);
/// - `_session/steering`: a prompt during a woken turn waits for it
///   instead of steering into it;
/// - terminal auth (`--login`) and the auth-required mapping from a server
///   event (provider HTTP 401/403 errors map to -32000 instead);
/// - full-file diffs on replay for files over 256 KiB (before and after
///   together), and for edits made before CodeSmith recorded them: those
///   replay their output only;
/// - live and replayed `messageId`s and compaction ids differ (replay ids
///   come from the log), and a step's reasoning replays as one part
///   (`:reasoning:0`), since CodeSmith stores it that way; a subagent's
///   activity is not replayed (it lives in the child session), and its
///   retry markers go to the session's own stream;
/// - permissions settled elsewhere (no other client shares a session's
///   asks), and the `patch` tool (CodeSmith has none);
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

    /// Sessions whose prompt turns stop between ending and settling, so a
    /// test can cancel in that gap.
    static PAUSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Sessions whose resumed turns fail at once, and those whose resumed
    /// turns stop before they report; every resumed turn, by session.
    static FAIL_RESUME: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static PARK_RESUME: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static RESUMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    pub(super) async fn resume_hook(sid: &str) -> Option<anyhow::Error> {
        RESUMES.lock().unwrap().push(sid.to_string());
        if FAIL_RESUME.lock().unwrap().iter().any(|s| s == sid) {
            return Some(anyhow!("resume failed"));
        }
        if PARK_RESUME.lock().unwrap().iter().any(|s| s == sid) {
            std::future::pending::<()>().await;
        }
        None
    }

    fn resumes(sid: &str) -> usize {
        RESUMES.lock().unwrap().iter().filter(|s| *s == sid).count()
    }

    // A resumed turn that fails is retried after a back-off (a second,
    // doubling), not at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_resume_backs_off() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        FAIL_RESUME.lock().unwrap().push(sid.clone());
        launch(&mut c, &sid, "sleep 0.5; echo done-fail").await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let n = resumes(&sid);
        assert!((1..=3).contains(&n), "{n} resumes in 2.5s");
        assert_eq!(resume_backoff(1), Duration::from_secs(1));
        assert_eq!(resume_backoff(3), Duration::from_secs(4));
        assert_eq!(resume_backoff(30), Duration::from_secs(60));
        // Once it works again, it reports the command.
        FAIL_RESUME.lock().unwrap().retain(|s| *s != sid);
        let (m, _) = c
            .wait(|m| chunk_text(m).is_some_and(|t| t.contains("woke")))
            .await;
        assert_eq!(chunk_text(&m), Some("woke: done-fail"));
    }

    // A user's cancel of a resumed turn that had not reported its commands
    // yet does not wake the session again for them: only another command
    // finishing does, and that turn reports both.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_resume_waits_for_another_command() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        PARK_RESUME.lock().unwrap().push(sid.clone());
        launch(&mut c, &sid, "sleep 0.5; echo done-held").await;
        for _ in 0..100 {
            if resumes(&sid) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(resumes(&sid), 1);
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(resumes(&sid), 1, "not woken again for the same command");
        PARK_RESUME.lock().unwrap().retain(|s| *s != sid);
        launch(&mut c, &sid, "sleep 1; echo done-later").await;
        let (m, _) = c
            .wait(|m| chunk_text(m).is_some_and(|t| t.contains("woke")))
            .await;
        assert_eq!(resumes(&sid), 2);
        assert!(chunk_text(&m).unwrap().contains("done-"), "{m}");
    }

    pub(super) async fn settle_pause(sid: &str) {
        if PAUSED.lock().unwrap().iter().any(|s| s == sid) {
            std::future::pending::<()>().await;
        }
    }
    /// The background command notices the model answered, by marker.
    static WOKE: Mutex<Vec<String>> = Mutex::new(Vec::new());
    const MARKERS: [&str; 11] = [
        "done-fail",
        "done-held",
        "done-later",
        "done-wake",
        "done-prompt",
        "done-cancel",
        "done-queue",
        "done-close",
        "done-keep",
        "done-reopen",
        "done-delete",
    ];
    /// Requests whose prompt carried attachments.
    static MEDIA_REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    /// Requests for prompts that link files.
    static LINKED: Mutex<Vec<Value>> = Mutex::new(Vec::new());

    /// A scripted model: the latest user message picks the script, and the
    /// tool results since it pick the step.
    async fn fake_model() -> String {
        async fn chat(Json(b): Json<Value>) -> axum::response::Response {
            let msgs = b["messages"].as_array().unwrap();
            if b.get("tools").is_none() {
                let system = msgs[0]["content"].as_str().unwrap_or("");
                return sse(if system.contains("summaries") {
                    vec![
                        json!({"choices": [{"delta": {"content": "## Objective\n"}}]}),
                        json!({"choices": [{"delta": {"content": "Ship it."}, "finish_reason": "stop"}]}),
                    ]
                } else {
                    reply("A title")
                });
            }
            // The engine's continuation nudge continues the same prompt, as
            // do the images a tool returned (a user message of images only).
            let at = msgs
                .iter()
                .rposition(|m| {
                    m["role"] == "user"
                        && m["content"].as_str()
                            != Some(codesmith_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT)
                        && !m["content"]
                            .as_array()
                            .is_some_and(|parts| parts.iter().all(|p| p["type"] == "image_url"))
                })
                .unwrap();
            // A prompt with attachments is a list of parts: its text picks.
            let prompt = match &msgs[at]["content"] {
                Value::Array(parts) => {
                    MEDIA_REQUESTS.lock().unwrap().push(b.clone());
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                c => c.as_str().unwrap_or("").to_string(),
            };
            let done = msgs[at..].iter().filter(|m| m["role"] == "tool").count();
            let last = msgs.last().unwrap()["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let after = format!("done: {last}");
            let chunks = if prompt.starts_with("<shell ") {
                // A background command's notice: a resumed turn, or one
                // reported within a prompt's turn.
                let marker = MARKERS.iter().find(|m| prompt.contains(*m)).unwrap();
                match done {
                    0 if prompt.contains("ask-again") => call(
                        7,
                        "shell",
                        json!({"command": "echo again", "description": "again"}),
                    ),
                    0 => {
                        WOKE.lock().unwrap().push(marker.to_string());
                        reply(&format!("woke: {marker}"))
                    }
                    _ => {
                        WOKE.lock().unwrap().push(marker.to_string());
                        reply(&format!("woke again: {marker}"))
                    }
                }
            } else if prompt.contains("linked") {
                LINKED.lock().unwrap().push(b.clone());
                reply("linked")
            } else if prompt.contains("delegate") || prompt.contains("nest deeper") {
                // A subagent that starts one of its own, which reads a file.
                let (description, task) = if prompt.contains("delegate") {
                    ("Explore code", "nest deeper")
                } else {
                    ("Deeper", "leaf read")
                };
                match done {
                    0 => call(
                        1,
                        "subagent",
                        json!({"agent": "nester", "description": description, "prompt": task}),
                    ),
                    _ => reply(&format!("{description} done")),
                }
            } else if prompt.contains("leaf read") {
                match done {
                    0 => call(1, "read", json!({"path": "a.txt"})),
                    _ => reply("leaf done"),
                }
            } else if let Some(command) = prompt.strip_prefix("background ") {
                match done {
                    0 => call(
                        1,
                        "shell",
                        json!({"command": command, "description": "bg", "background": true}),
                    ),
                    _ => reply("started"),
                }
            } else if prompt.contains("slow") {
                match done {
                    0 => call(
                        3,
                        "shell",
                        json!({"command": "sleep 2", "description": "slow"}),
                    ),
                    _ => reply(&after),
                }
            } else if prompt.contains("effort?") {
                reply(&format!("effort: {}", b["reasoning_effort"]))
            } else if prompt.contains("describe") {
                reply("described")
            } else if prompt.contains("think") {
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
            } else if prompt.contains("signed call") {
                // Gemini via Chat: the call's id gains its signature.
                match done {
                    0 => vec![
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "g1", "type": "function",
                            "function": {"name": "read", "arguments": "{\"path\":"},
                            "extra_content": {"google": {"thought_signature": "SIG"}}}]}}]}),
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"a.txt\"}"}}]}, "finish_reason": "tool_calls"}]}),
                    ],
                    _ => reply(&after),
                }
            } else if prompt.contains("cut call") {
                if last == codesmith_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT {
                    reply("smaller")
                } else {
                    vec![
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "cut1", "type": "function",
                        "function": {"name": "read", "arguments": "{\"pa"}}]}, "finish_reason": "length"}]}),
                    ]
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
            r["agentCapabilities"]["_meta"],
            json!({"opencode/child-session-updates": true})
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

    // prompt-content.subprocess.test.ts: "accepts ... inline ... image";
    // replay.test.ts: images replay as image content.
    #[tokio::test(flavor = "multi_thread")]
    async fn image_prompts_reach_the_model_and_replay() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let init = c
            .request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await;
        assert_eq!(
            init["result"]["agentCapabilities"]["promptCapabilities"],
            json!({"image": true, "audio": false, "embeddedContext": true})
        );
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [
                    {"type": "text", "text": "describe this"},
                    {"type": "image", "mimeType": "image/png", "data": PIXEL, "uri": "file:///tmp/shot.png"},
                ]}),
            )
            .await;
        let (done, seen) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let text: String = updates(&seen, "agent_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, "described");
        let url = format!("data:image/png;base64,{PIXEL}");
        let reached = MEDIA_REQUESTS.lock().unwrap().iter().any(|b| {
            b["messages"].as_array().unwrap().iter().any(|m| {
                m["role"] == "user"
                    && m["content"]
                        == json!([
                            {"type": "text", "text": "describe this"},
                            {"type": "image_url", "image_url": {"url": url}},
                        ])
            })
        });
        assert!(reached, "the image reached the model as an image part");

        // An image without data or uri is refused.
        let r = c
            .request(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "image", "mimeType": "image/png", "data": ""}]}),
            )
            .await;
        assert_eq!(r["error"]["code"], -32602, "{r}");

        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c2.until(id, |_| None).await;
        let users = updates(&seen, "user_message_chunk");
        assert_eq!(users.len(), 2, "{users:?}");
        assert_eq!(
            users[0]["content"],
            json!({"type": "text", "text": "describe this"})
        );
        assert_eq!(
            users[1]["content"],
            json!({"type": "image", "mimeType": "image/png", "data": PIXEL, "uri": "file:///shot.png"})
        );
        assert_eq!(users[0]["messageId"], users[1]["messageId"]);
    }

    // replay.test.ts: tool results with images carry them as image content.
    #[tokio::test(flavor = "multi_thread")]
    async fn tool_result_images_are_image_content() {
        let (_d, cwd, base) = tmp().await;
        use base64::Engine;
        std::fs::write(
            cwd.join("pixel.png"),
            base64::engine::general_purpose::STANDARD
                .decode(PIXEL)
                .unwrap(),
        )
        .unwrap();
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "read pixel.png"}]}),
            )
            .await;
        let (done, seen) = c.until(id, allow_once).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let image = json!({"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": PIXEL, "uri": "file:///pixel.png"}});
        let upd = updates(&seen, "tool_call_update");
        let last = upd.last().unwrap();
        assert_eq!(last["status"], "completed");
        assert_eq!(last["content"][1], image, "{last}");

        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c2.until(id, |_| None).await;
        let upd = updates(&seen, "tool_call_update");
        assert_eq!(upd[0]["content"][1], image, "replayed too");
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
            // The engine continues a truncated answer twice before giving up,
            // so the turn is three length-limited steps, summed.
            json!({"stopReason": "max_tokens", "usage": {"inputTokens": 300, "outputTokens": 120, "cachedReadTokens": 33, "totalTokens": 453}})
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
        assert_eq!(streamed.len(), 12, "three steps of four chunks");
        assert_eq!(streamed[..4], expected[..], "the first step");
        let usage = updates(&seen, "usage_update");
        assert_eq!(usage.len(), 1, "sent once, at the end of the turn");
        assert_eq!(
            (
                &usage[0]["used"],
                &usage[0]["size"],
                &usage[0]["cost"]["currency"]
            ),
            (&json!(151), &json!(200000), &json!("USD"))
        );
        let cost = usage[0]["cost"]["amount"].as_f64().unwrap();
        assert!(
            (cost - 3.0 * 180e-6).abs() < 1e-9,
            "the turn's three steps: {cost}"
        );
        // The title is published once generated.
        assert_eq!(
            updates(&seen, "session_info_update")
                .iter()
                .find(|u| u.get("title").is_some())
                .map(|u| u["title"].clone()),
            Some(json!("A title"))
        );
    }

    /// prompt.test.ts (tool lifecycle): a call is announced `pending` as its
    /// input starts streaming, then updated under the same id, even when the
    /// call's own id gained a Gemini signature; a call cut off by the output
    /// limit fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn tool_calls_are_announced_while_they_stream() {
        let (_d, cwd, base) = tmp().await;
        std::fs::write(cwd.join("a.txt"), "hello\n").unwrap();
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let lifecycle = |seen: &[Value]| -> Vec<(String, String, String)> {
            seen.iter()
                .filter(|m| m["params"]["update"].get("toolCallId").is_some())
                .map(|m| {
                    let u = &m["params"]["update"];
                    (
                        u["sessionUpdate"].as_str().unwrap().into(),
                        u["toolCallId"].as_str().unwrap().into(),
                        u["status"].as_str().unwrap().into(),
                    )
                })
                .collect()
        };
        let owned = |v: &[(&str, &str, &str)]| -> Vec<(String, String, String)> {
            v.iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect()
        };
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "signed call"}]}),
            )
            .await;
        let (done, seen) = c
            .until(id, |_| {
                Some(json!({"outcome": {"outcome": "selected", "optionId": "once"}}))
            })
            .await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert_eq!(
            lifecycle(&seen),
            owned(&[
                ("tool_call", "g1", "pending"),
                ("tool_call_update", "g1", "in_progress"),
                ("tool_call_update", "g1", "completed"),
            ])
        );
        let running = updates(&seen, "tool_call_update")[0];
        assert_eq!(running["rawInput"]["path"], "a.txt");

        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "cut call"}]}),
            )
            .await;
        let (_, seen) = c.until(id, |_| None).await;
        assert_eq!(
            lifecycle(&seen),
            owned(&[
                ("tool_call", "cut1", "pending"),
                ("tool_call_update", "cut1", "failed"),
            ])
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
        // The call is announced as its input starts streaming; the input
        // arrives with the update that starts it.
        let call = updates(&seen, "tool_call")[0];
        assert_eq!(
            (&call["name"], &call["kind"]),
            (&json!("shell"), &json!("execute"))
        );
        let running = updates(&seen, "tool_call_update")[0];
        assert_eq!(
            (&running["title"], &running["rawInput"]["command"]),
            (&json!("echo acp"), &json!("echo acp"))
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
        // replay.test.ts: a loaded session replays the same diffs, from what
        // each call recorded, though the files changed since.
        std::fs::write(&new, "changed later\n").unwrap();
        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c2.until(id, |_| None).await;
        let replayed: Vec<&Value> = updates(&seen, "tool_call_update")
            .into_iter()
            .filter(|u| u["status"] == "completed")
            .collect();
        assert_eq!(replayed.len(), 2, "{seen:?}");
        assert_eq!(replayed[0]["content"][1], previews[0]["content"][0]);
        assert_eq!(replayed[1]["content"][1], previews[1]["content"][0]);
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

    /// "Always" approvals outlive the session rebuilds of a cancel and of
    /// mode and model switches.
    #[tokio::test(flavor = "multi_thread")]
    async fn always_approvals_survive_rebuilds() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let prompt =
            |text: &str| json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        let always = |m: &Value| {
            (m["method"] == "session/request_permission")
                .then(|| json!({"outcome": {"outcome": "selected", "optionId": "always"}}))
        };
        let id = c.call("session/prompt", prompt("run it")).await;
        let (done, seen) = c.until(id, always).await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert_eq!(
            seen.iter()
                .filter(|m| m["method"] == "session/request_permission")
                .count(),
            1
        );
        // No ask may reach the client from here on.
        let unasked = |m: &Value| -> Option<Value> {
            panic!("asked again: {m}");
        };
        // A cancelled turn reloads the session.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f.txt"), "x").unwrap();
        let read = format!("read {}", outside.path().join("f.txt").display());
        let pid = c.call("session/prompt", prompt(&read)).await;
        c.wait(|m| m["method"] == "session/request_permission")
            .await;
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        let (done, _) = c.until(pid, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled");
        let id = c.call("session/prompt", prompt("run it")).await;
        let (done, _) = c.until(id, unasked).await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        // Mode and model switches rebuild it too.
        for (method, params) in [
            (
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "plan"}),
            ),
            (
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "build"}),
            ),
            (
                "session/set_config_option",
                json!({"sessionId": sid, "configId": "model", "value": "fake/other"}),
            ),
        ] {
            let r = c.request(method, params).await;
            assert!(r.get("error").is_none(), "{r}");
            let id = c.call("session/prompt", prompt("run it")).await;
            let (done, _) = c.until(id, unasked).await;
            assert_eq!(done["result"]["stopReason"], "end_turn");
        }
        // A new session starts without them.
        let other = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": other, "prompt": [{"type": "text", "text": "run it"}]}),
            )
            .await;
        let (_, seen) = c.until(id, always).await;
        assert!(seen
            .iter()
            .any(|m| m["method"] == "session/request_permission"));
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
                set("effort", json!("ultra")),
                json!({"code": -32602, "message": "Invalid params: effort not found: ultra", "data": {"effort": "ultra"}}),
            ),
            (
                set("model", json!("fake/other#ultra")),
                json!({"code": -32602, "message": "Invalid params: effort not found: ultra", "data": {"effort": "ultra"}}),
            ),
            (
                set("bogus", json!("x")),
                json!({"code": -32602, "message": "Invalid params: unknown config option: bogus", "data": {"configId": "bogus"}}),
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

    // config-option.test.ts: "builds effort option from variants and falls
    // back to default when current variant is invalid"
    // tool.ts `toLocations` / permission.ts `patchPreviews`, for the patch tool.
    #[test]
    fn patch_calls_name_every_file_and_preview_each_hunk() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        std::fs::write(cwd.join("a.txt"), "one\n").unwrap();
        std::fs::write(cwd.join("gone.txt"), "bye\n").unwrap();
        let input = json!({"patchText": "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-one\n+two\n*** Add File: new.txt\n+hi\n*** Delete File: gone.txt\n*** End Patch"});
        assert_eq!(
            locations(cwd, "patch", &input),
            json!([
                {"path": cwd.join("a.txt")},
                {"path": cwd.join("b.txt")},
                {"path": cwd.join("new.txt")},
                {"path": cwd.join("gone.txt")},
            ])
        );
        assert_eq!(
            previews(cwd, "build › patch", &input),
            vec![
                diff(&cwd.join("b.txt"), Some("one\n".into()), "two\n".into()),
                diff(&cwd.join("new.txt"), None, "hi\n".into()),
                diff(&cwd.join("gone.txt"), Some("bye\n".into()), String::new()),
            ]
        );
        assert_eq!(
            permission_title("patch", &input).as_deref(),
            Some("b.txt, new.txt, gone.txt")
        );
        let before = files_before(cwd, "patch", &input);
        assert_eq!(
            before,
            vec![
                (cwd.join("b.txt"), Some("one\n".to_string())),
                (cwd.join("new.txt"), None),
                (cwd.join("gone.txt"), Some("bye\n".to_string())),
            ]
        );
    }

    #[test]
    fn oc_acp_effort_option_shape() {
        let config: config::Config = serde_json::from_value(json!({
            "providers": {"t": {"protocol": "anthropic-messages", "settings": {"baseURL": "http://x", "apiKey": "k"},
                "models": {
                    "effort": {"variants": [{"id": "low"}, {"id": "default"}, {"id": "high"}]},
                    "plain": {},
                    "claude/sonnet-4": {"variants": {"very-high": {"reasoningEffort": "high"}}},
                }}}
        }))
        .unwrap();
        let effort = |spec: &str| {
            config_options(&config, spec, "build")
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == "effort")
                .cloned()
        };
        assert_eq!(
            effort("t/effort").unwrap(),
            json!({
                "id": "effort", "name": "Effort", "description": "Available effort levels for this model",
                "category": "thought_level", "type": "select", "currentValue": "default",
                "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "default", "name": "Default"},
                    {"value": "high", "name": "High"},
                ],
            })
        );
        assert_eq!(effort("t/effort#high").unwrap()["currentValue"], "high");
        // No variants: no effort selector.
        assert_eq!(effort("t/plain"), None);
        let ids: Vec<Value> = config_options(&config, "t/effort", "build")
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].clone())
            .collect();
        assert_eq!(ids, [json!("model"), json!("effort"), json!("mode")]);
        // The model selector names the model, not its variant.
        let all = config_options(&config, "t/claude/sonnet-4#very-high", "build");
        assert_eq!(all[0]["currentValue"], "t/claude/sonnet-4");
        assert_eq!(all[1]["currentValue"], "very-high");
        assert_eq!(all[1]["options"][0]["name"], "Very High");
        assert_eq!(
            current_variant(Some("missing"), &["minimal".into(), "low".into()]),
            "minimal"
        );
        assert_eq!(
            current_variant(Some("missing"), &["low".into(), "default".into()]),
            "default"
        );
        // parseModelSelection: exact slash-containing ids win; a trailing
        // `/variant` or `#variant` picks a variant.
        for (value, spec) in [
            ("t/effort", "t/effort"),
            ("t/effort/low", "t/effort#low"),
            ("t/effort#high", "t/effort#high"),
            ("t/effort#default", "t/effort"),
            ("t/claude/sonnet-4", "t/claude/sonnet-4"),
            ("t/claude/sonnet-4/very-high", "t/claude/sonnet-4#very-high"),
            ("t/claude/sonnet-4/missing", "t/claude/sonnet-4/missing"),
        ] {
            assert_eq!(model_selection(&config, value, "t/plain").unwrap(), spec);
        }
        // Switching to the same model keeps the variant; another drops it.
        assert_eq!(
            model_selection(&config, "t/effort", "t/effort#high").unwrap(),
            "t/effort#high"
        );
        assert_eq!(
            model_selection(&config, "t/plain", "t/effort#high").unwrap(),
            "t/plain"
        );
        assert_eq!(
            model_selection(&config, "t/effort#nope", "t/plain")
                .unwrap_err()
                .1,
            "Invalid params: effort not found: nope"
        );
    }

    // catalog.test.ts: "switches model, effort, and mode against the warm
    // catalog" (the effort part)
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_effort_switches_like_the_model() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start_with(
            &cwd,
            &base,
            json!({"providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"},
                "models": {"m": {"variants": {"fast": {"reasoningEffort": "minimal"}}}, "other": {"name": "Other"}}}}}),
        );
        let new = c
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let effort = option(&new, "effort");
        assert_eq!(effort["currentValue"], "default");
        // Built-in reasoning variants, then the configured ones.
        let values: Vec<&str> = effort["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["low", "medium", "high", "fast", "default"]);
        let set = |id: &str, value: &str| json!({"sessionId": sid, "configId": id, "value": value});
        async fn ask(c: &mut Client, sid: &str) -> String {
            let id = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": "effort?"}]}),
                )
                .await;
            let (_, seen) = c.until(id, |_| None).await;
            updates(&seen, "agent_message_chunk")
                .iter()
                .filter_map(|u| u["content"]["text"].as_str())
                .collect()
        }
        assert_eq!(ask(&mut c, &sid).await, "effort: null");

        let id = c
            .call("session/set_config_option", set("effort", "high"))
            .await;
        let (r, seen) = c.until(id, |_| None).await;
        assert_eq!(option(&r, "effort")["currentValue"], "high");
        assert_eq!(option(&r, "model")["currentValue"], "fake/m");
        let pushed = updates(&seen, "config_option_update");
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0]["configOptions"], r["result"]["configOptions"]);
        assert_eq!(ask(&mut c, &sid).await, "effort: \"high\"");
        let stored = |cwd: &Path| {
            Store::open(&cwd.join("acp.db"))
                .unwrap()
                .get(&sid)
                .unwrap()
                .unwrap()
                .model
        };
        assert_eq!(stored(&cwd), "fake/m#high");

        // A configured variant; `provider/model/variant` picks one too.
        let r = c
            .request("session/set_config_option", set("model", "fake/m/fast"))
            .await;
        assert_eq!(option(&r, "effort")["currentValue"], "fast");
        assert_eq!(ask(&mut c, &sid).await, "effort: \"minimal\"");
        // The same model keeps its variant.
        let r = c
            .request("session/set_config_option", set("model", "fake/m"))
            .await;
        assert_eq!(option(&r, "effort")["currentValue"], "fast");
        // Another model drops it.
        let r = c
            .request("session/set_config_option", set("model", "fake/other"))
            .await;
        assert_eq!(option(&r, "effort")["currentValue"], "default");
        assert_eq!(stored(&cwd), "fake/other");
        let r = c
            .request("session/set_config_option", set("model", "fake/m#medium"))
            .await;
        assert_eq!(option(&r, "effort")["currentValue"], "medium");
        let r = c
            .request("session/set_config_option", set("effort", "default"))
            .await;
        assert_eq!(option(&r, "effort")["currentValue"], "default");
        assert_eq!(stored(&cwd), "fake/m");

        // During a turn the switch waits for it to end, and is stored.
        let pid = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "think"}]}),
            )
            .await;
        let se = c
            .call("session/set_config_option", set("effort", "low"))
            .await;
        let mut answered = 0;
        while answered < 2 {
            let m = c.recv().await;
            if m.get("method").is_none() && (m["id"] == json!(pid) || m["id"] == json!(se)) {
                assert!(m.get("error").is_none(), "{m}");
                if m["id"] == json!(se) {
                    assert_eq!(option(&m, "effort")["currentValue"], "low");
                }
                answered += 1;
            }
        }
        assert_eq!(ask(&mut c, &sid).await, "effort: \"low\"");
        assert_eq!(stored(&cwd), "fake/m#low");
        let mut c2 = start(&cwd, &base);
        let r = c2
            .request(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        assert_eq!(option(&r, "model")["currentValue"], "fake/m");
        assert_eq!(option(&r, "effort")["currentValue"], "low");
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
        // additional-directories.test.ts: "load and resume replace ACP
        // grants": the grant is kept with the session and listed, and a
        // resume without directories drops it.
        let listed = |r: &Value, sid: &str| {
            r["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["sessionId"] == sid)
                .cloned()
                .unwrap()
        };
        let r = c.request("session/list", json!({"cwd": cwd})).await;
        assert_eq!(
            listed(&r, &granted)["additionalDirectories"],
            json!([shared])
        );
        assert!(listed(&r, &plain).get("additionalDirectories").is_none());
        c.request("session/resume", json!({"cwd": cwd, "sessionId": granted}))
            .await;
        let r = c.request("session/list", json!({"cwd": cwd})).await;
        assert!(listed(&r, &granted).get("additionalDirectories").is_none());
        // "forks replace inherited grants with the requested list"
        let r = c
            .request(
                "session/fork",
                json!({"cwd": cwd, "sessionId": plain, "additionalDirectories": [shared]}),
            )
            .await;
        let fork = r["result"]["sessionId"].as_str().unwrap().to_string();
        let r = c.request("session/list", json!({"cwd": cwd})).await;
        assert_eq!(listed(&r, &fork)["additionalDirectories"], json!([shared]));
        assert!(listed(&r, &plain).get("additionalDirectories").is_none());
    }

    // session.test.ts: session/list pages, newest first, 100 at a time.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_session_list_pages() {
        let (_d, cwd, base) = tmp().await;
        let db = Store::open(&cwd.join("acp.db")).unwrap();
        for i in 0..205 {
            db.create(&store::Info {
                id: format!("ses_{i:03}"),
                directory: cwd.display().to_string(),
                title: Some(format!("session {i}")),
                agent: "build".into(),
                model: "fake/m".into(),
                parent_id: None,
                created: i,
                updated: 1_000 + i / 2,
                usage: Usage::default(),
            })
            .unwrap();
        }
        let mut c = start(&cwd, &base);
        let mut seen = Vec::new();
        let mut cursor = Value::Null;
        let mut pages = 0;
        loop {
            let r = c
                .request("session/list", json!({"cwd": cwd, "cursor": cursor}))
                .await;
            let page = r["result"]["sessions"].as_array().unwrap().clone();
            pages += 1;
            seen.extend(
                page.iter()
                    .map(|s| s["sessionId"].as_str().unwrap().to_string()),
            );
            match r["result"].get("nextCursor") {
                Some(next) => {
                    assert_eq!(page.len(), 100);
                    cursor = next.clone();
                }
                None => break,
            }
        }
        assert_eq!(pages, 3);
        let mut want: Vec<String> = (0..205).map(|i| format!("ses_{i:03}")).collect();
        want.reverse();
        assert_eq!(seen, want, "newest first, each once");
        let bad = c
            .request("session/list", json!({"cwd": cwd, "cursor": "nope"}))
            .await;
        assert_eq!(bad["error"]["code"], -32602);
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
                // The truncated answer and the engine's two continuations
                // (their nudges are not replayed as user messages).
                "agent_thought_chunk",
                "agent_message_chunk",
                "agent_thought_chunk",
                "agent_message_chunk",
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
            (&u[8]["toolCallId"], &u[8]["status"], &u[8]["kind"]),
            (&json!("c1"), &json!("pending"), &json!("read"))
        );
        assert_eq!(
            u[8]["locations"],
            json!([{"path": cwd.join("missing.txt")}])
        );
        assert_eq!(
            (&u[9]["toolCallId"], &u[9]["status"]),
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
                let u: Vec<&Value> = seen
                    .iter()
                    .map(|m| &m["params"]["update"])
                    .filter(|u| {
                        u["sessionUpdate"]
                            .as_str()
                            .is_some_and(|k| k.starts_with("compaction"))
                    })
                    .collect();
                let cid = &u[0]["compactionId"];
                assert!(cid.is_string());
                assert_eq!(
                    u,
                    [
                        &json!({"sessionUpdate": "compaction_update", "compactionId": cid, "status": "in_progress"}),
                        &json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": cid, "content": {"type": "text", "text": "## Objective\n"}}),
                        &json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": cid, "content": {"type": "text", "text": "Ship it."}}),
                        &json!({"sessionUpdate": "compaction_update", "compactionId": cid, "status": "completed", "summary": [{"type": "text", "text": "## Objective\nShip it."}]}),
                    ]
                );
            }
        }
        // Without the capability: a started and a completed marker, no chunks.
        let mut c3 = start_with(&cwd, &base, json!({"compaction": {"keep": {"tokens": 0}}}));
        let sid3 = c3.new_session(&cwd).await;
        for text in ["think", "/compact"] {
            let id = c3
                .call(
                    "session/prompt",
                    json!({"sessionId": sid3, "prompt": [{"type": "text", "text": text}]}),
                )
                .await;
            let (done, seen) = c3.until(id, |_| None).await;
            assert!(done.get("error").is_none(), "{done}");
            if text == "/compact" {
                assert!(updates(&seen, "compaction_summary_chunk").is_empty());
                let markers: Vec<&Value> = updates(&seen, "session_info_update")
                    .into_iter()
                    .filter_map(|u| u["_meta"].get(COMPACTION_META))
                    .collect();
                assert_eq!(markers.len(), 2, "{seen:?}");
                assert_eq!(markers[0]["status"], "started");
                assert_eq!(markers[0]["reason"], "manual");
                assert_eq!(markers[1]["status"], "completed");
                assert_eq!(markers[1]["messageId"], markers[0]["messageId"]);
                assert!(markers[1]["tokensAfter"].is_u64());
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

    // A turn that ends as a cancel arrives is answered once, and the
    // session takes the next prompt (the cancel lands between the turn's
    // end and its settling).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_as_the_turn_ends_answers_the_prompt() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        PAUSED.lock().unwrap().push(sid.clone());
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        c.wait(|m| chunk_text(m) == Some("described")).await;
        // The turn has ended and parks before settling.
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        let (done, _) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
        PAUSED.lock().unwrap().retain(|s| *s != sid);
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        let (done, _) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    }

    // prompt-content.subprocess.test.ts: "accepts embedded text resource,
    // inline and remote image, and file resource link prompt content": a
    // linked file is attached as the Read tool sees it, through its
    // permission checks, and replays as the link.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_linked_files_are_attached() {
        let (_d, cwd, base) = tmp().await;
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        std::fs::write(cwd.join("README.md"), "# ACP content smoke\n").unwrap();
        std::fs::write(outside.join("secret.txt"), "hidden words\n").unwrap();
        std::fs::write(cwd.join("shot.png"), media::unbase64(PIXEL).unwrap()).unwrap();
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let cases = [
            (
                "linked readme",
                json!({"type": "resource_link", "uri": file_uri(&cwd.join("README.md").display().to_string()), "name": "README.md", "mimeType": "text/markdown"}),
            ),
            (
                "linked missing",
                json!({"type": "resource_link", "uri": file_uri(&cwd.join("missing.md").display().to_string()), "name": "missing.md"}),
            ),
            (
                "linked outside",
                json!({"type": "resource_link", "uri": file_uri(&outside.join("secret.txt").display().to_string()), "name": "secret.txt"}),
            ),
            (
                "linked image",
                json!({"type": "image", "mimeType": "image/png", "data": "", "uri": file_uri(&cwd.join("shot.png").display().to_string())}),
            ),
        ];
        let mut asked = Vec::new();
        for (text, link) in &cases {
            let id = c
                .call(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}, link]}),
                )
                .await;
            let (done, _) = c
                .until(id, |m| {
                    asked.push(m["params"].clone());
                    Some(json!({"outcome": {"outcome": "selected", "optionId": "reject"}}))
                })
                .await;
            assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        }
        let sent: Vec<String> = LINKED
            .lock()
            .unwrap()
            .iter()
            .map(|b| b["messages"].to_string())
            .collect();
        let find = |what: &str| {
            sent.iter()
                .rev()
                .find(|m| m.contains(what))
                .cloned()
                .unwrap_or_else(|| panic!("no request for {what}"))
        };
        let readme = find("linked readme");
        assert!(readme.contains("Called the Read tool"), "{readme}");
        assert!(readme.contains("1: # ACP content smoke"), "{readme}");
        assert!(find("linked missing").contains("missing.md"));
        // Outside the project the client is asked first, and a refusal
        // keeps the file out.
        let outside_ask = asked
            .iter()
            .find(|a| a["toolCall"]["toolCallId"] == "perm_external_directory")
            .expect("an external_directory request");
        assert_eq!(outside_ask["sessionId"], json!(sid));
        assert!(!find("linked outside").contains("hidden words"));
        assert!(find("linked image").contains("image_url"));
        // A loaded session replays the link, not the file's contents.
        let mut c2 = start(&cwd, &base);
        let id = c2
            .call(
                "session/load",
                json!({"cwd": cwd, "sessionId": sid, "mcpServers": []}),
            )
            .await;
        let (_, seen) = c2.until(id, |_| None).await;
        let user = updates(&seen, "user_message_chunk");
        assert!(user.iter().any(|u| u["content"]["text"] == "linked readme"));
        assert!(user.iter().any(|u| u["content"]
            == json!({"type": "resource_link", "uri": file_uri(&cwd.join("README.md").display().to_string()), "name": "README.md"})));
        assert!(!user.iter().any(|u| u["content"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("ACP content smoke"))));
    }

    /// A client and a session whose prompt runs a subagent, which runs
    /// one of its own that reads a file (with permission).
    async fn delegate(child_updates: bool) -> (Value, Vec<Value>, Vec<Value>, String) {
        let (_d, cwd, base) = tmp().await;
        std::fs::write(cwd.join("a.txt"), "alpha\n").unwrap();
        let mut c = start_with(
            &cwd,
            &base,
            json!({
                "subagent_depth": 2,
                "agents": {"nester": {"mode": "subagent", "description": "Nests"}},
                "permissions": [{"action": "read", "resource": "*", "effect": "ask"}],
            }),
        );
        let caps = if child_updates {
            json!({"_meta": {"opencode/child-session-updates": true}})
        } else {
            json!({})
        };
        c.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": caps}),
        )
        .await;
        let sid = c.new_session(&cwd).await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "delegate"}]}),
            )
            .await;
        let mut asked = Vec::new();
        let (done, seen) = c
            .until(id, |m| {
                asked.push(m["params"].clone());
                allow_once(m)
            })
            .await;
        (done, seen, asked, sid)
    }

    // turn.test.ts: "shows nested and background child work and routes its
    // permissions (child capability: false)": a subagent's calls stream in
    // the parent's own updates, under ids and titles of their child session.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_child_work_is_projected_into_the_parent() {
        let (done, seen, asked, sid) = delegate(false).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        assert!(!seen.iter().any(|m| m["method"] == CHILD_UPDATE_METHOD));
        let calls = updates(&seen, "tool_call");
        // The parent's subagent call, the child's, and the grandchild's read.
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0]["toolCallId"], "c1");
        assert!(calls[0]["_meta"].is_null());
        let child = &calls[1]["_meta"][CHILD_META];
        assert_eq!(child["parentID"], json!(sid));
        assert_eq!(child["depth"], 1);
        assert_eq!(child["title"], "Explore code");
        assert_eq!(
            calls[1]["toolCallId"],
            json!(format!("{}:c1", child["id"].as_str().unwrap()))
        );
        assert!(calls[1]["title"]
            .as_str()
            .unwrap()
            .starts_with("Explore code: subagent"));
        let grandchild = &calls[2]["_meta"][CHILD_META];
        assert_eq!(grandchild["parentID"], child["id"]);
        assert_eq!(grandchild["depth"], 2);
        assert_eq!(grandchild["title"], "Deeper");
        let read_id = format!("{}:c1", grandchild["id"].as_str().unwrap());
        assert_eq!(calls[2]["toolCallId"], json!(read_id));
        assert_eq!(calls[2]["title"], "Deeper: read a.txt");
        // Its permission request points at the projected call.
        let read_ask = asked
            .iter()
            .find(|a| a["toolCall"]["toolCallId"] == json!(read_id))
            .expect("the read's permission request");
        assert_eq!(read_ask["sessionId"], json!(sid));
        assert_eq!(read_ask["toolCall"]["title"], "Deeper: a.txt");
        assert_eq!(read_ask["toolCall"]["_meta"][CHILD_META], *grandchild);
        let ended: Vec<&Value> = updates(&seen, "tool_call_update")
            .into_iter()
            .filter(|u| u["status"] == "completed")
            .collect();
        assert_eq!(
            ended.iter().map(|u| &u["toolCallId"]).collect::<Vec<_>>(),
            [&json!(read_id), &calls[1]["toolCallId"], &json!("c1")]
        );
        // The children's answers are marked as theirs.
        let texts: Vec<&Value> = updates(&seen, "agent_message_chunk");
        let deeper = texts
            .iter()
            .find(|u| u["content"]["text"] == "Deeper done")
            .unwrap();
        assert_eq!(deeper["_meta"][CHILD_META], *child);
        let own = texts
            .iter()
            .find(|u| u["content"]["text"] == "Explore code done")
            .unwrap();
        assert!(own["_meta"].is_null());
    }

    // turn.test.ts: "shows nested and background child work and routes its
    // permissions (child capability: true)": with the capability, a
    // subagent's activity and status go apart, as child_update notifications.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_child_work_goes_to_child_updates() {
        let (done, seen, asked, sid) = delegate(true).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let calls = updates(&seen, "tool_call");
        assert_eq!(calls.len(), 1, "only the parent's own call: {calls:?}");
        assert!(updates(&seen, "agent_message_chunk")
            .iter()
            .all(|u| u["_meta"].is_null()));
        let child: Vec<&Value> = seen
            .iter()
            .filter(|m| m["method"] == CHILD_UPDATE_METHOD)
            .map(|m| &m["params"])
            .collect();
        assert!(child.iter().all(|p| p["rootSessionId"] == json!(sid)));
        let statuses: Vec<(u64, &str)> = child
            .iter()
            .filter(|p| p["type"] == "status")
            .map(|p| (p["depth"].as_u64().unwrap(), p["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            statuses,
            [
                (1, "created"),
                (1, "running"),
                (2, "created"),
                (2, "running"),
                (2, "completed"),
                (1, "completed"),
            ]
        );
        let first = child[0];
        assert_eq!(first["parentSessionId"], json!(sid));
        assert_eq!(first["title"], "Explore code");
        let deeper = child.iter().find(|p| p["depth"] == 2).unwrap();
        assert_eq!(deeper["parentSessionId"], first["childSessionId"]);
        let read_id = format!("{}:c1", deeper["childSessionId"].as_str().unwrap());
        let read: Vec<&str> = child
            .iter()
            .filter(|p| p["type"] == "update" && p["update"]["toolCallId"] == json!(read_id))
            .map(|p| p["update"]["sessionUpdate"].as_str().unwrap())
            .collect();
        assert_eq!(read, ["tool_call", "tool_call_update", "tool_call_update"]);
        assert!(child.iter().any(|p| p["type"] == "update"
            && p["childSessionId"] == first["childSessionId"]
            && p["update"]["content"]["text"] == "Deeper done"));
        // Permissions still go to the session, with the child's call.
        assert!(
            asked
                .iter()
                .any(|a| a["sessionId"] == json!(sid)
                    && a["toolCall"]["toolCallId"] == json!(read_id))
        );
    }

    // compaction.ts `update`: failed and cancelled compactions, for clients
    // with and without the capability.
    #[test]
    fn oc_acp_compaction_failure_shapes() {
        assert_eq!(
            compaction_update(true, "c", "auto", "failed", Some("model down")),
            json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "failed", "summary": null, "error": "model down"})
        );
        assert_eq!(
            compaction_update(true, "c", "auto", "failed", Some("a\nb"))["error"],
            "Compaction failed"
        );
        assert_eq!(
            compaction_update(true, "c", "manual", "cancelled", None),
            json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "cancelled", "summary": null})
        );
        assert_eq!(
            compaction_update(true, "c", "manual", "completed", Some("")),
            json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed", "summary": null})
        );
        assert_eq!(
            compaction_update(false, "c", "manual", "cancelled", None)["_meta"][COMPACTION_META],
            json!({"status": "failed", "messageId": "c", "reason": "manual", "error": {"type": "aborted", "message": "Compaction cancelled"}})
        );
        assert_eq!(
            compaction_update(false, "c", "auto", "failed", Some("x"))["_meta"][COMPACTION_META]
                ["error"]["message"],
            "x"
        );
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

    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    /// The text of a message's `agent_message_chunk` update, if it is one.
    fn chunk_text(m: &Value) -> Option<&str> {
        let u = &m["params"]["update"];
        (m["method"] == "session/update" && u["sessionUpdate"] == "agent_message_chunk")
            .then(|| u["content"]["text"].as_str())
            .flatten()
    }

    fn woke(marker: &str) -> usize {
        WOKE.lock().unwrap().iter().filter(|m| *m == marker).count()
    }

    /// Start `command` in the background and end the turn while it runs.
    async fn launch(c: &mut Client, sid: &str, command: &str) -> Vec<Value> {
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": format!("background {command}")}]}),
            )
            .await;
        let (done, seen) = c.until(id, allow_once).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{seen:?}");
        let text: String = seen.iter().filter_map(chunk_text).collect();
        assert_eq!(text, "started");
        seen
    }

    /// OpenCode's background-turn test: a background command finishing
    /// after `end_turn` wakes the idle session, and the model's reaction
    /// streams as agent-initiated `session/update`s (a new message, no
    /// request to answer); the session then takes prompts as usual.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_background_shell_wakes_the_session() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        let seen = launch(&mut c, &sid, "sleep 1; echo done-wake").await;
        let launched = seen
            .iter()
            .find(|m| chunk_text(m).is_some())
            .map(|m| m["params"]["update"]["messageId"].clone())
            .unwrap();
        let (m, before) = c
            .wait(|m| chunk_text(m).is_some_and(|t| t.contains("woke")))
            .await;
        assert_eq!(chunk_text(&m), Some("woke: done-wake"));
        assert_eq!(m["params"]["sessionId"], sid.as_str());
        assert_ne!(m["params"]["update"]["messageId"], launched);
        assert!(
            before.iter().all(|m| m.get("id").is_none()),
            "no request or response: {before:?}"
        );
        assert_eq!(woke("done-wake"), 1);
        let r = c
            .request(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
        assert_eq!(woke("done-wake"), 1, "reported once");
    }

    /// A prompt sent while the session waits for its background command
    /// takes over: its turn reports the command, once, and no resumed turn
    /// follows.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_prompt_during_the_wait_reports_the_command_once() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        launch(&mut c, &sid, "sleep 1; echo done-prompt").await;
        // Its foreground `sleep 2` outlasts the background command.
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "slow"}]}),
            )
            .await;
        let (done, seen) = c.until(id, allow_once).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{seen:?}");
        let text: String = seen.iter().filter_map(chunk_text).collect();
        assert_eq!(text, "woke: done-prompt");
        assert_eq!(woke("done-prompt"), 1);
        // Nothing wakes the session again.
        let more = tokio::time::timeout(Duration::from_millis(1500), c.recv()).await;
        assert!(more.is_err(), "{more:?}");
        assert_eq!(woke("done-prompt"), 1);
    }

    /// The resumed turn asks permission through the client as usual;
    /// `session/cancel` stops it (withdrawing the request and failing the
    /// call) and answers a prompt queued behind it with `cancelled`. The
    /// session reloads and takes prompts again.
    #[tokio::test(flavor = "multi_thread")]
    async fn oc_acp_cancel_stops_a_resumed_turn() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        launch(&mut c, &sid, "sleep 1; echo ask-again done-cancel").await;
        let (ask, _) = c
            .wait(|m| m["method"] == "session/request_permission")
            .await;
        assert_eq!(ask["params"]["sessionId"], sid.as_str());
        assert_eq!(ask["params"]["toolCall"]["toolCallId"], "c7");
        let queued = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        // The prompt waits for the resumed turn rather than failing.
        let early = tokio::time::timeout(Duration::from_millis(300), c.recv()).await;
        assert!(early.is_err(), "{early:?}");
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        let (done, seen) = c.until(queued, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{seen:?}");
        let failed = seen.iter().any(|m| {
            m["params"]["update"]["toolCallId"] == "c7"
                && m["params"]["update"]["status"] == "failed"
        });
        assert!(failed, "{seen:?}");
        let withdrawn = seen
            .iter()
            .any(|m| m["method"] == "$/cancel_request" && m["params"]["requestId"] == ask["id"]);
        assert!(withdrawn, "{seen:?}");
        let r = c
            .request(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
        assert_eq!(woke("done-cancel"), 0);
    }

    /// Closing or deleting a session that waits for a background command
    /// stops the wait and the command: nothing wakes it, and the command
    /// never gets to its end.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_a_waiting_session_stops_its_wake() {
        for (method, marker) in [
            ("session/close", "done-close"),
            ("session/delete", "done-delete"),
        ] {
            let (_d, cwd, base) = tmp().await;
            let mut c = start(&cwd, &base);
            let sid = c.new_session(&cwd).await;
            launch(
                &mut c,
                &sid,
                &format!("sleep 1; touch ended; echo {marker}"),
            )
            .await;
            let r = c.request(method, json!({"sessionId": sid})).await;
            assert_eq!(r["result"], json!({}), "{r}");
            let more = tokio::time::timeout(Duration::from_millis(1800), c.recv()).await;
            assert!(more.is_err(), "{more:?}");
            assert_eq!(woke(marker), 0);
            assert!(
                !cwd.join("ended").exists(),
                "{method} left the command running"
            );
        }
    }

    /// Fixed: a cancel and every mode or model switch rebuilt the session
    /// and stopped its background commands with the old one, so their
    /// results were never reported. As in OpenCode they keep running, and
    /// the command still wakes the session when it finishes.
    #[tokio::test(flavor = "multi_thread")]
    async fn background_commands_survive_a_cancel_and_mode_and_model_switches() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        launch(&mut c, &sid, "sleep 3; echo done-keep").await;
        // A later turn, cancelled while it asks to run its command.
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "slow"}]}),
            )
            .await;
        c.wait(|m| m["method"] == "session/request_permission")
            .await;
        c.notify("session/cancel", json!({"sessionId": sid})).await;
        let (done, _) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "cancelled");
        let set = |id: &str, value: &str| json!({"sessionId": sid, "configId": id, "value": value});
        for (option, value) in [("mode", "plan"), ("model", "fake/other"), ("mode", "build")] {
            let r = c
                .request("session/set_config_option", set(option, value))
                .await;
            assert!(r.get("error").is_none(), "{r}");
        }
        let (m, _) = c
            .wait(|m| chunk_text(m).is_some_and(|t| t.contains("woke")))
            .await;
        assert_eq!(chunk_text(&m), Some("woke: done-keep"));
        assert_eq!(woke("done-keep"), 1);
    }

    /// Reopening (`session/load`) a session that is open keeps its
    /// background commands: the reopened session is woken by them.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopening_a_session_keeps_its_background_commands() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        launch(&mut c, &sid, "sleep 2; echo done-reopen").await;
        let r = c
            .request(
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        let (m, _) = c
            .wait(|m| chunk_text(m).is_some_and(|t| t.contains("woke")))
            .await;
        assert_eq!(chunk_text(&m), Some("woke: done-reopen"));
        assert_eq!(woke("done-reopen"), 1);
    }

    /// A prompt sent during a resumed turn waits for it to end, then runs
    /// (the closest to OpenCode steering it into the running turn).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_prompt_during_a_resumed_turn_runs_after_it() {
        let (_d, cwd, base) = tmp().await;
        let mut c = start(&cwd, &base);
        let sid = c.new_session(&cwd).await;
        launch(&mut c, &sid, "sleep 1; echo ask-again done-queue").await;
        let (ask, _) = c
            .wait(|m| m["method"] == "session/request_permission")
            .await;
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "describe"}]}),
            )
            .await;
        c.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": allow_once(&ask)}))
            .await;
        let (done, seen) = c.until(id, |_| None).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{seen:?}");
        let text: String = seen.iter().filter_map(chunk_text).collect();
        assert_eq!(text, "woke again: done-queuedescribed");
        assert_eq!(woke("done-queue"), 1);
    }

    #[test]
    fn content_and_servers() {
        let limits = ImageLimits::default();
        let PromptContent {
            text: t,
            media: m,
            files,
        } = prompt_content(
            &json!([
                {"type": "text", "text": "look"},
                {"type": "resource", "resource": {"uri": "file:///p/a.rs", "text": "fn a() {}"}},
                {"type": "resource_link", "name": "b.rs", "uri": "file:///p/b%20c.rs"},
                {"type": "resource_link", "name": "z", "uri": "zed://file?path=%2Fp%2Fz.rs&x=1"},
                {"type": "resource_link", "name": "site", "uri": "https://example.com/a"},
                {"type": "resource_link", "uri": "https://example.com/b"},
                {"type": "audio", "data": "AAEC", "mimeType": "audio/wav"}
            ]),
            &limits,
        )
        .unwrap();
        assert_eq!(t, "look\n\n<file uri=\"file:///p/a.rs\">\nfn a() {}\n</file>\n\n[site](https://example.com/a)\n\nhttps://example.com/b\n\n[audio content omitted: not supported]");
        assert!(m.is_empty());
        assert_eq!(
            files,
            [PathBuf::from("/p/b c.rs"), PathBuf::from("/p/z.rs")]
        );
        // prompt-content.subprocess.test.ts and prompt.test.ts: inline and
        // remote images, blob resources, and an image with neither.
        let PromptContent {
            text: t,
            media: m,
            files,
        } = prompt_content(
            &json!([
                {"type": "text", "text": "compare"},
                {"type": "image", "mimeType": "image/png", "data": PIXEL, "uri": "file:///p/shot.png"},
                {"type": "image", "mimeType": "image/png", "data": "", "uri": "https://example.com/remote.png"},
                {"type": "image", "mimeType": "image/png", "data": "", "uri": "file:///p/local.png"},
                {"type": "resource", "resource": {"uri": "file:///p/doc.pdf", "mimeType": "application/pdf", "blob": "JVBERi0xLjQ="}},
                {"type": "resource", "resource": {"uri": "file:///p/a.bin", "mimeType": "application/octet-stream", "blob": "AAEC"}},
            ]),
            &limits,
        )
        .unwrap();
        assert_eq!(t, "compare\n\n[remote.png](https://example.com/remote.png)\n\n[attached resource file:///p/a.bin omitted: binary]");
        assert_eq!(files, [PathBuf::from("/p/local.png")]);
        assert_eq!(
            m,
            vec![
                Media::base64("image/png", PIXEL).named("shot.png"),
                Media::base64("application/pdf", "JVBERi0xLjQ=").named("doc.pdf"),
            ]
        );
        let no_source = prompt_content(
            &json!([{"type": "image", "mimeType": "image/png", "data": ""}]),
            &limits,
        )
        .unwrap_err();
        assert_eq!(no_source.0, -32602);
        assert!(no_source.1.contains("image content has no data or uri"));
        // An image too large to fit when resizing is off is refused.
        let tight = ImageLimits {
            auto_resize: false,
            max_width: 0,
            ..ImageLimits::default()
        };
        let big = prompt_content(
            &json!([{"type": "image", "mimeType": "image/png", "data": PIXEL}]),
            &tight,
        )
        .unwrap_err();
        assert!(big.1.contains("exceeding configured limits"), "{}", big.1);
        assert_eq!(
            tool_media_content(&[
                Media::base64("image/png", PIXEL),
                Media::base64("application/pdf", "JVBERi0=")
            ]),
            vec![
                json!({"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": PIXEL}}),
                json!({"type": "content", "content": {"type": "resource", "resource": {"uri": "file:///file", "mimeType": "application/pdf", "blob": "JVBERi0="}}}),
            ]
        );
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
        assert_eq!(tool_kind("patch"), "edit");
        assert_eq!(tool_kind("apply_patch"), "edit");
        assert_eq!(stop_reason(Some(Reason::Length)), "max_tokens");
        assert_eq!(stop_reason(Some(Reason::ContentFilter)), "refusal");
        assert_eq!(stop_reason(Some(Reason::Stop)), "end_turn");
        assert_eq!(stop_reason(None), "end_turn");
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

    // A project symlink to a file outside: the prompt for the outside
    // directory names that directory and the real file, not the link.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_symlinks_permission_prompt_names_its_real_target() {
        let (_d, cwd, base) = tmp().await;
        let outside = tempfile::tempdir().unwrap();
        let secret_dir = outside.path().canonicalize().unwrap().join("dotssh");
        std::fs::create_dir(&secret_dir).unwrap();
        let secret = secret_dir.join("id_rsa");
        std::fs::write(&secret, "PRIVATE KEY MATERIAL").unwrap();
        std::os::unix::fs::symlink(&secret, cwd.join("notes.md")).unwrap();
        let mut c = start(&cwd, &base);
        c.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
        let new = c
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let id = c
            .call(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "read notes.md"}]}),
            )
            .await;
        let mut asks = vec![];
        c.until(id, |m| {
            asks.push(m["params"]["toolCall"].clone());
            allow_once(m)
        })
        .await;
        let external = &asks[0];
        let title = external["title"].as_str().unwrap();
        assert!(title.contains("outside the project"), "{title}");
        assert!(title.contains(&secret_dir.display().to_string()), "{title}");
        assert_eq!(
            external["locations"],
            json!([{"path": secret.display().to_string()}])
        );
        let text = external["content"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains(&secret.display().to_string()), "{text}");
        assert!(
            text.contains(&format!("{}/*", secret_dir.display())),
            "{text}"
        );
    }
}
