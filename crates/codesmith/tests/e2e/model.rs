//! A scripted OpenAI-compatible model server.
//!
//! A [`Script`] is an ordered list of rules, each a request matcher and a
//! reply. Every request to `POST /v1/chat/completions` is recorded, then
//! answered by the first rule (in order) that still has uses left and
//! matches it. A request nothing matches is answered with a non-retryable
//! 400 and listed by [`Model::unmatched`], so a test fails fast instead of
//! waiting out retries.
//!
//! Replies stream as Server-Sent Events like OpenAI's chat completions:
//! text, reasoning, tool calls, any finish reason, usage. A reply can also
//! be an HTTP error, a stream that drops mid-way without finishing, or one
//! that stalls forever (for cancel and abort tests).

use axum::body::{Body, Bytes};
use axum::extract::State as Shared;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A predicate over a request body.
#[derive(Clone)]
pub struct M(pub Arc<dyn Fn(&Value) -> bool + Send + Sync>);

/// The text of an OpenAI chat message's content (a string or text parts).
pub fn content(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Whether a request message is the date a local server gets before the
/// history (the scripted model listens on loopback).
pub fn is_date(m: &Value) -> bool {
    content(m).starts_with("<system-update>\nToday's date: ")
}

pub fn messages(body: &Value) -> &[Value] {
    body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Whether a user message is one the engine wrote (a continuation nudge, or
/// a system update such as the final-step instruction, which Chat receives
/// as `<system-update>` user text), not the user's prompt.
fn engine(m: &Value) -> bool {
    let text = content(m);
    text.starts_with("<system-update>") || codesmith::core::runner::is_engine_message(&text)
}

/// The latest user prompt: the last user-role message the engine did not write.
pub fn prompt(body: &Value) -> String {
    messages(body)
        .iter()
        .rev()
        .find(|m| m["role"] == "user" && !engine(m))
        .map(content)
        .unwrap_or_default()
}

/// The system prompt.
pub fn system(body: &Value) -> String {
    messages(body)
        .iter()
        .filter(|m| m["role"] == "system")
        .map(content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Names of the tools offered.
pub fn tools(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
        .collect()
}

/// Tool results since the latest user prompt.
pub fn results(body: &Value) -> Vec<String> {
    let msgs = messages(body);
    let at = msgs
        .iter()
        .rposition(|m| m["role"] == "user" && !engine(m))
        .unwrap_or(0);
    msgs[at..]
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(content)
        .collect()
}

impl M {
    pub fn f(f: impl Fn(&Value) -> bool + Send + Sync + 'static) -> M {
        M(Arc::new(f))
    }
    /// A request that offers tools (an agent step).
    pub fn tools() -> M {
        M::f(|b| b.get("tools").is_some())
    }
    /// The session-title request.
    pub fn title() -> M {
        M::f(|b| system(b).starts_with("You name conversations"))
    }
    /// The latest user prompt contains `s`.
    pub fn prompt(s: &str) -> M {
        let s = s.to_string();
        M::f(move |b| prompt(b).contains(&s))
    }
    /// The system prompt contains `s`.
    pub fn system(s: &str) -> M {
        let s = s.to_string();
        M::f(move |b| system(b).contains(&s))
    }
    /// The very last message (any role) contains `s`.
    pub fn last(s: &str) -> M {
        let s = s.to_string();
        M::f(move |b| messages(b).last().is_some_and(|m| content(m).contains(&s)))
    }
    /// Exactly `n` tool results since the latest user prompt.
    pub fn step(n: usize) -> M {
        M::f(move |b| results(b).len() == n)
    }
    pub fn and(self, o: M) -> M {
        M::f(move |b| (self.0)(b) && (o.0)(b))
    }
    pub fn not(self) -> M {
        M::f(move |b| !(self.0)(b))
    }
}

#[derive(Clone, Debug)]
pub enum End {
    /// `finish_reason`, usage, then `[DONE]`.
    Finish(String),
    /// Close the connection mid-body, without a finish or `[DONE]`.
    Drop,
    /// Keep the connection open and send nothing more.
    Stall,
}

/// One scripted answer.
#[derive(Clone, Debug)]
pub enum Reply {
    Stream {
        chunks: Vec<Value>,
        end: End,
        usage: (u64, u64),
    },
    Status(u16, String),
}

static CALLS: AtomicUsize = AtomicUsize::new(0);

impl Reply {
    /// A reply with nothing in it yet that finishes with `stop`.
    pub fn empty() -> Reply {
        Reply::Stream {
            chunks: vec![],
            end: End::Finish("stop".into()),
            usage: (100, 10),
        }
    }
    /// A text answer that finishes with `stop`.
    pub fn text(t: &str) -> Reply {
        Reply::empty().and_text(t)
    }
    /// One tool call that finishes with `tool_calls`.
    pub fn call(name: &str, args: Value) -> Reply {
        Reply::empty().and_call(name, args).finish("tool_calls")
    }
    pub fn status(code: u16, body: &str) -> Reply {
        Reply::Status(code, body.into())
    }
    fn push(mut self, c: Value) -> Reply {
        if let Reply::Stream { chunks, .. } = &mut self {
            chunks.push(c);
        }
        self
    }
    pub fn and_text(self, t: &str) -> Reply {
        self.push(json!({"choices": [{"index": 0, "delta": {"content": t}}]}))
    }
    pub fn and_reasoning(self, t: &str) -> Reply {
        self.push(json!({"choices": [{"index": 0, "delta": {"reasoning_content": t}}]}))
    }
    pub fn and_call(self, name: &str, args: Value) -> Reply {
        let n = CALLS.fetch_add(1, Ordering::SeqCst);
        let index = match &self {
            Reply::Stream { chunks, .. } => chunks
                .iter()
                .filter(|c| c["choices"][0]["delta"].get("tool_calls").is_some())
                .count(),
            _ => 0,
        };
        self.push(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "id": format!("call_{n}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}]}))
    }
    pub fn finish(mut self, reason: &str) -> Reply {
        if let Reply::Stream { end, .. } = &mut self {
            *end = End::Finish(reason.into());
        }
        self
    }
    pub fn usage(mut self, input: u64, output: u64) -> Reply {
        if let Reply::Stream { usage, .. } = &mut self {
            *usage = (input, output);
        }
        self
    }
    /// Drop the connection after the chunks so far.
    pub fn then_drop(mut self) -> Reply {
        if let Reply::Stream { end, .. } = &mut self {
            *end = End::Drop;
        }
        self
    }
    /// Stall after the chunks so far.
    pub fn then_stall(mut self) -> Reply {
        if let Reply::Stream { end, .. } = &mut self {
            *end = End::Stall;
        }
        self
    }

    fn respond(&self) -> Response {
        match self {
            Reply::Status(code, body) => (
                axum::http::StatusCode::from_u16(*code).unwrap(),
                [("content-type", "application/json")],
                body.clone(),
            )
                .into_response(),
            Reply::Stream { chunks, end, usage } => {
                let mut frames: Vec<String> =
                    chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
                if let End::Finish(reason) = end {
                    frames.push(format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
                    ));
                    frames.push(format!(
                        "data: {}\n\n",
                        json!({"choices": [], "usage": {"prompt_tokens": usage.0, "completion_tokens": usage.1}})
                    ));
                    frames.push("data: [DONE]\n\n".into());
                }
                let end = end.clone();
                let stream = async_stream::stream! {
                    for f in frames {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(f));
                    }
                    match end {
                        End::Finish(_) => {}
                        End::Drop => {
                            // Give the client the frames, then cut the body short.
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            yield Err(std::io::Error::other("dropped"));
                        }
                        End::Stall => futures_util::future::pending::<()>().await,
                    }
                };
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(stream),
                )
                    .into_response()
            }
        }
    }
}

struct Rule {
    label: String,
    when: M,
    reply: Reply,
    /// Uses left; `None` is unlimited.
    left: Option<usize>,
}

/// A recorded request and the rule that answered it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub body: Value,
    pub rule: Option<String>,
}

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    seen: Vec<Seen>,
}

/// The rules a model follows, in order.
#[derive(Default)]
pub struct Script {
    rules: Vec<Rule>,
}

impl Script {
    pub fn new() -> Script {
        Script::default()
    }
    /// Title requests get `title`, every time.
    pub fn titled(self, title: &str) -> Script {
        self.always("title", M::title(), Reply::text(title))
    }
    /// A rule that answers once.
    pub fn then(mut self, label: &str, when: M, reply: Reply) -> Script {
        self.rules.push(Rule {
            label: label.into(),
            when,
            reply,
            left: Some(1),
        });
        self
    }
    /// A rule that answers every matching request.
    pub fn always(mut self, label: &str, when: M, reply: Reply) -> Script {
        self.rules.push(Rule {
            label: label.into(),
            when,
            reply,
            left: None,
        });
        self
    }
}

/// A running scripted model.
#[derive(Clone)]
pub struct Model {
    /// The OpenAI-compatible base URL (`http://127.0.0.1:PORT/v1`).
    pub url: String,
    state: Arc<Mutex<State>>,
}

async fn chat(Shared(st): Shared<Arc<Mutex<State>>>, Json(body): Json<Value>) -> Response {
    let reply = {
        let mut g = st.lock().unwrap();
        let pick = g
            .rules
            .iter()
            .position(|r| r.left != Some(0) && (r.when.0)(&body));
        let rule = pick.map(|i| {
            let r = &mut g.rules[i];
            if let Some(n) = &mut r.left {
                *n -= 1;
            }
            (r.label.clone(), r.reply.clone())
        });
        if rule.is_none() {
            eprintln!(
                "model: unscripted request: prompt {:?}, last message {:?}",
                prompt(&body),
                messages(&body).last().map(content)
            );
        }
        g.seen.push(Seen {
            body,
            rule: rule.as_ref().map(|r| r.0.clone()),
        });
        rule.map(|r| r.1)
    };
    match reply {
        Some(r) => r.respond(),
        None => Reply::status(
            400,
            r#"{"error": {"message": "unscripted request", "type": "invalid_request_error", "code": "invalid_request"}}"#,
        )
        .respond(),
    }
}

impl Model {
    pub async fn start(script: Script) -> Model {
        let state = Arc::new(Mutex::new(State {
            rules: script.rules,
            seen: vec![],
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Model {
            url: format!("http://{addr}/v1"),
            state,
        }
    }

    /// Every request so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    /// Agent-step requests (those offering tools), in order.
    pub fn steps(&self) -> Vec<Value> {
        self.seen()
            .into_iter()
            .filter(|s| s.body.get("tools").is_some())
            .map(|s| s.body)
            .collect()
    }

    /// Requests answered by the rule with this label.
    pub fn by(&self, label: &str) -> Vec<Value> {
        self.seen()
            .into_iter()
            .filter(|s| s.rule.as_deref() == Some(label))
            .map(|s| s.body)
            .collect()
    }

    /// Requests no rule matched.
    pub fn unmatched(&self) -> Vec<Value> {
        self.seen()
            .into_iter()
            .filter(|s| s.rule.is_none())
            .map(|s| s.body)
            .collect()
    }

    /// Labels of the one-shot rules not used yet.
    pub fn unused(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .rules
            .iter()
            .filter(|r| r.left == Some(1))
            .map(|r| r.label.clone())
            .collect()
    }

    /// Every request was scripted and every one-shot rule was used.
    pub fn assert_done(&self) {
        let un = self.unmatched();
        assert!(
            un.is_empty(),
            "unscripted requests: {}",
            un.iter()
                .map(|b| format!(
                    "\n- prompt {:?}, last {:?}",
                    prompt(b),
                    messages(b).last().map(content)
                ))
                .collect::<String>()
        );
        assert!(
            self.unused().is_empty(),
            "unused rules: {:?}",
            self.unused()
        );
    }
}
