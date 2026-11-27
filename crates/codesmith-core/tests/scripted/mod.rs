//! The scripted OpenAI-compatible model and session harness shared by the
//! backlog ports (`tests/oc_*_backlog.rs`): the one `oc_runner.rs` uses,
//! with the user's home, config and data kept out.
#![allow(dead_code)]

use codesmith_ai::{Part, Role};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- The scripted model ---

/// Replies in order (raw HTTP responses); then a plain "ok". Title
/// requests are answered from `titles` (then "Generated Title") and logged
/// apart. A reply of [`HOLD`] waits for [`Script::gate`] to be released,
/// then answers with the next reply.
pub struct Script {
    pub replies: VecDeque<String>,
    /// Every request body except title requests.
    pub log: Vec<Value>,
    pub titles: VecDeque<String>,
    pub title_log: Vec<Value>,
    pub gate: Arc<tokio::sync::Semaphore>,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            replies: VecDeque::new(),
            log: Vec::new(),
            titles: VecDeque::new(),
            title_log: Vec::new(),
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}

/// A reply that holds the response until the gate opens.
pub const HOLD: &str = "\0HOLD";

pub type Shared = Arc<Mutex<Script>>;

pub async fn read_body(s: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return Value::Null;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = text.find("\r\n\r\n") {
            let len: usize = text[..i]
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                return serde_json::from_slice(&buf[i + 4..i + 4 + len]).unwrap();
            }
        }
    }
}

pub async fn model(script: Shared) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let script = script.clone();
            tokio::spawn(async move {
                let body = read_body(&mut s).await;
                let is_title =
                    body["messages"][0]["content"].as_str() == Some(codesmith_core::title::SYSTEM);
                let (mut resp, gate) = {
                    let mut sc = script.lock().unwrap();
                    if is_title {
                        sc.title_log.push(body);
                        let t = sc.titles.pop_front();
                        (
                            text(t.as_deref().unwrap_or("Generated Title")),
                            sc.gate.clone(),
                        )
                    } else {
                        sc.log.push(body);
                        let r = sc.replies.pop_front().unwrap_or_else(|| text("ok"));
                        (r, sc.gate.clone())
                    }
                };
                if let Some((before, after)) = resp.clone().split_once(HOLD) {
                    let _ = s.write_all(before.as_bytes()).await;
                    let _ = s.flush().await;
                    // Bounded: a test that never opens the gate still ends.
                    if let Ok(Ok(p)) =
                        tokio::time::timeout(Duration::from_secs(60), gate.acquire()).await
                    {
                        p.forget();
                    }
                    resp = if before.is_empty() && after.is_empty() {
                        script
                            .lock()
                            .unwrap()
                            .replies
                            .pop_front()
                            .unwrap_or_else(|| text("ok"))
                    } else {
                        after.to_string()
                    };
                }
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

pub fn http(status: u16, content_type: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// A chat stream that sends `before`, then waits for the gate before
/// sending `after` and the end of the stream.
pub fn held(before: &[Value], after: &[Value]) -> String {
    let a: String = before.iter().map(|c| format!("data: {c}\n\n")).collect();
    let mut b: String = after.iter().map(|c| format!("data: {c}\n\n")).collect();
    b.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{a}{HOLD}{b}",
        a.len() + b.len()
    )
}

/// An OpenAI chat stream of these chunks.
pub fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    http(200, "text/event-stream", &body)
}

pub fn finish(reason: &str, content: &str) -> String {
    sse(&[json!({"choices": [{"delta": {"content": content}, "finish_reason": reason}]})])
}

pub fn text(t: &str) -> String {
    finish("stop", t)
}

/// Tool calls as (id, name, raw JSON arguments).
pub fn calls_with(reason: &str, calls: &[(&str, &str, &str)]) -> String {
    let tc: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (id, name, args))| json!({"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": args}}))
        .collect();
    sse(&[json!({"choices": [{"delta": {"tool_calls": tc}, "finish_reason": reason}]})])
}

