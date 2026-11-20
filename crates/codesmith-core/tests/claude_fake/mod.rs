//! A fake `claude` (after OpenCode's `test/plugin/provider/claude-agent/
//! lib/fake-claude.ts`, MIT): `fixtures/claude-agent/fake_claude.py` is the
//! executable CodeSmith starts, and relays its input and output over a Unix
//! socket to this side, which answers from Claude Code sessions OpenCode
//! recorded (`fixtures/claude-agent/recordings`). The real `claude` never
//! runs.
//!
//! Each `claude` started takes the next steps of one script, in order. A
//! step can be held, and the test can wait until a step started (its input
//! arrived) or finished (its replies were written). Recorded names of
//! OpenCode's tool server are read as CodeSmith's.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-agent")
}

pub fn executable() -> PathBuf {
    fixtures().join("fake_claude.py")
}

/// A one-shot latch.
#[derive(Clone)]
pub struct Gate(Arc<watch::Sender<bool>>);

impl Default for Gate {
    fn default() -> Self {
        Gate(Arc::new(watch::channel(false).0))
    }
}

impl Gate {
    pub fn open(&self) {
        self.0.send_replace(true);
    }
    pub fn is_open(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
    /// A gate that never opens.
    pub fn never() -> Gate {
        Gate::default()
    }
}

// ---------------------------------------------------------------------------
// Recordings

pub fn load(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixtures().join(format!("recordings/{name}.jsonl")))
        .unwrap_or_else(|e| panic!("recording {name}: {e}"));
    text.trim()
        .lines()
        .skip(1)
        .map(|l| {
            let l = l
                .replace("mcp__opencode__", "mcp__codesmith__")
                .replace("\"opencode\"", "\"codesmith\"");
            serde_json::from_str(&l).unwrap()
        })
        .collect()
}

/// What `claude` answered the first start-up request.
pub fn startup_reply(lines: &[Value]) -> Value {
    lines
        .iter()
        .find(|l| l["from"] == "claude" && l["message"]["type"] == "control_response")
        .map(|l| l["message"]["response"]["response"].clone())
        .expect("a start-up reply")
}

/// The `index`th result of a recording.
pub fn result(name: &str, index: usize) -> Value {
    load(name)
        .into_iter()
        .filter(|l| l["from"] == "claude" && l["message"]["type"] == "result")
        .nth(index)
        .map(|l| l["message"].clone())
        .unwrap_or_else(|| panic!("no result {index} in {name}"))
}

// ---------------------------------------------------------------------------
// Scripts

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Startup,
    User,
    Interrupt,
    Exchange,
}

/// One step of a script.
#[derive(Clone)]
pub struct Step {
    pub kind: Kind,
    /// A start-up reply.
    pub reply: Value,
    pub exit_code: Option<i32>,
    /// The recorded input an exchange answers.
    pub input: Value,
    pub replies: Vec<Value>,
    pub hold: Option<Gate>,
    pub before_result: Option<Gate>,
    pub started: Gate,
    pub done: Gate,
}

impl Step {
    fn new(kind: Kind) -> Step {
        Step {
            kind,
            reply: Value::Null,
            exit_code: None,
            input: Value::Null,
            replies: Vec::new(),
            hold: None,
            before_result: None,
            started: Gate::default(),
            done: Gate::default(),
        }
    }

    pub fn hold(mut self, g: &Gate) -> Step {
        self.hold = Some(g.clone());
        self
    }

    pub fn before_result(mut self, g: &Gate) -> Step {
        self.before_result = Some(g.clone());
        self
    }

    /// The same step with fresh gates.
    pub fn again(&self) -> Step {
        Step {
            started: Gate::default(),
            done: Gate::default(),
            ..self.clone()
        }
    }

    pub fn is_exchange_of(&self, subtype: &str) -> bool {
        self.kind == Kind::Exchange && self.input["request"]["subtype"] == subtype
    }
}

/// Answers the start-up request with the reply from a recording.
pub fn startup(recording: &str) -> Step {
    let mut s = Step::new(Kind::Startup);
    s.reply = startup_reply(&load(recording));
    s
}

