//! The agent loop pinned to OpenCode's session runner: ports of
//! `packages/core/test/session-runner*.test.ts`, `session-revert.test.ts`,
//! `tool-subagent.test.ts` and the subagent report tests, run against a
//! scripted OpenAI-compatible (or Anthropic) model on a raw TCP socket.
//!
//! The pure rules (retry classification and schedule, history repair, the
//! texts the model sees) are tested in `retry.rs`, `runner.rs` and
//! `tool/subagent.rs`; their module headers list the deliberate differences.

use codesmith_ai::{Message, Part, Role, Usage};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{self, Options, RepeatGuard, Session, Sink};
use codesmith_core::store::{self, Info, Record, Store};
use codesmith_core::{runner, snapshot::Snapshots};
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
        let dir =
            std::env::temp_dir().join(format!("codesmith-runner-parity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        "providers": {"fake": {"protocol": protocol, "settings": {"baseURL": base, "apiKey": "x"}, "models": models}},
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

// --- Tool failures and permissions (session-runner.test.ts) ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_durably_settles_local_tool_failures_before_continuing() {
    let mut t = plain().await;
    t.push([
        call("call-missing", "missing", json!({})),
        text("Recovered"),
    ]);
    t.prompt("Call missing").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    assert_eq!(
        results(&t.s),
        [(
            "call-missing".into(),
            "No tool named \"missing\" is currently available. Please use a tool from the available tool list.".into(),
            true
        )]
    );
    assert_eq!(roles(&t.requests()[1])[1..], ["user", "assistant", "tool"]);
    assert_eq!(texts(&t.s), ["Recovered"]);
}

/// Fixed: a tool the agent was not offered used to run anyway.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_tools_the_agent_was_not_offered_are_unavailable() {
    let mut t = start(Setup {
        agent: Some("plan"),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "c1",
            "write",
            json!({"filePath": "out.txt", "content": "nope"}),
        ),
        text("ok"),
    ]);
    t.prompt("Write a file").await.unwrap();
    assert!(!tool_names(&t.requests()[0]).contains(&"write".to_string()));
    assert!(!t.path("out.txt").exists());
    assert_eq!(results(&t.s)[0].1, runner::unavailable("write"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_returns_permission_corrections_to_the_model_and_continues() {
    let mut t = start(Setup {
        extra: json!({"permissions": [{"action": "glob", "resource": "*", "effect": "ask"}]}),
        reply: Reply::Reject(Some("Use another tool".into())),
        ..Default::default()
    })
    .await;
    t.push([
        call("call-corrected", "glob", json!({"pattern": "*"})),
        text("done"),
    ]);
    t.prompt("Call corrected").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    let r = &results(&t.s)[0];
    assert!(r.2 && r.1.contains("Use another tool"), "{r:?}");
    assert!(t.s.stopped().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_returns_configured_permission_denials_to_the_model_and_continues() {
    let mut t = start(Setup {
        extra: json!({"permissions": [{"action": "shell", "resource": "rm *", "effect": "deny"}]}),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-permission",
            "shell",
            json!({"command": "rm -rf build", "description": "clean"}),
        ),
        text("done"),
    ]);
    t.prompt("Reject permission").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    let r = &results(&t.s)[0];
    assert!(r.2 && r.1.contains("Permission denied"), "{r:?}");
    assert!(t.s.stopped().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_interrupts_runner_continuation_on_a_decline_after_settling_an_ordinary_tool_error(
) {
    let mut t = start(Setup {
        extra: json!({"permissions": [{"action": "glob", "resource": "*", "effect": "ask"}]}),
        reply: Reply::Reject(None),
        ..Default::default()
    })
    .await;
    t.push([calls(&[
        ("call-failed", "missing", "{}"),
        ("call-declined", "glob", r#"{"pattern":"*"}"#),
        ("call-after", "glob", r#"{"pattern":"*.rs"}"#),
    ])]);
    t.prompt("Call declined").await.unwrap();
    assert_eq!(t.requests().len(), 1, "a decline ends the turn");
    let r = results(&t.s);
    assert_eq!(r[0].1, runner::unavailable("missing"));
    assert_eq!((r[1].1.as_str(), r[1].2), (runner::DECLINED, true));
    assert_eq!((r[2].1.as_str(), r[2].2), (runner::TOOLS_INTERRUPTED, true));
    assert!(t.s.stopped().is_some());
    assert!(t
        .seen
        .lock()
        .unwrap()
        .notices
        .iter()
        .any(|n| n.contains("declined")));
    // The next prompt continues normally with every call answered.
    t.push([text("again")]);
    t.prompt("Next").await.unwrap();
    assert!(t.s.stopped().is_none());
    assert_eq!(
        roles(&t.requests()[1])[1..],
        ["user", "assistant", "tool", "tool", "tool", "user"]
    );
}

struct Dismiss;
impl codesmith_core::question::Questioner for Dismiss {
    fn ask(&self, _: &[codesmith_core::question::Prompt]) -> Option<Vec<Vec<String>>> {
        None
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_interrupts_runner_continuation_when_a_question_is_cancelled() {
    let mut t = start(Setup {
        questioner: Some(Arc::new(Dismiss)),
        ..Default::default()
    })
    .await;
    t.push([call(
        "call-question",
        "question",
        json!({"questions": [{"question": "Pick", "header": "Pick", "options": [{"label": "A", "description": "a"}]}]}),
    )]);
    t.prompt("Ask").await.unwrap();
    assert_eq!(t.requests().len(), 1);
    assert_eq!(
        results(&t.s)[0],
        (
            "call-question".into(),
            runner::QUESTION_DISMISSED.into(),
            true
        )
    );
    assert!(t.s.stopped().is_some());
}

// --- Step limits ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_forces_a_text_response_on_an_agents_configured_final_step() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"steps": 2}}}),
        ..Default::default()
    })
    .await;
    t.push([
        call("call-terminal", "glob", json!({"pattern": "*"})),
        call("call-forbidden", "glob", json!({"pattern": "*.rs"})),
    ]);
    t.prompt("Finish at the limit").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].get("tool_choice").is_none());
    assert_eq!(reqs[1]["tool_choice"], "none");
    // Definitions stay for prompt caching.
    assert!(tool_names(&reqs[1]).contains(&"glob".to_string()));
    let last = last_message(&reqs[1]);
    assert!(last["content"]
        .to_string()
        .contains("MAXIMUM STEPS REACHED"));
    let r = results(&t.s);
    assert!(!r[0].2);
    assert_eq!(
        r[1],
        ("call-forbidden".into(), runner::TOOLS_DISABLED.into(), true)
    );
    // The final-step instruction is not stored.
    assert!(!t.s.messages.iter().any(|m| m
        .parts
        .contains(&Part::Text(runner::MAX_STEPS_PROMPT.into()))));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_final_step_answer_ends_the_turn() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"steps": 1}}}),
        ..Default::default()
    })
    .await;
    t.push([text("Summary of the work")]);
    t.prompt("Go").await.unwrap();
    assert_eq!(t.requests()[0]["tool_choice"], "none");
    assert_eq!(texts(&t.s), ["Summary of the work"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_continue_malformed_tool_input_past_the_agent_step_limit() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"steps": 2}}}),
        ..Default::default()
    })
    .await;
    t.push([
        calls(&[("call-first", "glob", r#"{"pattern":"partial"#)]),
        calls(&[("call-at-limit", "glob", r#"{"pattern":"partial"#)]),
    ]);
    t.prompt("Stop malformed tools at the step limit")
        .await
        .unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].get("tool_choice").is_none());
    assert_eq!(reqs[1]["tool_choice"], "none");
    assert_eq!(results(&t.s).iter().filter(|r| r.2).count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_continues_after_repeated_malformed_tool_input() {
    let mut t = plain().await;
    t.push([
        calls(&[("call-first", "glob", r#"{"pattern":"partial"#)]),
        call("call-valid-between", "glob", json!({"pattern": "*"})),
        calls(&[("call-second", "glob", r#"{"pattern":"partial"#)]),
        text("done"),
    ]);
    t.prompt("Keep producing malformed tools").await.unwrap();
    assert_eq!(t.requests().len(), 4);
    let r = results(&t.s);
    assert_eq!(
        r[0].1,
        "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments.\n\nInput excerpt (first 19 of 19 characters):\n{\"pattern\":\"partial"
    );
    assert!(!r[1].2);
    assert!(r[2].2);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_settles_unfinished_tool_input_after_an_output_limit() {
    let mut t = plain().await;
    t.push([
        calls_with(
            "length",
            &[("call-incomplete", "glob", r#"{"pattern":"partial"#)],
        ),
        text("done"),
    ]);
    t.prompt("Big call").await.unwrap();
    // Difference from OpenCode: CodeSmith's provider layer drops a tool call
    // cut off by the output limit (as OpenCode's own provider tests expect),
    // so the session never sees it and continues with the output-limit nudge
    // instead of recording a failed call. Either way the model is told it hit
    // the limit and nothing half-formed runs.
    assert!(results(&t.s).is_empty(), "the truncated call never ran");
    assert_eq!(t.requests().len(), 2);
    assert_eq!(
        last_message(&t.requests()[1])["content"],
        codesmith_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT
    );
}

/// A tool call is shown while its arguments stream, and paired with the call
/// it forms by order even when the call's id differs (Gemini via Chat
/// appends its signature); a call cut off by the output limit is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn tool_inputs_pair_with_the_calls_they_form() {
    let mut t = plain().await;
    t.push([
        sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                "function": {"name": "glob", "arguments": "{\"pattern\":"},
                "extra_content": {"google": {"thought_signature": "SIG"}}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"*.rs\"}"}}]}, "finish_reason": "tool_calls"}]}),
        ]),
        calls_with("length", &[("call_b", "glob", r#"{"pattern":"par"#)]),
        text("done"),
    ]);
    t.prompt("Find files").await.unwrap();
    assert_eq!(
        t.seen.lock().unwrap().lifecycle,
        [
            "input call_a glob",
            "formed call_a call_a~SIG",
            "start call_a~SIG",
            "input call_b glob",
            "dropped call_b",
        ]
    );
}

// --- Output limits and filtered responses ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_continues_text_after_an_output_token_limit() {
    let mut t = plain().await;
    t.push([finish("length", "Partial"), text("Finished")]);
    t.prompt("Complete the response").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let msgs = reqs[1]["messages"].as_array().unwrap();
    assert_eq!(msgs[msgs.len() - 2]["role"], "assistant");
    assert_eq!(msgs[msgs.len() - 2]["content"], "Partial");
    assert_eq!(msgs[msgs.len() - 1]["role"], "user");
    assert_eq!(
        msgs[msgs.len() - 1]["content"],
        runner::CONTINUE_AFTER_OUTPUT_LIMIT
    );
    assert_eq!(texts(&t.s), ["Partial", "Finished"]);
    assert!(t.seen.lock().unwrap().retries.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_caps_output_token_limit_continuations_and_resets_for_new_input() {
    let mut t = plain().await;
    t.push((0..3).map(|_| finish("length", "Partial")));
    let err = t.prompt("Keep going").await.unwrap_err();
    assert_eq!(err.to_string(), runner::OUTPUT_LIMIT_EXHAUSTED);
    assert_eq!(t.requests().len(), 3);
    let nudges = |s: &Session| {
        s.messages
            .iter()
            .filter(|m| m.parts == [Part::Text(runner::CONTINUE_AFTER_OUTPUT_LIMIT.into())])
            .count()
    };
    assert_eq!(nudges(&t.s), 2);
    t.push([finish("length", "Partial"), text("Finished")]);
    t.prompt("Try a new response").await.unwrap();
    assert_eq!(t.requests().len(), 5);
    assert_eq!(nudges(&t.s), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_compacts_when_the_provider_cuts_an_oversized_prompt_to_its_window_and_replies_with_nothing(
) {
    let mut t = start(Setup {
        models: json!({"m": {"limit": {"context": 200000, "output": 8000}}}),
        ..Default::default()
    })
    .await;
    t.push([
        sse(&[
            json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 199994, "completion_tokens": 0}}),
        ]),
        text("## Objective\n- summary"),
        text("Done"),
    ]);
    t.prompt("Large prompt").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 3);
    assert!(reqs[1]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are a helpful assistant that writes concise"));
    assert_eq!(texts(&t.s).last().unwrap(), "Done");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_projects_content_filter_finishes_as_visible_terminal_failures() {
    let mut t = plain().await;
    t.push([sse(&[
        json!({"choices": [{"delta": {"content": "Partial"}, "finish_reason": "content_filter"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 8, "completion_tokens": 3}}),
    ])]);
    let err = t.prompt("Blocked response").await.unwrap_err();
    assert_eq!(err.to_string(), "Provider blocked the response");
    assert_eq!(texts(&t.s), ["Partial"]);
    assert_eq!(
        t.s.usage,
        Usage {
            input: 8,
            output: 3,
            total: Some(11),
            ..Default::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_message_omits_empty_assistant_turns() {
    let mut t = plain().await;
    t.push([text(""), text("second")]);
    t.prompt("First").await.unwrap();
    t.prompt("Second").await.unwrap();
    assert_eq!(roles(&t.requests()[1])[1..], ["user", "user"]);
}

// --- Retries ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_a_model_call_without_consuming_the_logical_agent_step() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"steps": 2}}}),
        ..Default::default()
    })
    .await;
    t.push([
        status(500, json!({"error": {"message": "internal"}})),
        call("c1", "glob", json!({"pattern": "*"})),
        status(503, json!({"error": {"message": "overloaded"}})),
        text("Recovered"),
    ]);
    t.prompt("Retry").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 4);
    assert!(
        reqs[1].get("tool_choice").is_none(),
        "the retry was not a step"
    );
    assert_eq!(reqs[3]["tool_choice"], "none");
    let attempts: Vec<u32> = t.seen.lock().unwrap().retries.iter().map(|r| r.0).collect();
    assert_eq!(attempts, [1, 1]);
    assert_eq!(texts(&t.s), ["Recovered"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_retry_non_eligible_provider_failures() {
    let mut t = plain().await;
    t.push([status(
        400,
        json!({"error": {"message": "bad request", "type": "invalid_request_error"}}),
    )]);
    assert!(t.prompt("Fail").await.is_err());
    assert_eq!(t.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_records_a_usage_limit_429_without_retrying() {
    let mut t = plain().await;
    t.push([status(
        429,
        json!({"error": {"message": "You exceeded your current quota", "type": "insufficient_quota", "code": "insufficient_quota"}}),
    )]);
    let err = t.prompt("Fail").await.unwrap_err();
    assert!(err.to_string().contains("quota"));
    assert_eq!(t.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_stops_once_the_retry_allowance_is_spent() {
    let mut t = plain().await;
    t.push((0..12).map(|_| status(503, json!({"error": {"message": "unavailable"}}))));
    assert!(t.prompt("Fail").await.is_err());
    assert_eq!(t.requests().len(), 11, "one attempt and ten retries");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_after_a_mid_stream_failure_without_partial_text() {
    let mut t = plain().await;
    let broken = {
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices": [{"delta": {"content": "Partial"}}]}),
            json!({"error": {"type": "server_error", "message": "The server had an error"}})
        );
        http(200, "text/event-stream", &body)
    };
    t.push([broken, text("Recovered")]);
    t.prompt("Retry mid-stream").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[1].to_string().contains("Partial"));
    assert_eq!(texts(&t.s), ["Recovered"]);
    assert_eq!(t.seen.lock().unwrap().retries.len(), 1);
}

/// A Chat stream that sends `chunks`, then fails with a request error the
/// session does not retry.
fn fails_after(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str(&format!(
        "data: {}\n\n",
        json!({"error": {"type": "invalid_request_error", "message": "Invalid request"}})
    ));
    http(200, "text/event-stream", &body)
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_durably_closes_partial_text_when_the_provider_stream_fails() {
    let mut t = plain().await;
    t.push([fails_after(&[
        json!({"choices": [{"delta": {"content": "Partial"}}]}),
    ])]);
    let err = t.prompt("Fail after text").await.unwrap_err();
    assert!(err.to_string().contains("Invalid request"), "{err}");
    assert_eq!(t.requests().len(), 1);
    assert_eq!(texts(&t.s), ["Partial"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_durably_closes_partial_reasoning_when_the_provider_stream_fails() {
    let mut t = plain().await;
    t.push([fails_after(&[
        json!({"choices": [{"delta": {"reasoning_content": "Partial"}}]}),
    ])]);
    assert!(t.prompt("Fail after reasoning").await.is_err());
    let last = t.s.messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert!(matches!(&last.parts[..], [Part::Reasoning(r)] if r.text == "Partial"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_durably_closes_partial_tool_input_when_the_provider_stream_fails() {
    // The call never completed, so it is not kept (OpenCode keeps it as a
    // failed tool); nothing else streamed, so no message is added.
    let mut t = plain().await;
    t.push([fails_after(&[
        json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call-partial", "function": {"name": "echo", "arguments": "{\"va"}}
        ]}}]}),
    ])]);
    assert!(t.prompt("Fail after tool input").await.is_err());
    assert_eq!(t.s.messages.last().unwrap().role, Role::User);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_error_partial_loop_reports_only_the_failed_step_as_partial() {
    let mut t = plain().await;
    t.push([
        call("call-1", "glob", json!({"pattern": "*.none"})),
        fails_after(&[json!({"choices": [{"delta": {"content": "Partial answer"}}]})]),
    ]);
    assert!(t.prompt("Glob, then answer.").await.is_err());
    let last = t.s.messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.parts, [Part::Text("Partial answer".into())]);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_an_incomplete_stream_before_output() {
    let mut t = start(Setup {
        protocol: "anthropic-messages",
        ..Default::default()
    })
    .await;
    let ev = |kind: &str, v: Value| format!("event: {kind}\ndata: {v}\n\n");
    let start = ev(
        "message_start",
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
    );
    let cut = http(200, "text/event-stream", &start);
    let full = http(
        200,
        "text/event-stream",
        &format!(
            "{start}{}{}{}{}",
            ev(
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
            ),
            ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Recovered"}})
            ),
            ev(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}})
            ),
            ev("message_stop", json!({"type": "message_stop"})),
        ),
    );
    t.push([cut, full]);
    t.prompt("Retry incomplete stream").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    assert_eq!(texts(&t.s), ["Recovered"]);
}

/// A tool the provider runs (OpenCode's `providerExecuted`) shows as a
/// settled tool row, never runs here, does not continue the turn, and is
/// replayed to the provider in its own blocks.
#[tokio::test(flavor = "multi_thread")]
async fn hosted_tool_calls_settle_without_running_or_continuing() {
    let mut t = start(Setup {
        protocol: "anthropic-messages",
        models: json!({"m": {"options": {"hostedTools": ["web_search"]}}}),
        ..Default::default()
    })
    .await;
    let ev = |v: Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let found =
        json!([{"type": "web_search_result", "url": "https://example.com", "title": "Example"}]);
    let stream: String = [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"rust\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": found}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": "Found it."}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ]
    .into_iter()
    .map(ev)
    .collect();
    t.push([http(200, "text/event-stream", &stream)]);
    t.prompt("Search").await.unwrap();
    assert_eq!(
        t.requests().len(),
        1,
        "a hosted call does not continue the turn"
    );
    assert!(t.requests()[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["type"] == "web_search_20250305" && t["name"] == "web_search"));
    {
        let seen = t.seen.lock().unwrap();
        assert!(seen.lifecycle.contains(&"start srvtoolu_1".to_string()));
        assert_eq!(
            seen.tools,
            [("web_search".to_string(), "1 result".to_string(), false)]
        );
    }
    assert!(results(&t.s).is_empty());
    assert_eq!(texts(&t.s), ["Found it."]);

    t.push([http(
        200,
        "text/event-stream",
        &[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ]
        .into_iter()
        .map(ev)
        .collect::<String>(),
    )]);
    t.prompt("More").await.unwrap();
    let replayed = &t.requests()[1]["messages"][1];
    assert_eq!(replayed["role"], "assistant");
    let blocks: Vec<&str> = replayed["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        blocks,
        ["server_tool_use", "web_search_tool_result", "text"]
    );
    assert_eq!(replayed["content"][1]["content"], found);
}

// --- Interrupted turns ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_durably_fails_local_tools_left_running_by_a_prior_process_before_continuing() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db = root.join("sessions.db");
    let st = Store::open(&db).unwrap();
    st.create(&Info {
        id: "ses_interrupted".into(),
        directory: root.display().to_string(),
        title: Some("t".into()),
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: store::now(),
        updated: store::now(),
        usage: Usage::default(),
    })
    .unwrap();
    for message in [
        Message::user("Recover interrupted tool"),
        Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall {
                id: "call-interrupted".into(),
                name: "glob".into(),
                input: json!({"pattern": "stale"}),
                meta: None,
            }],
        },
    ] {
        st.append("ses_interrupted", &Record::Message { message })
            .unwrap();
    }
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let loaded = Loaded {
        config: config(&base, json!({})),
        files: vec![],
        root: root.clone(),
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: Some("ses_interrupted".into()),
        title: None,
        parent: None,
        questioner: None,
        mcp: None,
    };
    let mut s = Session::new(
        &loaded,
        root,
        opts,
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();
    let mut sink = Rec(Arc::default());
    s.prompt("Continue", &mut sink).await.unwrap();
    let body = script.lock().unwrap().log[0].clone();
    assert_eq!(roles(&body)[1..], ["user", "assistant", "tool", "user"]);
    let tool = &body["messages"][3];
    assert_eq!(tool["content"], "Tool execution interrupted: glob");
    // Recorded, so a reload sees the same history.
    let hist = st.history("ses_interrupted").unwrap();
    assert!(
        matches!(&hist[2].parts[0], Part::ToolResult { is_error: true, content, .. } if content == "Tool execution interrupted: glob")
    );
}

// --- Subagents (tool-subagent.test.ts, subagent-report.test.ts) ---

fn reviewer(extra: Value) -> Setup {
    let mut cfg = json!({"agents": {"reviewer": {"description": "Reviews", "mode": "subagent", "system": "You review code."}}});
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    Setup {
        extra: cfg,
        store: true,
        ..Default::default()
    }
}

fn subagent(id: &str, input: Value) -> String {
    call(id, "subagent", input)
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_starts_a_child_session_and_returns_its_report() {
    let mut t = start(reviewer(json!({}))).await;
    t.push([
        subagent(
            "call-subagent",
            json!({"agent": "reviewer", "description": "review", "prompt": "review this", "model": "", "sessionID": ""}),
        ),
        text("Looks fine."),
        text("Parent done"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 3);
    let child_prompt = last_message(&reqs[1])["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        child_prompt,
        "You are a subagent spawned by another session.\nreview this"
    );
    let r = &results(&t.s)[0];
    assert!(!r.2);
    let re = regex::Regex::new(r#"^<subagent sessionID="(ses_[0-9a-f]+)" state="completed" description="review">\nLooks fine.\n</subagent>$"#).unwrap();
    let child = re.captures(&r.1).unwrap_or_else(|| panic!("{}", r.1))[1].to_string();
    let st = t.store();
    let info = st.get(&child).unwrap().unwrap();
    assert_eq!(info.parent_id.as_deref(), Some(t.s.id.as_str()));
    assert_eq!(info.agent, "reviewer");
    assert_eq!(info.title.as_deref(), Some("review"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_continues_an_existing_child_session() {
    let mut t = start(reviewer(json!({}))).await;
    t.push([
        subagent(
            "c1",
            json!({"agent": "reviewer", "description": "review", "prompt": "review this"}),
        ),
        text("First report"),
        text("ok"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let child = regex::Regex::new(r#"sessionID="([^"]+)""#)
        .unwrap()
        .captures(&results(&t.s)[0].1)
        .unwrap()[1]
        .to_string();
    t.push([
        subagent(
            "c2",
            json!({"agent": "reviewer", "description": "follow up", "prompt": "continue this", "sessionID": child}),
        ),
        text("Second report"),
        text("ok"),
    ]);
    t.prompt("Follow up").await.unwrap();
    let reqs = t.requests();
    let child_req = &reqs[4];
    let users: Vec<String> = child_req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        users,
        [
            "You are a subagent spawned by another session.\nreview this",
            "continue this"
        ]
    );
    assert!(results(&t.s)[1]
        .1
        .contains(&format!("sessionID=\"{child}\"")));
    assert!(results(&t.s)[1].1.contains("Second report"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_rejects_unrelated_children() {
    let mut t = start(reviewer(json!({}))).await;
    let me = t.s.id.clone();
    t.push([
        calls(&[
            ("c-missing", "subagent", &json!({"agent": "reviewer", "description": "follow up", "prompt": "continue", "sessionID": "ses_missing"}).to_string()),
            ("c-parent", "subagent", &json!({"agent": "reviewer", "description": "follow up", "prompt": "continue", "sessionID": me}).to_string()),
        ]),
        text("ok"),
    ]);
    t.prompt("Continue the wrong ones").await.unwrap();
    let r = results(&t.s);
    assert_eq!(r[0].1, "Subagent session not found: ses_missing");
    assert_eq!(
        r[1].1,
        format!("Session {me} is not a child of the current session")
    );
    assert!(r[0].2 && r[1].2);
    assert_eq!(t.requests().len(), 2, "no child ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_prevents_subagents_from_launching_subagents_by_default() {
    let mut t = start(reviewer(json!({}))).await;
    t.push([
        subagent(
            "c1",
            json!({"agent": "reviewer", "description": "outer", "prompt": "go"}),
        ),
        subagent(
            "c2",
            json!({"agent": "reviewer", "description": "nested", "prompt": "should fail"}),
        ),
        text("child done"),
        text("parent done"),
    ]);
    t.prompt("Nest").await.unwrap();
    assert_eq!(t.requests().len(), 4);
    let nested = last_message(&t.requests()[2]).clone();
    assert!(nested["content"]
        .as_str()
        .unwrap()
        .starts_with("Subagent depth limit reached (1)."));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_allows_nested_subagents_up_to_the_configured_depth() {
    let mut t = start(reviewer(json!({"subagent_depth": 2}))).await;
    t.push([
        subagent(
            "c1",
            json!({"agent": "reviewer", "description": "outer", "prompt": "go"}),
        ),
        subagent(
            "c2",
            json!({"agent": "reviewer", "description": "nested", "prompt": "should run"}),
        ),
        text("grandchild done"),
        text("child done"),
        text("parent done"),
    ]);
    t.prompt("Nest").await.unwrap();
    assert_eq!(t.requests().len(), 5);
    let nested = last_message(&t.requests()[3]).clone();
    assert!(nested["content"]
        .as_str()
        .unwrap()
        .contains("state=\"completed\" description=\"nested\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_reports_failures() {
    let mut t = start(reviewer(json!({}))).await;
    t.push([
        subagent(
            "c1",
            json!({"agent": "reviewer", "description": "review", "prompt": "go"}),
        ),
        status(
            400,
            json!({"error": {"message": "Disconnected", "type": "invalid_request_error"}}),
        ),
        text("parent saw it"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let r = &results(&t.s)[0];
    assert!(r.2);
    assert!(
        r.1.contains("state=\"error\" description=\"review\""),
        "{}",
        r.1
    );
    assert!(r.1.contains("Disconnected"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_reports_a_cancelled_child() {
    let mut cfg = reviewer(json!({}));
    cfg.extra["agents"]["reviewer"]["permissions"] =
        json!([{"action": "glob", "resource": "*", "effect": "ask"}]);
    cfg.reply = Reply::Reject(None);
    // The parent may start the subagent without asking.
    cfg.extra["permissions"] = json!([{"action": "subagent", "resource": "*", "effect": "allow"}]);
    let mut t = start(cfg).await;
    t.push([
        subagent(
            "c1",
            json!({"agent": "reviewer", "description": "review", "prompt": "go"}),
        ),
        call("g1", "glob", json!({"pattern": "*"})),
        text("parent continues"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let r = &results(&t.s);
    let report = &r.iter().find(|r| r.0 == "c1").unwrap().1;
    assert!(report.contains("state=\"cancelled\""), "{report}");
    assert!(report.contains("Subagent cancelled"));
    assert!(t.s.stopped().is_none(), "the parent keeps going");
    assert_eq!(texts(&t.s), ["parent continues"]);
}

// --- Revert (session-revert.test.ts) ---

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_revert_undoes_and_restores_a_file_rename_without_losing_either_path() {
    if !git_available() {
        return;
    }
    let mut t = start(Setup {
        extra: json!({"snapshots": true}),
        store: true,
        ..Default::default()
    })
    .await;
    let root = t.dir.path().canonicalize().unwrap();
    std::fs::write(root.join("old name.txt"), "Preserve this content.\n").unwrap();
    std::fs::write(root.join("unrelated.txt"), "Unrelated content.\n").unwrap();
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "mv 'old name.txt' 'new name.txt'", "description": "rename"}),
        ),
        text("Renamed"),
    ]);
    t.prompt("Rename the file").await.unwrap();
    assert!(root.join("new name.txt").exists());
    // An edit made after the turn is not the turn's to undo.
    std::fs::write(root.join("unrelated.txt"), "Keep this later edit.\n").unwrap();
    let db = t.store();
    let snaps = Snapshots::for_project(&root).unwrap();
    let mut changes = session::undo(&db, &t.s.id, &snaps).unwrap().unwrap();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(
        changes
            .iter()
            .map(|c| (c.path.as_str(), c.status))
            .collect::<Vec<_>>(),
        [("new name.txt", 'A'), ("old name.txt", 'D')]
    );
    assert!(!root.join("new name.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("old name.txt")).unwrap(),
        "Preserve this content.\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("unrelated.txt")).unwrap(),
        "Keep this later edit.\n"
    );
    session::redo(&db, &t.s.id, &snaps).unwrap().unwrap();
    assert!(!root.join("old name.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("new name.txt")).unwrap(),
        "Preserve this content.\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("unrelated.txt")).unwrap(),
        "Keep this later edit.\n"
    );
}

// --- Engine fixes found in review (not OpenCode ports) ---

/// An OpenAI chat stream that answers `t` and reports token usage.
fn text_with_usage(t: &str, input: u64, output: u64) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}}),
    ])
}

/// Fixed: a reloaded session's spending came from its stored total, taken
/// as all billed when any step was billed, so a session mixing billed and
/// unbilled steps lost the unbilled ones' cost (the TUI's status after a
/// reload). The unbilled part is now stored beside the total.
#[tokio::test(flavor = "multi_thread")]
async fn spending_of_mixed_billed_and_unbilled_steps_survives_a_reload() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([sse(&[
        json!({"choices": [{"delta": {"content": "billed"}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 1, "cost": 0.5}}),
    ])]);
    t.prompt("one").await.unwrap();
    t.push([text_with_usage("unbilled", 1000, 100)]);
    t.prompt("two").await.unwrap();
    let spend = t.s.spend();
    assert_eq!(spend.billed, Some(0.5));
    assert_eq!((spend.unbilled.input, spend.unbilled.output), (1000, 100));

    let loaded = Loaded {
        config: config("http://127.0.0.1:9/v1", json!({})),
        files: vec![],
        root: t.dir.path().canonicalize().unwrap(),
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: Some(t.s.id.clone()),
        title: None,
        parent: None,
        questioner: None,
        mcp: None,
    };
    let root = loaded.root.clone();
    let again = Session::new(
        &loaded,
        root,
        opts,
        Arc::new(Fixed(Reply::Once)),
        Some(t.store()),
    )
    .unwrap();
    assert_eq!(again.usage, t.s.usage);
    assert_eq!(again.spend(), spend);
    let price = codesmith_core::config::Cost {
        input: 1.0,
        output: 10.0,
        cache_read: 0.0,
        cache_write: 0.0,
    };
    // $0.50 billed, plus 1000 input and 100 output tokens at $1 and $10 per million.
    let cost = again.spend().cost(Some(&price)).unwrap();
    assert!((cost - 0.502).abs() < 1e-9, "{cost}");
}

/// Fixed: a turn whose model answered while a background command still ran
/// waited for the command, so the TUI and server stayed busy (and `run`
/// hung) until it ended. As in OpenCode, the turn now ends; the command is
/// reported when it finishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_ends_while_its_background_command_runs_and_resumes_when_it_finishes() {
    let mut t = plain().await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "sleep 1; echo bg-finished", "description": "slow", "background": true}),
        ),
        text("Started it."),
    ]);
    let started = std::time::Instant::now();
    t.prompt("start it").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the turn waited"
    );
    assert_eq!(t.s.background_running(), 1);
    assert_eq!(texts(&t.s), ["Started it."]);

    // Nothing finished yet: resuming does nothing.
    let mut sink = Rec(t.seen.clone());
    t.s.resume_background(&mut sink).await.unwrap();
    assert_eq!(t.requests().len(), 2);

    t.s.wait_background().await;
    assert_eq!(t.s.background_finished(), 1);
    t.push([text("It printed bg-finished.")]);
    t.s.resume_background(&mut sink).await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 3);
    let note = last_message(&reqs[2]);
    assert_eq!(note["role"], "user");
    let note = note["content"].to_string();
    assert!(
        note.contains("<shell id=\\\"shell_1\\\"") && note.contains("bg-finished"),
        "{note}"
    );
    assert_eq!(texts(&t.s), ["Started it.", "It printed bg-finished."]);
    assert_eq!(t.s.background_finished(), 0);
}

/// The user moves a running foreground command to the background (as
/// OpenCode's `Session.background` does): the call returns at once, the
/// model is told before its next step, and the call's shell ID reached the
/// sink while it ran.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_command_the_user_backgrounds_is_told_to_the_model() {
    let mut t = plain().await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "echo started; sleep 30", "description": "slow"}),
        ),
        text("Moved on."),
    ]);
    let jobs = t.s.jobs();
    let mover = tokio::spawn(async move {
        loop {
            if !jobs.background_all().is_empty() {
                return jobs;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let started = std::time::Instant::now();
    t.prompt("run it").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the call waited"
    );
    let jobs = mover.await.unwrap();
    assert_eq!(t.s.background_running(), 1);
    let reqs = t.requests();
    let msgs = reqs[1]["messages"].as_array().unwrap();
    let n = msgs.len();
    assert!(
        msgs[n - 2]["content"]
            .to_string()
            .contains("Command moved to the background (shell ID: shell_1)."),
        "{msgs:?}"
    );
    let note = msgs[n - 1]["content"].to_string();
    assert!(
        note.contains("User requested that active blocking work be moved to the background.")
            && note.contains("- shell: echo started; sleep 30"),
        "{note}"
    );
    let first = t.seen.lock().unwrap().progress[0].clone();
    assert_eq!(first, ("c1".into(), r#"{"shellID":"shell_1"}"#.into()));
    jobs.stop_all().await;
}

/// Fixed: rebuilding a session (a cancel, undo, or a mode or model switch)
/// stopped its background commands with the old session. A front end now
/// keeps the jobs and hands them to the rebuilt session, which reports them.
#[tokio::test(flavor = "multi_thread")]
async fn adopted_jobs_outlive_the_old_session_and_are_reported_by_the_new_one() {
    let mut t = plain().await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "sleep 1; echo survived", "description": "slow", "background": true}),
        ),
        text("Started it."),
    ]);
    t.prompt("start it").await.unwrap();
    let jobs = t.s.jobs();
    let mut u = plain().await;
    drop(t);
    u.s.adopt_jobs(jobs);
    assert_eq!(u.s.background_running(), 1);
    u.s.wait_background().await;
    assert_eq!(u.s.background_finished(), 1);
    u.push([text("Saw it.")]);
    let mut sink = Rec(u.seen.clone());
    u.s.resume_background(&mut sink).await.unwrap();
    let note = last_message(&u.requests()[0])["content"].to_string();
    assert!(
        note.contains("<shell id=\\\"shell_1\\\"") && note.contains("survived"),
        "{note}"
    );
}

/// Fixed: a resume that failed before reporting its finished commands (the
/// store failing, another process holding the turn) left them unreported,
/// so a front end waking the session for them resumed again at once: a
/// tight error loop. Resumes now back off after a failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_resume_that_fails_before_reporting_backs_off() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "echo bg-done", "description": "quick", "background": true}),
        ),
        text("Started it."),
    ]);
    t.prompt("start it").await.unwrap();
    t.s.wait_background().await;
    assert_eq!(t.s.background_finished(), 1);
    assert_eq!(t.s.resume_due(), None);

    // Another process holds the turn, and this session does not wait for it.
    let path = t.data.path().join("sessions.db");
    let other = store::TurnLease::acquire(&path, &t.s.id, "other", false, || {})
        .await
        .unwrap();
    t.s.set_wait_for_turn(false);
    t.s.set_retry_scale(1.0);
    let mut sink = Rec(t.seen.clone());
    let before = std::time::Instant::now();
    assert!(t.s.resume_background(&mut sink).await.is_err());
    assert_eq!(t.s.background_finished(), 1, "still unreported");
    let due = t.s.resume_due().expect("backing off");
    assert!(
        due >= before + Duration::from_millis(900),
        "{:?}",
        due - before
    );
    // A front end waits until then; held after an interrupt, not at all.
    assert_eq!(t.s.idle_wake(&mut None), session::IdleWake::At(due));
    let mut held = Some(1);
    assert_eq!(t.s.idle_wake(&mut held), session::IdleWake::Idle);
    assert_eq!(held, Some(1));

    // Called again early, the resume waits out the backoff first.
    t.s.set_retry_scale(0.0001);
    let again = std::time::Instant::now();
    assert!(t.s.resume_background(&mut sink).await.is_err());
    assert!(std::time::Instant::now() >= due, "{:?}", again.elapsed());
    assert_eq!(t.requests().len(), 2, "no model call while failing");

    // Once it can run, it reports the command and the backoff is over.
    drop(other);
    tokio::time::sleep(Duration::from_millis(5)).await;
    t.push([text("Saw it.")]);
    t.s.resume_background(&mut sink).await.unwrap();
    assert_eq!(t.s.background_finished(), 0);
    assert_eq!(t.s.resume_due(), None);
    let note = last_message(&t.requests()[2])["content"].to_string();
    assert!(note.contains("bg-done"), "{note}");
}

/// A command that finished after its turn ended is reported at the next
/// prompt, before the user's message.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_command_is_reported_with_the_next_prompt() {
    let mut t = plain().await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "sleep 0.3; echo quick", "description": "quick", "background": true}),
        ),
        text("Started."),
    ]);
    t.prompt("start it").await.unwrap();
    t.s.wait_background().await;
    t.push([text("Saw it.")]);
    t.prompt("anything new?").await.unwrap();
    let reqs = t.requests();
    let msgs = reqs.last().unwrap()["messages"].as_array().unwrap().clone();
    let n = msgs.len();
    assert!(
        msgs[n - 2]["content"].to_string().contains("quick"),
        "{msgs:?}"
    );
    assert_eq!(msgs[n - 1]["content"], "anything new?");
}

/// What a one-shot caller (`codesmith run`, a subagent, an automation)
/// does: wait for the commands and let the model act on each result.
#[tokio::test(flavor = "multi_thread")]
async fn settling_waits_for_background_commands_and_resumes_the_model() {
    let mut t = plain().await;
    t.push([
        call(
            "c1",
            "shell",
            json!({"command": "sleep 0.3; echo one", "description": "a", "background": true}),
        ),
        text("Started."),
        text("Done: one."),
    ]);
    t.prompt("start it").await.unwrap();
    let mut sink = Rec(t.seen.clone());
    t.s.settle_background(&mut sink).await.unwrap();
    assert_eq!(texts(&t.s), ["Started.", "Done: one."]);
    assert_eq!(t.s.background_running() + t.s.background_finished(), 0);
    let notices = t.seen.lock().unwrap().notices.clone();
    assert!(
        notices
            .iter()
            .any(|n| n == "waiting for 1 background command: shell_1 (sleep 0.3; echo one)"),
        "{notices:?}"
    );
}

/// Fixed: when the repeat guard stopped the turn, the batch's results were
/// thrown away, so a call that really ran earlier in it (here a write) was
/// later reported to the model as interrupted.
#[tokio::test(flavor = "multi_thread")]
async fn the_repeat_guard_keeps_the_results_of_calls_that_ran() {
    let mut t = plain().await;
    let read = json!({"path": "a.txt"}).to_string();
    let write = json!({"path": "a.txt", "content": "hello"}).to_string();
    let mut batch: Vec<(&str, &str, &str)> = vec![("w", "write", write.as_str())];
    let ids: Vec<String> = (0..RepeatGuard::ABORT + 1)
        .map(|i| format!("r{i}"))
        .collect();
    for id in &ids {
        batch.push((id.as_str(), "read", read.as_str()));
    }
    batch.push(("after", "read", r#"{"path": "b.txt"}"#));
    t.push([calls(&batch)]);
    let err = t.prompt("go").await.unwrap_err();
    assert!(
        err.to_string().contains("repeated the same read call"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(t.path("a.txt")).unwrap(), "hello");
    let r = results(&t.s);
    assert_eq!(r.len(), batch.len(), "every call is settled: {r:?}");
    assert_eq!(r[0].0, "w");
    assert!(!r[0].2 && r[0].1 != runner::TOOLS_INTERRUPTED, "{:?}", r[0]);
    assert!(!r[1].2 && r[1].1.contains("hello"), "{:?}", r[1]);
    let aborted = &r[RepeatGuard::ABORT as usize + 1];
    assert!(aborted.2 && aborted.1.contains("repeated"), "{aborted:?}");
    assert_eq!(r.last().unwrap().1, runner::TOOLS_INTERRUPTED);

    // The next request carries the real results, not "interrupted".
    t.push([text("ok")]);
    t.prompt("again").await.unwrap();
    let next = t.requests().last().unwrap().clone();
    let tool_msgs: Vec<&Value> = next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), batch.len());
    assert_ne!(tool_msgs[0]["content"], runner::TOOLS_INTERRUPTED);
}

/// Fixed: summary calls' tokens were dropped, understating the session's
/// usage and cost and `codesmith stats`.
#[tokio::test(flavor = "multi_thread")]
async fn compaction_counts_the_summary_calls_tokens() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([text_with_usage("one", 10, 1), text_with_usage("two", 20, 2)]);
    t.prompt("first").await.unwrap();
    t.prompt("second").await.unwrap();
    assert_eq!((t.s.usage.input, t.s.usage.output), (30, 3));
    t.push([text_with_usage("## Objective\n- summary", 70, 7)]);
    let mut sink = Rec(t.seen.clone());
    t.s.compact(&mut sink).await.unwrap();
    assert_eq!((t.s.usage.input, t.s.usage.output), (100, 10));
    let steps: Vec<(String, Usage)> = t
        .store()
        .entries(&t.s.id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Step { model, usage } => Some((model, usage)),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[2].0, "fake/m");
    assert_eq!((steps[2].1.input, steps[2].1.output), (70, 7));
}

/// Fixed: when the system prompt and tools alone filled the window, every
/// step summarized the (already small) history again. After the first
/// compaction it now says so once and stops.
#[tokio::test(flavor = "multi_thread")]
async fn compaction_does_not_repeat_when_the_history_is_already_small() {
    let mut t = start(Setup {
        models: json!({"m": {"limit": {"context": 3000, "output": 500}}}),
        ..Default::default()
    })
    .await;
    assert!(
        t.s.estimate_prompt() > 2_550,
        "the tools alone are over the ceiling: {}",
        t.s.estimate_prompt()
    );
    let is_summary = |b: &Value| {
        b["messages"][0]["content"]
            .as_str()
            .is_some_and(|s| s.starts_with("You are a helpful assistant that writes concise"))
    };
    t.push([text("one")]);
    t.prompt("first").await.unwrap();
    t.push([text("## Objective\n- summary"), text("two")]);
    t.prompt("second").await.unwrap();
    assert_eq!(t.requests().iter().filter(|b| is_summary(b)).count(), 1);
    for (i, p) in ["third", "fourth"].into_iter().enumerate() {
        t.push([text(&format!("answer {i}"))]);
        t.prompt(p).await.unwrap();
    }
    assert_eq!(
        t.requests().iter().filter(|b| is_summary(b)).count(),
        1,
        "compacted again with nothing to summarize"
    );
    let notices = t.seen.lock().unwrap().notices.clone();
    let skipped: Vec<&String> = notices
        .iter()
        .filter(|n| n.contains("not compacted"))
        .collect();
    assert_eq!(skipped.len(), 1, "{notices:?}");
}

/// Fixed: the prompt estimate counted only the built-in tools, not MCP,
/// plugin, Code Mode or automation tools the model is also sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_estimate_counts_every_offered_tool() {
    let plain = plain().await;
    let stored = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    let (description, parameters) = codesmith_core::automation::tool_spec();
    let spec = codesmith_ai::ToolSpec {
        name: codesmith_core::automation::TOOL.into(),
        description,
        parameters,
    };
    let extra = serde_json::to_string(&spec).unwrap().len() as u64 / 4;
    let diff = stored.s.estimate_prompt() - plain.s.estimate_prompt();
    assert!(diff.abs_diff(extra) <= 1, "{diff} vs {extra}");
}
