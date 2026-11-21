//! More ports of OpenCode's session runner tests
//! (`packages/core/test/session-runner.test.ts`, MIT) for behaviour
//! `runner_parity.rs` does not pin yet, against the same kind of scripted
//! OpenAI-compatible model. Each test names the OpenCode scenario it ports;
//! see `crates/codesmith-core/PARITY.md`.

// The harness is the one in `runner_parity.rs`; not every helper is used here.
#![allow(dead_code)]

use codesmith_ai::{Part, Role};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- The scripted model ---

/// Replies in order (raw HTTP responses); then a plain "ok".
#[derive(Default)]
struct Script {
    replies: VecDeque<String>,
    /// Every request body except title requests.
    log: Vec<Value>,
}

type Shared = Arc<Mutex<Script>>;

async fn read_body(s: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = s.read(&mut tmp).await.unwrap();
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

async fn model(script: Shared) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let script = script.clone();
            tokio::spawn(async move {
                let body = read_body(&mut s).await;
                let resp = {
                    let mut sc = script.lock().unwrap();
                    sc.log.push(body);
                    sc.replies.pop_front().unwrap_or_else(|| text("ok"))
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn http(status: u16, content_type: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// An OpenAI chat stream of these chunks.
fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    http(200, "text/event-stream", &body)
}

fn finish(reason: &str, content: &str) -> String {
    sse(&[json!({"choices": [{"delta": {"content": content}, "finish_reason": reason}]})])
}

fn text(t: &str) -> String {
    finish("stop", t)
}

/// Tool calls as (id, name, raw JSON arguments).
fn calls_with(reason: &str, calls: &[(&str, &str, &str)]) -> String {
    let tc: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (id, name, args))| json!({"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": args}}))
        .collect();
    sse(&[json!({"choices": [{"delta": {"tool_calls": tc}, "finish_reason": reason}]})])
}

fn calls(calls: &[(&str, &str, &str)]) -> String {
    calls_with("tool_calls", calls)
}

fn call(id: &str, name: &str, args: Value) -> String {
    calls(&[(id, name, &args.to_string())])
}

fn status(code: u16, body: Value) -> String {
    http(code, "application/json", &body.to_string())
}

// --- Sessions ---

#[derive(Default)]
struct Seen {
    retries: Vec<(u32, String)>,
    notices: Vec<String>,
    tools: Vec<(String, String, bool)>,
    text: String,
    /// Tool lifecycle: `input id name`, `formed id call`, `dropped id`,
    /// `start id`.
    lifecycle: Vec<String>,
    /// `tool_progress` and `tool_output` reports: (call id, what).
    progress: Vec<(String, String)>,
}

struct Rec(Arc<Mutex<Seen>>);
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

/// Snapshots live under the data dir; keep them out of the real one.
fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // A new folder with a random name, created exclusively (0700),
        // not a predictable one another local user could create first.
        let dir = tempfile::Builder::new()
            .prefix("codesmith-runner-parity-")
            .tempdir()
            .unwrap()
            .keep();
        std::env::set_var("XDG_DATA_HOME", dir);
    });
}

struct T {
    s: Session,
    script: Shared,
    seen: Arc<Mutex<Seen>>,
    dir: tempfile::TempDir,
    /// The session store lives outside the project, as it does for real.
    data: tempfile::TempDir,
}

impl T {
    fn push(&self, replies: impl IntoIterator<Item = String>) {
        self.script.lock().unwrap().replies.extend(replies);
    }
    fn requests(&self) -> Vec<Value> {
        self.script.lock().unwrap().log.clone()
    }
    async fn prompt(&mut self, text: &str) -> anyhow::Result<()> {
        let mut sink = Rec(self.seen.clone());
        self.s.prompt(text, &mut sink).await
    }
    fn path(&self, p: &str) -> std::path::PathBuf {
        self.dir.path().join(p)
    }
    fn store(&self) -> Store {
        Store::open(&self.data.path().join("sessions.db")).unwrap()
    }
}

fn config(base: &str, extra: Value) -> Config {
    config_with(base, "openai-chat", json!({}), extra)
}

fn config_with(base: &str, protocol: &str, models: Value, extra: Value) -> Config {
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

struct Setup {
    extra: Value,
    protocol: &'static str,
    models: Value,
    reply: Reply,
    store: bool,
    agent: Option<&'static str>,
    questioner: Option<Arc<dyn codesmith_core::question::Questioner>>,
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
        }
    }
}