pub fn calls(calls: &[(&str, &str, &str)]) -> String {
    calls_with("tool_calls", calls)
}

pub fn call(id: &str, name: &str, args: Value) -> String {
    calls(&[(id, name, &args.to_string())])
}

pub fn status(code: u16, body: Value) -> String {
    http(code, "application/json", &body.to_string())
}

// --- Sessions ---

#[derive(Default)]
pub struct Seen {
    pub retries: Vec<(u32, String)>,
    pub notices: Vec<String>,
    pub tools: Vec<(String, String, bool)>,
    pub text: String,
    /// Tool lifecycle: `input id name`, `formed id call`, `dropped id`,
    /// `start id`.
    pub lifecycle: Vec<String>,
    /// `tool_progress` and `tool_output` reports: (call id, what).
    pub progress: Vec<(String, String)>,
}

pub struct Rec(pub Arc<Mutex<Seen>>);
impl Sink for Rec {
    fn text(&mut self, d: &str) {
        self.0.lock().unwrap().text.push_str(d);
    }
    fn retry(&mut self, attempt: u32, error: &str, _: Duration) {
        self.0.lock().unwrap().retries.push((attempt, error.into()));
    }
    fn notice(&mut self, m: &str) {
        self.0.lock().unwrap().notices.push(m.into());
    }
    fn tool_input(&mut self, id: &str, name: &str) {
        let mut s = self.0.lock().unwrap();
        s.lifecycle.push(format!("input {id} {name}"));
    }
    fn tool_input_end(&mut self, id: &str, call: Option<&str>) {
        let mut s = self.0.lock().unwrap();
        s.lifecycle.push(match call {
            Some(c) => format!("formed {id} {c}"),
            None => format!("dropped {id}"),
        });
    }
    fn tool_start(&mut self, id: &str, _: &str, _: &Value) {
        let mut s = self.0.lock().unwrap();
        s.lifecycle.push(format!("start {id}"));
    }
    fn tool_end(&mut self, _: &str, name: &str, output: &str, is_error: bool) {
        self.0
            .lock()
            .unwrap()
            .tools
            .push((name.into(), output.into(), is_error));
    }
    fn tool_progress(&mut self, id: &str, metadata: &Value) {
        let mut s = self.0.lock().unwrap();
        s.progress.push((id.into(), metadata.to_string()));
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        let mut s = self.0.lock().unwrap();
        s.progress.push((id.into(), delta.into()));
    }
}

/// Keep the user's own config, data and skills out of the tests.
pub fn isolate() -> std::path::PathBuf {
    // A new folder with a random name, created exclusively (0700): a
    // name made from the pid in the shared temporary directory could be
    // created or linked first by another local user.
    static BASE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let base = tempfile::Builder::new()
            .prefix("codesmith-oc-backlog-")
            .tempdir()
            .unwrap()
            .keep();
        for d in ["home", "config", "data", "xdg-config"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::env::set_var("HOME", base.join("home"));
        std::env::set_var("CODESMITH_CONFIG_DIR", base.join("config"));
        std::env::set_var("XDG_CONFIG_HOME", base.join("xdg-config"));
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        base
    })
    .clone()
}

pub struct T {
    pub s: Session,
    pub script: Shared,
    pub seen: Arc<Mutex<Seen>>,
    pub dir: tempfile::TempDir,
    /// The session store lives outside the project, as it does for real.
    pub data: tempfile::TempDir,
    /// The scripted model's base URL.
    pub base: String,
    /// What the session was configured with.
    pub protocol: &'static str,
    pub models: Value,
    pub extra: Value,
}