pub fn expect_startup() -> Step {
    startup("startup")
}

/// Replays one recorded user turn or interrupt, up to the next input.
fn recorded(recording: &str, kind: Kind, index: usize) -> Vec<Value> {
    let mut replies = Vec::new();
    let mut found = false;
    let mut seen = 0;
    for line in load(recording) {
        if line["from"] == "sdk" {
            if found {
                break;
            }
            let m = &line["message"];
            let matches = match kind {
                Kind::User => m["type"] == "user",
                _ => m["request"]["subtype"] == "interrupt",
            };
            if matches {
                if seen == index {
                    found = true;
                }
                seen += 1;
            }
            continue;
        }
        if found && line["from"] == "claude" {
            replies.push(line["message"].clone());
        }
    }
    assert!(found, "no {kind:?} {index} in {recording}");
    replies
}

pub fn turn(recording: &str, index: usize) -> Step {
    let mut s = Step::new(Kind::User);
    s.replies = recorded(recording, Kind::User, index);
    s
}

pub fn expect_turn() -> Step {
    turn("text", 0)
}

pub fn interrupt(recording: &str) -> Step {
    let mut s = Step::new(Kind::Interrupt);
    s.replies = recorded(recording, Kind::Interrupt, 0);
    s
}

pub fn expect_interrupt() -> Step {
    interrupt("text")
}

/// Every protocol exchange of a recording, Claude's own requests included.
pub fn replay(recording: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut current: Option<usize> = None;
    for line in load(recording) {
        if line["from"] == "sdk" {
            if line["message"]["request"]["subtype"] == "initialize" {
                let mut s = Step::new(Kind::Startup);
                s.reply = startup_reply(&load(recording));
                steps.push(s);
                current = None;
                continue;
            }
            let mut s = Step::new(Kind::Exchange);
            s.input = line["message"].clone();
            steps.push(s);
            current = Some(steps.len() - 1);
            continue;
        }
        if line["from"] == "claude" {
            if let Some(i) = current {
                steps[i].replies.push(line["message"].clone());
            }
        }
    }
    steps
}

// ---------------------------------------------------------------------------
// The fake

/// One `claude` CodeSmith started.
pub struct Run {
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: String,
    pub received: Mutex<Vec<Value>>,
    pub closed: Gate,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    exited: AtomicBool,
    request_ids: Mutex<HashMap<String, String>>,
}

impl Run {
    pub fn received(&self) -> Vec<Value> {
        self.received.lock().unwrap().clone()
    }

    pub fn users(&self) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|m| m["type"] == "user")
            .collect()
    }

    pub fn requests(&self, subtype: &str) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|m| m["request"]["subtype"] == subtype)
            .collect()
    }

    pub fn initialize(&self) -> Value {
        self.requests("initialize")
            .first()
            .map(|m| m["request"].clone())
            .unwrap_or(Value::Null)
    }

    /// The MCP results sent back.
    pub fn mcp_results(&self) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|m| !m["response"]["response"]["mcp_response"]["result"].is_null())
            .map(|m| m["response"]["response"]["mcp_response"]["result"].clone())
            .filter(|r| r.get("content").is_some())
            .collect()
    }

    pub fn arg(&self, prefix: &str) -> Option<String> {
        self.args
            .iter()
            .find_map(|a| a.strip_prefix(prefix).map(str::to_string))
    }

    pub fn session(&self) -> Option<String> {
        self.arg("--session-id=").or_else(|| self.arg("--resume="))
    }

    fn is_probe(&self) -> bool {
        !self.args.iter().any(|a| a == "--include-partial-messages")
    }

    async fn write(&self, line: &[u8]) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        let mut w = self.writer.lock().await;
        if let Some(w) = w.as_mut() {
            let _ = w.write_all(line).await;
        }
    }

    /// End the process with `code`.
    pub async fn exit(&self, code: i32) {
        if self.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut w = self.writer.lock().await;
        if let Some(mut w) = w.take() {
            let _ = w.write_all(format!("\0EXIT {code}\n").as_bytes()).await;
            let _ = w.shutdown().await;
        }
        drop(w);
        self.closed.wait().await;
    }
}