async fn start(setup: Setup) -> T {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let loaded = Loaded {
        config: config_with(&base, setup.protocol, setup.models, setup.extra),
        files: vec![],
        root: root.clone(),
    };
    let data = tempfile::tempdir().unwrap();
    let store = setup
        .store
        .then(|| Store::open(&data.path().join("sessions.db")).unwrap());
    let opts = Options {
        agent: setup.agent.map(String::from),
        model: None,
        resume: None,
        title: Some("Test".into()),
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
    }
}

async fn plain() -> T {
    start(Setup::default()).await
}

fn roles(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect()
}

fn last_message(body: &Value) -> &Value {
    body["messages"].as_array().unwrap().last().unwrap()
}

fn tool_names(body: &Value) -> Vec<String> {
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
fn results(s: &Session) -> Vec<(String, String, bool)> {
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

fn texts(s: &Session) -> Vec<String> {
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
fn cut(chunks: &[Value]) -> String {
    sse(chunks)
}

/// The text of the user messages a request sends, in order.
fn user_texts(body: &Value) -> Vec<String> {
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

fn assistant_texts_sent(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["content"].to_string())
        .collect()
}

// --- Retries before output ---

/// "retries an unknown finish before output". Fixed: an unknown finish
/// ended the turn as if the model had stopped.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_an_unknown_finish_before_output() {
    let mut t = plain().await;
    t.push([finish("unknown", ""), text("Recovered")]);
    t.prompt("Retry unknown finish").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    assert_eq!(t.seen.lock().unwrap().retries.len(), 1);
    assert_eq!(texts(&t.s), ["Recovered"]);
}

/// "retries an unknown finish cleanly after observable text when no tool
/// ran". Fixed as above.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_an_unknown_finish_cleanly_after_text() {
    let mut t = plain().await;
    t.push([finish("unknown", "Partial"), text("Recovered")]);
    t.prompt("Continue unknown finish").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(user_texts(&reqs[1]), ["Continue unknown finish"]);
    assert!(assistant_texts_sent(&reqs[1]).is_empty(), "{}", reqs[1]);
    assert_eq!(texts(&t.s), ["Recovered"]);
}

/// Not from OpenCode: a tool call with an unknown finish still runs.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_finish_with_a_tool_call_runs_the_tool() {
    let mut t = plain().await;
    std::fs::write(t.path("a.txt"), "x").unwrap();
    t.push([
        calls_with("unknown", &[("c1", "read", r#"{"path":"a.txt"}"#)]),
        text("Done"),
    ]);
    t.prompt("Read it").await.unwrap();
    assert_eq!(results(&t.s).len(), 1);
    assert_eq!(t.requests().len(), 2);
}

/// "retries an incomplete stream cleanly from the prompt when no tool ran"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_an_incomplete_stream_cleanly_from_the_prompt() {
    let mut t = plain().await;
    t.push([
        cut(&[json!({"choices": [{"delta": {"content": "Partial"}}]})]),
        text("Recovered"),
    ]);
    t.prompt("Continue partial output").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(user_texts(&reqs[1]), ["Continue partial output"]);
    assert!(assistant_texts_sent(&reqs[1]).is_empty(), "{}", reqs[1]);
    assert_eq!(texts(&t.s).last().unwrap(), "Recovered");
}

/// "retries after an unrecognized mid-stream provider failure when no tool ran"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_after_an_unrecognized_mid_stream_failure() {
    let mut t = plain().await;
    let broken = http(
        200,
        "text/event-stream",
        &format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices": [{"delta": {"content": "Partial"}}]}),
            json!({"error": {"message": "Provider returned error"}})
        ),
    );
    t.push([broken, text("Recovered")]);
    t.prompt("Continue after unknown failure").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(user_texts(&reqs[1]), ["Continue after unknown failure"]);
    assert_eq!(texts(&t.s).last().unwrap(), "Recovered");
}

/// "drops interrupted reasoning and retries cleanly without continuation prompt"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_drops_interrupted_reasoning_and_retries_cleanly() {
    let mut t = plain().await;
    t.push([
        cut(&[json!({"choices": [{"delta": {"reasoning_content": "Partial thought"}}]})]),
        text("Recovered"),
    ]);
    t.prompt("Continue interrupted reasoning").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(user_texts(&reqs[1]), ["Continue interrupted reasoning"]);
    assert!(!reqs[1].to_string().contains("Partial thought"));
    assert_eq!(texts(&t.s).last().unwrap(), "Recovered");
}