impl T {
    pub fn push(&self, replies: impl IntoIterator<Item = String>) {
        self.script.lock().unwrap().replies.extend(replies);
    }
    pub fn requests(&self) -> Vec<Value> {
        self.script.lock().unwrap().log.clone()
    }
    pub async fn prompt(&mut self, text: &str) -> anyhow::Result<()> {
        let mut sink = Rec(self.seen.clone());
        self.s.prompt(text, &mut sink).await
    }
    pub fn path(&self, p: &str) -> std::path::PathBuf {
        self.dir.path().join(p)
    }
    pub fn store(&self) -> Store {
        Store::open(&self.data.path().join("sessions.db")).unwrap()
    }
    /// The loaded config the session started with.
    pub fn loaded(&self) -> Loaded {
        Loaded {
            config: config_with(
                &self.base,
                self.protocol,
                self.models.clone(),
                self.extra.clone(),
            ),
            files: vec![],
            root: self.dir.path().canonicalize().unwrap(),
            ..Default::default()
        }
    }
    /// The stored session, opened again (as after a restart), against the
    /// same scripted model.
    pub fn reopen(&self) -> Session {
        let loaded = self.loaded();
        let opts = Options {
            agent: None,
            model: None,
            resume: Some(self.s.id.clone()),
            title: None,
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        };
        let root = loaded.root.clone();
        let mut s = Session::new(
            &loaded,
            root,
            opts,
            Arc::new(Fixed(Reply::Once)),
            Some(self.store()),
        )
        .unwrap();
        s.set_retry_scale(0.0001);
        s
    }
}

pub fn config(base: &str, extra: Value) -> Config {
    config_with(base, "openai-chat", json!({}), extra)
}

pub fn config_with(base: &str, protocol: &str, models: Value, extra: Value) -> Config {
    let mut v = json!({
        "model": "fake/m", "snapshots": false,
        // A hosted provider, as in OpenCode, though it listens on loopback.
        "providers": {"fake": {"protocol": protocol, "local": false, "settings": {"baseURL": base, "apiKey": "x"}, "models": models}},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v).unwrap()
}

pub struct Setup {
    pub extra: Value,
    pub protocol: &'static str,
    pub models: Value,
    pub reply: Reply,
    pub store: bool,
    pub agent: Option<&'static str>,
    pub questioner: Option<Arc<dyn codesmith_core::question::Questioner>>,
    /// The session's title (`None`: one is generated).
    pub title: Option<String>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            extra: json!({}),
            protocol: "openai-chat",
            models: json!({}),
            reply: Reply::Once,
            store: false,
            agent: None,
            questioner: None,
            title: Some("Test".into()),
        }
    }
}

pub async fn start(setup: Setup) -> T {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let loaded = Loaded {
        config: config_with(
            &base,
            setup.protocol,
            setup.models.clone(),
            setup.extra.clone(),
        ),
        files: vec![],
        root: root.clone(),
        ..Default::default()
    };
    let data = tempfile::tempdir().unwrap();
    let store = setup
        .store
        .then(|| Store::open(&data.path().join("sessions.db")).unwrap());
    let opts = Options {
        agent: setup.agent.map(String::from),
        model: None,
        resume: None,
        title: setup.title,
        parent: None,
        id: None,
        questioner: setup.questioner,
        mcp: None,
    };
    let mut s = Session::new(&loaded, root, opts, Arc::new(Fixed(setup.reply)), store).unwrap();
    s.set_retry_scale(0.0001);
    T {
        s,
        script,
        seen: Arc::default(),
        dir,
        data,
        base,
        protocol: setup.protocol,
        models: setup.models,
        extra: setup.extra,
    }
}

pub async fn plain() -> T {
    start(Setup::default()).await
}

pub fn roles(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect()
}

pub fn last_message(body: &Value) -> &Value {
    body["messages"].as_array().unwrap().last().unwrap()
}

pub fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Every tool result in the session history, in order.
pub fn results(s: &Session) -> Vec<(String, String, bool)> {
    s.messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::ToolResult {
                id,
                content,
                is_error,
                ..
            } => Some((id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

pub fn texts(s: &Session) -> Vec<String> {
    s.messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.to_string()),
            _ => None,
        })
        .collect()
}

/// A chat stream that ends (with `[DONE]`) before any finish reason.
pub fn cut(chunks: &[Value]) -> String {
    sse(chunks)
}

/// The text of the user messages a request sends, in order.
pub fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

pub fn assistant_texts_sent(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["content"].to_string())
        .collect()
}