struct Inner {
    script: Mutex<Vec<Step>>,
    next: AtomicUsize,
    completed: AtomicUsize,
    errors: Mutex<Vec<String>>,
    runs: Mutex<Vec<Arc<Run>>>,
    probes: bool,
    config: PathBuf,
}

#[derive(Clone)]
pub struct Fake(Arc<Inner>);

/// The recorded no-prompt controls of the login check, by model.
fn probe_controls() -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let (mut model, mut control) = (None::<String>, None::<String>);
    for line in load("startup") {
        let m = &line["message"];
        if line["from"] == "sdk" {
            control = m["request"]["subtype"].as_str().map(str::to_string);
            if control.as_deref() == Some("set_model") {
                model = m["request"]["model"].as_str().map(str::to_string);
            }
            continue;
        }
        if line["from"] == "claude" && m["type"] == "control_response" {
            if let (Some(mo), Some(c)) = (&model, &control) {
                out.insert(format!("{mo}/{c}"), m.clone());
            }
        }
    }
    out
}

fn project_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Fake {
    /// A fake answering `script`; with `probes`, login checks are answered
    /// on their own, outside the script.
    pub fn new(script: Vec<Step>, probes: bool, config: &Path) -> Fake {
        Fake(Arc::new(Inner {
            script: Mutex::new(script),
            next: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            errors: Mutex::default(),
            runs: Mutex::default(),
            probes,
            config: config.to_path_buf(),
        }))
    }

    pub fn step(&self, i: usize) -> Step {
        self.0.script.lock().unwrap()[i].clone()
    }

    pub fn runs(&self) -> Vec<Arc<Run>> {
        self.0.runs.lock().unwrap().clone()
    }

    pub fn run(&self, i: usize) -> Arc<Run> {
        self.runs()
            .get(i)
            .cloned()
            .unwrap_or_else(|| panic!("no run {i}"))
    }

    /// Wait until `claude` number `i` started.
    pub async fn wait_run(&self, i: usize) -> Arc<Run> {
        for _ in 0..2000 {
            if let Some(r) = self.runs().get(i) {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("claude {i} never started")
    }

    /// Fails if a step did not finish, or CodeSmith wrote something the
    /// script did not expect.
    pub fn check(&self) -> Result<(), String> {
        let errors = self.0.errors.lock().unwrap();
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        let total = self.0.script.lock().unwrap().len();
        let done = self.0.completed.load(Ordering::SeqCst);
        if done < total {
            return Err(format!("{} script steps did not finish", total - done));
        }
        Ok(())
    }

    /// Claude Code's transcript folder.
    pub fn config(&self) -> &Path {
        &self.0.config
    }

    pub fn transcript(&self, session: &str) -> Option<PathBuf> {
        let projects = std::fs::read_dir(self.0.config.join("projects")).ok()?;
        projects
            .filter_map(Result::ok)
            .map(|e| e.path().join(format!("{session}.jsonl")))
            .find(|p| p.is_file())
    }

    pub fn delete_transcript(&self, session: &str) {
        if let Some(p) = self.transcript(session) {
            std::fs::remove_file(p).unwrap();
        }
    }

    /// Serve the CodeSmith side's `claude`s on `socket`.
    pub fn serve(&self, socket: &Path) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        let me = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let me = me.clone();
                tokio::spawn(async move { me.connection(stream).await });
            }
        })
    }

    async fn connection(&self, stream: UnixStream) {
        let (read, write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let Ok(Some(hello)) = lines.next_line().await else {
            return;
        };
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let run = Arc::new(Run {
            args: serde_json::from_value(hello["args"].clone()).unwrap(),
            env: serde_json::from_value(hello["env"].clone()).unwrap(),
            cwd: hello["cwd"].as_str().unwrap_or_default().to_string(),
            received: Mutex::default(),
            closed: Gate::default(),
            writer: tokio::sync::Mutex::new(Some(write)),
            exited: AtomicBool::new(false),
            request_ids: Mutex::default(),
        });
        self.0.runs.lock().unwrap().push(run.clone());
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            run.received.lock().unwrap().push(message.clone());
            if let Some(task) = self.answer(&run, message) {
                tokio::spawn(task);
            }
        }
        // Input closed (CodeSmith stops `claude` this way) or the process
        // died: it exits.
        let r = run.clone();
        let w = tokio::spawn(async move {
            if !r.exited.swap(true, Ordering::SeqCst) {
                if let Some(mut w) = r.writer.lock().await.take() {
                    let _ = w.write_all(b"\0EXIT 0\n").await;
                    let _ = w.shutdown().await;
                }
            }
        });
        let _ = w.await;
        run.closed.open();
    }

    fn error(&self, e: String) {
        self.0.errors.lock().unwrap().push(e);
    }

    /// Match `message` to the next step now, in arrival order, and return
    /// the rest of the answer to run.
    fn answer(
        &self,
        run: &Arc<Run>,
        message: Value,
    ) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        let kind = if message["type"] == "user" {
            "user".to_string()
        } else if message["request"]["subtype"] == "initialize" {
            "startup".to_string()
        } else {
            message["request"]["subtype"]
                .as_str()
                .or(message["type"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        if self.0.probes && run.is_probe() && kind == "startup" {
            let reply = startup_reply(&load("startup"));
            let line = startup_response(&message["request_id"], &reply);
            let run = run.clone();
            return Some(boxed(async move { run.write(&line).await }));
        }
        let step = {
            let script = self.0.script.lock().unwrap();
            let next = self.0.next.load(Ordering::SeqCst);
            script.get(next).cloned()
        };
        let matches = match &step {
            Some(s) if s.kind == Kind::Exchange => {
                message["type"] == s.input["type"]
                    && message["request"]["subtype"] == s.input["request"]["subtype"]
                    && message["response"]["request_id"] == s.input["response"]["request_id"]
            }
            Some(s) => {
                let want = match s.kind {
                    Kind::Startup => "startup",
                    Kind::User => "user",
                    Kind::Interrupt => "interrupt",
                    Kind::Exchange => unreachable!(),
                };
                kind == want
            }
            None => false,
        };
        if !matches && run.is_probe() {
            let model = run
                .received()
                .iter()
                .rev()
                .find(|m| m["request"]["subtype"] == "set_model")
                .and_then(|m| m["request"]["model"].as_str().map(str::to_string));
            if let Some(model) = model {
                if let Some(reply) = probe_controls().get(&format!("{model}/{kind}")) {
                    let mut reply = reply.clone();
                    reply["response"]["request_id"] = message["request_id"].clone();
                    let line = line(&reply);
                    let run = run.clone();
                    return Some(boxed(async move { run.write(&line).await }));
                }
            }
        }
        let Some(step) = step.filter(|_| matches) else {
            let next = self
                .0
                .script
                .lock()
                .unwrap()
                .get(self.0.next.load(Ordering::SeqCst))
                .map(|s| format!("{:?}", s.kind))
                .unwrap_or("the end of the script".into());
            self.error(format!("unexpected {kind}, the next step is {next}"));
            if std::env::var_os("FAKE_CLAUDE_DEBUG").is_some() {
                eprintln!(
                    "unexpected (next {next}, runs {}): {}",
                    self.runs().len(),
                    &message.to_string()[..200.min(message.to_string().len())]
                );
            }
            return None;
        };
        if step.kind == Kind::Exchange && message["type"] == "control_request" {
            if let Some(recorded) = step.input["request_id"].as_str() {
                run.request_ids.lock().unwrap().insert(
                    recorded.to_string(),
                    message["request_id"].as_str().unwrap_or_default().into(),
                );
            }
        }
        self.0.next.fetch_add(1, Ordering::SeqCst);
        if message["type"] == "user" {
            self.remember(run, &message);
        }
        step.started.open();
        let me = self.clone();
        let run = run.clone();
        Some(boxed(async move {
            if let Some(h) = &step.hold {
                tokio::select! { _ = h.wait() => {}, _ = run.closed.wait() => {} }
            }
            if run.exited.load(Ordering::SeqCst) {
                return;
            }
            if step.kind == Kind::Startup {
                match step.exit_code {
                    Some(code) => run.exit(code).await,
                    None => {
                        run.write(&startup_response(&message["request_id"], &step.reply))
                            .await
                    }
                }
            } else {
                let initialize = run.initialize();
                // Replies go out in one write (up to a held result), so a
                // step is finished by the time CodeSmith sees its answer.
                let mut batch = Vec::new();
                for recorded in &step.replies {
                    let mut reply = recorded.clone();
                    if reply["type"] == "result" {
                        if let Some(g) = &step.before_result {
                            run.write(&std::mem::take(&mut batch)).await;
                            tokio::select! { _ = g.wait() => {}, _ = run.closed.wait() => {} }
                        }
                    }
                    if run.exited.load(Ordering::SeqCst) {
                        return;
                    }
                    if reply["type"] == "control_response" {
                        if step.kind == Kind::Exchange {
                            let recorded = reply["response"]["request_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            if let Some(id) = run.request_ids.lock().unwrap().get(&recorded) {
                                reply["response"]["request_id"] = json!(id);
                            }
                        } else {
                            reply["response"]["request_id"] = message["request_id"].clone();
                        }
                    }
                    connect_servers(&mut reply, &initialize);
                    if reply["type"] == "control_request"
                        && reply["request"]["subtype"] == "hook_callback"
                    {
                        let event = reply["request"]["input"]["hook_event_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        reply["request"]["callback_id"] =
                            initialize["hooks"][&event][0]["hookCallbackIds"][0].clone();
                    }
                    if let Some(id) = run.session() {
                        if reply.get("session_id").is_some() {
                            reply["session_id"] = json!(id);
                        }
                    }
                    me.remember(&run, &reply);
                    batch.extend(line(&reply));
                }
                me.0.completed.fetch_add(1, Ordering::SeqCst);
                step.done.open();
                run.write(&batch).await;
                return;
            }
            me.0.completed.fetch_add(1, Ordering::SeqCst);
            step.done.open();
        }))
    }

    /// Keep a user or assistant message in the run's transcript, as
    /// Claude Code does.
    fn remember(&self, run: &Run, message: &Value) {
        if run.args.iter().any(|a| a == "--no-session-persistence")
            || !message["uuid"].is_string()
            || !(message["type"] == "user" || message["type"] == "assistant")
        {
            return;
        }
        let Some(id) = run.session() else {
            return;
        };
        let path = self.transcript(&id).unwrap_or_else(|| {
            self.0
                .config
                .join("projects")
                .join(project_name(&run.cwd))
                .join(format!("{id}.jsonl"))
        });
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entries: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if entries.iter().any(|e| e["uuid"] == message["uuid"]) {
            return;
        }
        let parent = entries
            .last()
            .map(|e| e["uuid"].clone())
            .unwrap_or(Value::Null);
        let entry = json!({
            "type": message["type"],
            "uuid": message["uuid"],
            "parentUuid": parent,
            "sessionId": id,
            "message": message["message"],
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = text;
        text.push_str(&entry.to_string());
        text.push('\n');
        std::fs::write(&path, text).unwrap();
    }
}

fn boxed(
    f: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(f)
}

fn line(v: &Value) -> Vec<u8> {
    let mut l = serde_json::to_vec(v).unwrap();
    l.push(b'\n');
    l
}

fn startup_response(id: &Value, reply: &Value) -> Vec<u8> {
    line(&json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": id, "response": reply},
    }))
}

/// Like `claude`, report the offered SDK servers as connected when the
/// recording was made without tools.
fn connect_servers(message: &mut Value, initialize: &Value) {
    if message["type"] != "system" || message["subtype"] != "init" {
        return;
    }
    if message["mcp_servers"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        return;
    }
    let Some(servers) = initialize["sdkMcpServers"].as_array() else {
        return;
    };
    let list: Vec<Value> = servers
        .iter()
        .map(|n| json!({"name": n, "status": "connected", "source": "sdk"}))
        .collect();
    message["mcp_servers"] = Value::Array(list);
}