/// "stops incomplete stream retries once the retry allowance is spent"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_stops_incomplete_stream_retries_once_the_allowance_is_spent() {
    let mut t = plain().await;
    t.push((0..12).map(|_| cut(&[json!({"choices": [{"delta": {"content": "Partial"}}]})])));
    assert!(t.prompt("Exhaust partial continuations").await.is_err());
    assert_eq!(t.requests().len(), 11, "one attempt and ten retries");
    t.push([text("Continued after exhaustion")]);
    t.prompt("Continue please").await.unwrap();
    let last = t.requests().last().unwrap().clone();
    assert_eq!(
        user_texts(&last),
        ["Exhaust partial continuations", "Continue please"]
    );
}

// --- Provider failures ---

/// "projects provider errors as terminal assistant step failures"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_projects_provider_errors_as_terminal_failures() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([status(
        400,
        json!({"error": {"message": "Provider unavailable", "type": "invalid_request_error"}}),
    )]);
    let err = t.prompt("Fail durably").await.unwrap_err();
    assert!(err.to_string().contains("Provider unavailable"), "{err}");
    assert_eq!(t.requests().len(), 1);
    // The prompt stays in the session, so the next turn starts after it.
    t.push([text("ok")]);
    t.prompt("Again").await.unwrap();
    assert_eq!(
        user_texts(t.requests().last().unwrap()),
        ["Fail durably", "Again"]
    );
}

/// "does not recover context overflow after durable assistant output".
/// Fixed: the overflow was compacted and retried after partial output.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_recover_context_overflow_after_output() {
    let mut t = plain().await;
    let broken = http(
        200,
        "text/event-stream",
        &format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices": [{"delta": {"content": "Partial"}}]}),
            json!({"error": {"code": "context_length_exceeded", "message": "prompt too long"}})
        ),
    );
    t.push([broken]);
    let err = t.prompt("Fail after output").await.unwrap_err();
    assert!(err.to_string().contains("prompt too long"), "{err}");
    assert_eq!(t.requests().len(), 1);
    assert_eq!(texts(&t.s), ["Partial"]);
}

/// "settles a local tool before one content-filter step failure"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_settles_a_local_tool_before_a_content_filter_failure() {
    let mut t = plain().await;
    std::fs::write(t.path("a.txt"), "settled").unwrap();
    t.push([calls_with(
        "content_filter",
        &[("c1", "read", r#"{"path":"a.txt"}"#)],
    )]);
    let err = t.prompt("Tool before blocked response").await.unwrap_err();
    assert_eq!(err.to_string(), "Provider blocked the response");
    assert_eq!(t.requests().len(), 1);
}

/// "returns unexpected local tool defects to the model and continues"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_returns_tool_failures_to_the_model_and_continues() {
    let mut t = plain().await;
    t.push([
        call("c1", "read", json!({"path": "missing.txt"})),
        text("Handled"),
    ]);
    t.prompt("Use the tool").await.unwrap();
    let r = results(&t.s);
    assert_eq!(r.len(), 1);
    assert!(r[0].2, "a failed call: {:?}", r[0]);
    assert_eq!(t.requests().len(), 2);
    assert_eq!(texts(&t.s).last().unwrap(), "Handled");
}

/// "does not continue automatically after a provider error follows a local tool call"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_continue_after_a_provider_error_follows_a_tool_call() {
    let mut t = plain().await;
    std::fs::write(t.path("a.txt"), "x").unwrap();
    t.push([
        call("c1", "read", json!({"path": "a.txt"})),
        status(
            400,
            json!({"error": {"message": "bad request", "type": "invalid_request_error"}}),
        ),
    ]);
    assert!(t.prompt("Read then fail").await.is_err());
    assert_eq!(t.requests().len(), 2);
    assert_eq!(results(&t.s).len(), 1);
}

// --- Agent selection ---

/// "fails before the model request when the selected agent is unavailable"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_fails_before_the_model_request_when_the_agent_is_unavailable() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let loaded = Loaded {
        config: config(&base, json!({})),
        files: vec![],
        root: root.clone(),
    };
    let opts = Options {
        agent: Some("missing".into()),
        model: None,
        resume: None,
        title: Some("Test".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    assert!(Session::new(&loaded, root, opts, Arc::new(Fixed(Reply::Once)), None).is_err());
    assert!(script.lock().unwrap().log.is_empty());
}
