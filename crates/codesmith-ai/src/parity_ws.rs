//! Parity with OpenCode's Responses WebSocket transport and incremental
//! continuation: the WebSocket cases of `provider/openai-responses.test.ts`
//! (MIT, copyright the OpenCode authors). Each test is named after the
//! OpenCode test it ports; "(adapted)" marks one rephrased for CodeSmith
//! (errors carry no HTTP context, Rust strings cannot hold lone surrogates,
//! and connections are opened by tokio-tungstenite instead of a browser
//! `WebSocket`).
//!
//! The transport runs against a scripted [`Connector`] or a local
//! tokio-tungstenite server; the HTTP fallback against a local raw TCP
//! server.

use crate::ws::{
    self, continuation, Checkpoint, Connection, Connector, Create, Delivery, Driver, Frame,
    Handshake, Mode, Observation, Outbound, Phase, Recovery, Route, SessionTransport, WsError,
};
use crate::{
    assemble, Auth, Error, Media, Message, Part, Protocol, Provider, Reasoning, ReasoningMeta,
    Request, Response, Role, ToolSpec,
};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

// ---- a scripted connector ----------------------------------------------------

/// What the "server" sends after one message.
struct Reply {
    frames: Vec<Result<Frame, WsError>>,
    /// Close the inbound side after the frames (otherwise it stays open,
    /// like a socket that never closes).
    end: bool,
}

fn reply(events: &[Value]) -> Reply {
    Reply {
        frames: events
            .iter()
            .map(|e| Ok(Frame::Text(e.to_string())))
            .collect(),
        end: false,
    }
}

#[derive(Default)]
struct Script {
    opened: Vec<Handshake>,
    sent: Vec<String>,
    closed: usize,
    replies: VecDeque<Reply>,
    open_errors: VecDeque<WsError>,
    send_errors: VecDeque<WsError>,
}

#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Script>>);

impl Scripted {
    fn new(replies: Vec<Reply>) -> Self {
        let s = Scripted::default();
        s.0.lock().unwrap().replies = replies.into();
        s
    }
    fn sent(&self) -> Vec<Value> {
        let s = self.0.lock().unwrap();
        s.sent
            .iter()
            .map(|m| serde_json::from_str(m).unwrap())
            .collect()
    }
    fn opened(&self) -> Vec<Handshake> {
        self.0.lock().unwrap().opened.clone()
    }
    fn closed(&self) -> usize {
        self.0.lock().unwrap().closed
    }
    fn transport(&self) -> SessionTransport {
        SessionTransport::new(Arc::new(self.clone()))
    }
}

struct ScriptOut {
    script: Scripted,
    tx: Option<mpsc::UnboundedSender<Result<Frame, WsError>>>,
}

impl Outbound for ScriptOut {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), WsError>> {
        let mut s = self.script.0.lock().unwrap();
        if let Some(e) = s.send_errors.pop_front() {
            return Box::pin(async move { Err(e) });
        }
        s.sent.push(text);
        if let Some(r) = s.replies.pop_front() {
            if let Some(tx) = &self.tx {
                for f in r.frames {
                    let _ = tx.send(f);
                }
            }
            if r.end {
                self.tx = None;
            }
        }
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) -> BoxFuture<'_, ()> {
        self.script.0.lock().unwrap().closed += 1;
        self.tx = None;
        Box::pin(async {})
    }
}

impl Connector for Scripted {
    fn open(&self, handshake: Handshake) -> BoxFuture<'static, Result<Connection, WsError>> {
        let mut s = self.0.lock().unwrap();
        s.opened.push(handshake);
        let result = match s.open_errors.pop_front() {
            Some(e) => Err(e),
            None => {
                let (tx, inbound) = mpsc::unbounded_channel();
                Ok(Connection {
                    outbound: Box::new(ScriptOut {
                        script: self.clone(),
                        tx: Some(tx),
                    }),
                    inbound,
                })
            }
        };
        Box::pin(async move { result })
    }
}

// ---- local servers -------------------------------------------------------------

type Log<T> = Arc<Mutex<Vec<T>>>;

/// A raw HTTP server answering every request with this SSE body; it
/// records each request's head and body.
async fn http_server(sse: String) -> (String, Log<(String, Value)>) {
    let log: Log<(String, Value)> = Arc::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (seen, sse) = (seen.clone(), sse.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, body) = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
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
                            break (
                                text[..i].to_string(),
                                serde_json::from_slice(&buf[i + 4..i + 4 + len])
                                    .unwrap_or(Value::Null),
                            );
                        }
                    }
                };
                seen.lock().unwrap().push((head, body));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}/v1"), log)
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

/// A tokio-tungstenite server: it records each handshake's headers and
/// each message, and answers the n-th message with the n-th script.
async fn ws_server(script: Vec<Vec<Value>>) -> (String, Log<Vec<(String, String)>>, Log<Value>) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request as Req, Response as Resp};
    let handshakes: Log<Vec<(String, String)>> = Arc::default();
    let messages: Arc<Mutex<Vec<Value>>> = Arc::default();
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (h, m) = (handshakes.clone(), messages.clone());
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                return;
            };
            let (h, m, script) = (h.clone(), m.clone(), script.clone());
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let cb = |req: &Req, resp: Resp| {
                    let mut headers: Vec<(String, String)> = req
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    headers.push((":path".into(), req.uri().to_string()));
                    h.lock().unwrap().push(headers);
                    Ok(resp)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(s, cb).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    let tokio_tungstenite::tungstenite::Message::Text(t) = msg else {
                        continue;
                    };
                    m.lock()
                        .unwrap()
                        .push(serde_json::from_str(t.as_str()).unwrap());
                    let events = script.lock().unwrap().pop_front().unwrap_or_default();
                    for e in events {
                        let msg = tokio_tungstenite::tungstenite::Message::text(e.to_string());
                        if ws.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (format!("http://{addr}/v1"), handshakes, messages)
}

// ---- builders ------------------------------------------------------------------

fn provider(base: &str) -> Provider {
    Provider {
        protocol: Protocol::OpenaiResponses,
        base_url: base.into(),
        api_key: Some("test".into()),
        ..Default::default()
    }
}

fn request(prompt: &str) -> Request {
    Request {
        model: "gpt-4.1-mini".into(),
        messages: vec![Message::user(prompt)],
        ..Default::default()
    }
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn generate(
    t: &SessionTransport,
    p: &Provider,
    req: &Request,
    route: Route,
) -> Result<Response, Error> {
    let client = reqwest::Client::new();
    let s = t.stream(p, &client, req, route).await?;
    let r = tokio::time::timeout(Duration::from_secs(10), assemble(s, |_| {}))
        .await
        .expect("the exchange ends without the socket closing");
    r.map_err(|f| f.error)
}

fn created(id: &str) -> Value {
    json!({"type": "response.created", "response": {"id": id}})
}

fn completed(id: &str) -> Value {
    json!({"type": "response.completed", "response": {"id": id}})
}

fn prepared(p: &Provider, req: &Request, route: Route) -> ws::Prepared {
    let req = ws::resolve(p, req).unwrap();
    ws::prepare(p, &reqwest::Client::new(), &req, route).unwrap()
}

fn http_body(p: &ws::Prepared) -> Value {
    let bytes = p.http.body().and_then(|b| b.as_bytes()).unwrap();
    serde_json::from_slice(bytes).unwrap()
}

// ---- the continuation driver -----------------------------------------------------

/// OpenCode's `baseChannelObserve`: errors are provider failures, nothing
/// is checked for order.
fn base() -> continuation::Base {
    Box::new(|_create, frame, event| {
        Ok(match event["type"].as_str().unwrap_or("") {
            "response.completed" => Observation::Completed {
                frame: frame.into(),
                checkpoint: None,
            },
            "response.incomplete" => Observation::Incomplete(frame.into()),
            "error" | "response.failed" => Observation::ProviderFailure(Error::Provider {
                message: "provider rejected request".into(),
                retryable: false,
            }),
            _ => Observation::Frame(frame.into()),
        })
    })
}

/// OpenCode's `classifyingChannelObserve`: provider failures classified as
/// the production channel does.
fn classifying() -> continuation::Base {
    let mut b = base();
    Box::new(move |create, frame, event| {
        Ok(match b(create, frame, event)? {
            Observation::ProviderFailure(_) => {
                let kind = event["type"].as_str().unwrap_or("");
                Observation::ProviderFailure(
                    crate::responses::handle(&mut Default::default(), kind, event).unwrap_err(),
                )
            }
            o => o,
        })
    })
}

fn driver(request: Value) -> Driver {
    driver_with(request, base(), None)
}

fn driver_with(request: Value, base: continuation::Base, shape: Option<ws::Shape>) -> Driver {
    let Value::Object(m) = request else {
        panic!("request is an object")
    };
    Driver::new("OpenAI Responses", m, base, shape)
}

fn checkpoint(o: Observation) -> Checkpoint {
    match o {
        Observation::Completed {
            checkpoint: Some(c),
            ..
        } => c,
        other => panic!("expected a checkpoint, got {other:?}"),
    }
}

fn observe(d: &mut Driver, c: &Create, event: Value) -> Observation {
    d.observe(c, &event.to_string()).unwrap()
}

fn message(c: &Create) -> Value {
    serde_json::from_str(&c.message).unwrap()
}

fn user_item(t: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]})
}

fn first_request(model: &str, prompt: &str) -> Value {
    json!({"type": "response.create", "model": model, "store": false, "input": [user_item(prompt)]})
}

fn with_input(request: &Value, extra: &[Value]) -> Value {
    let mut r = request.clone();
    let input = r["input"].as_array_mut().unwrap();
    input.extend(extra.iter().cloned());
    r
}

#[test]
fn continues_a_streamed_tool_call_with_only_the_new_tool_output() {
    for output in [None, Some(json!([]))] {
        let first_req = first_request("gpt-5.2", "Weather?");
        let mut first = driver(first_req.clone());
        let c = first.create(None);
        observe(
            &mut first,
            &c,
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "weather", "arguments": "{ \"city\": \"Paris\" }"}}),
        );
        let mut response = json!({"id": "resp_1"});
        if let Some(o) = output {
            response["output"] = o;
        }
        let saved = checkpoint(observe(
            &mut first,
            &c,
            json!({"type": "response.completed", "response": response}),
        ));
        let output_item = json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"temperature\":22}"});
        let mut second = driver(with_input(
            &first_req,
            &[
                json!({"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"}),
                output_item.clone(),
            ],
        ));
        let create = second.create(Some(&saved));
        assert_eq!(create.mode, Mode::Incremental);
        let m = message(&create);
        assert_eq!(m["previous_response_id"], "resp_1");
        assert_eq!(m["input"], json!([output_item]));
    }
}

#[test]
fn continues_a_tool_call_from_authoritative_completed_response_output() {
    let first_req = first_request("gpt-5.2", "Weather?");
    let mut first = driver(first_req.clone());
    let c = first.create(None);
    let saved = checkpoint(observe(
        &mut first,
        &c,
        json!({"type": "response.completed", "response": {"id": "resp_1", "output": [
            {"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "weather", "arguments": "{ \"city\": \"Paris\" }"}
        ]}}),
    ));
    let output_item = json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"temperature\":22}"});
    let mut second = driver(with_input(
        &first_req,
        &[
            json!({"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"}),
            output_item.clone(),
        ],
    ));
    let create = second.create(Some(&saved));
    assert_eq!(create.mode, Mode::Incremental);
    assert_eq!(message(&create)["previous_response_id"], "resp_1");
    assert_eq!(message(&create)["input"], json!([output_item]));
}

#[test]
fn continues_a_custom_tool_call_from_completed_response_output() {
    let first_req = first_request("gpt-6-astra", "Run it.");
    let mut first = driver(first_req.clone());
    let c = first.create(None);
    let saved = checkpoint(observe(
        &mut first,
        &c,
        json!({"type": "response.completed", "response": {"id": "resp_1", "output": [
            {"type": "custom_tool_call", "id": "ctc_1", "status": "completed", "call_id": "call_1", "name": "run", "input": "ls"}
        ]}}),
    ));
    let output_item =
        json!({"type": "custom_tool_call_output", "call_id": "call_1", "output": "a.txt"});
    let mut second = driver(with_input(
        &first_req,
        &[
            json!({"type": "custom_tool_call", "call_id": "call_1", "name": "run", "input": "ls"}),
            output_item.clone(),
        ],
    ));
    let create = second.create(Some(&saved));
    assert_eq!(create.mode, Mode::Incremental);
    assert_eq!(message(&create)["input"], json!([output_item]));
}

#[test]
fn continues_a_promoted_steer_after_assistant_output_with_response_only_text_metadata() {
    for output in [None, Some(json!([]))] {
        let first_req = first_request("gpt-5.2", "First");
        let mut first = driver(first_req.clone());
        let c = first.create(None);
        observe(
            &mut first,
            &c,
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "Hello", "annotations": [], "logprobs": []}]}}),
        );
        let mut response = json!({"id": "resp_1"});
        if let Some(o) = output {
            response["output"] = o;
        }
        let saved = checkpoint(observe(
            &mut first,
            &c,
            json!({"type": "response.completed", "response": response}),
        ));
        let steer = user_item("Actually, be brief");
        let mut next = driver(with_input(
            &first_req,
            &[
                json!({"role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]}),
                steer.clone(),
            ],
        ));
        let create = next.create(Some(&saved));
        assert_eq!(create.mode, Mode::Incremental);
        assert_eq!(message(&create)["previous_response_id"], "resp_1");
        assert_eq!(message(&create)["input"], json!([steer]));
    }
}

#[test]
fn continues_streamed_reasoning_when_completion_re_encrypts_the_same_item() {
    let req = first_request("gpt-5.2", "Think");
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Thought"}], "encrypted_content": "encrypted"});
    let mut first = driver(req.clone());
    let c = first.create(None);
    observe(
        &mut first,
        &c,
        json!({"type": "response.output_item.done", "item": reasoning}),
    );
    let mut terminal = reasoning.clone();
    terminal["encrypted_content"] = json!("terminal-encrypted");
    let saved = checkpoint(observe(
        &mut first,
        &c,
        json!({"type": "response.completed", "response": {"id": "resp_1", "output": [terminal]}}),
    ));
    let cont = user_item("Continue");
    let create = driver(with_input(&req, &[reasoning.clone(), cont.clone()])).create(Some(&saved));
    assert_eq!(create.mode, Mode::Incremental);
    assert_eq!(message(&create)["input"], json!([cont]));
    let mut edited = reasoning;
    edited["encrypted_content"] = json!("edited");
    let create = driver(with_input(
        &req,
        &[edited, json!({"role": "user", "content": "Continue"})],
    ))
    .create(Some(&saved));
    assert_eq!(create.mode, Mode::Full);
}

#[test]
fn uses_a_full_request_when_any_non_input_invariant_changes() {
    let mut req = first_request("gpt-5.2", "First");
    req["metadata"] = json!({"source": "one"});
    let mut first = driver(req.clone());
    let c = first.create(None);
    let saved = checkpoint(observe(&mut first, &c, completed("resp_1")));
    let appended = with_input(&req, &[user_item("Second")]);
    let set = |k: &str, v: Value| {
        let mut r = appended.clone();
        r[k] = v;
        r
    };
    let mut rewritten = req.clone();
    rewritten["input"] = json!([user_item("Rewritten history"), user_item("Second")]);
    let changes = [
        set("model", json!("gpt-5.3")),
        set("instructions", json!("Changed")),
        set("tools", json!([{"type": "function", "name": "other"}])),
        set("temperature", json!(0.5)),
        set("metadata", json!({"source": "two"})),
        rewritten,
    ];
    for changed in changes {
        let create = driver(changed).create(Some(&saved));
        assert_eq!(create.mode, Mode::Full);
        assert!(message(&create).get("previous_response_id").is_none());
    }
}

#[test]
fn stages_no_checkpoint_for_incomplete_or_id_less_completion() {
    let mut d = driver(json!({"type": "response.create", "model": "gpt-5.2", "input": []}));
    let c = d.create(None);
    match observe(
        &mut d,
        &c,
        json!({"type": "response.completed", "response": {}}),
    ) {
        Observation::Completed { checkpoint, .. } => assert!(checkpoint.is_none()),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        observe(
            &mut d,
            &c,
            json!({"type": "response.incomplete", "response": {}})
        ),
        Observation::Incomplete(_)
    ));
}

/// (adapted) CodeSmith's errors carry no HTTP context or cause; the
/// recovery is on the observation.
#[test]
fn classifies_explicit_continuation_rejection_for_runner_owned_recovery() {
    let mut d = driver(json!({"type": "response.create", "model": "gpt-5.2", "input": []}));
    let c = d.create(None);
    let missing = observe(
        &mut d,
        &c,
        json!({"type": "error", "error": {"code": "previous_response_not_found", "message": "Missing response"}}),
    );
    let limit = observe(
        &mut d,
        &c,
        json!({"type": "error", "error": {"code": "websocket_connection_limit_reached", "message": "Rotate"}}),
    );
    for (o, want) in [
        (missing, Recovery::RetryFull),
        (limit, Recovery::RotateAndRetryFull),
    ] {
        match o {
            Observation::Rejected { error, recovery } => {
                assert_eq!(recovery, want);
                assert_eq!(error.to_string(), "provider rejected request");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn retries_an_incremental_send_in_full_when_the_provider_rejects_it_without_a_code() {
    let first_req = first_request("gpt-5.2", "First");
    let mut first = driver_with(first_req.clone(), classifying(), None);
    let c = first.create(None);
    let saved = checkpoint(observe(&mut first, &c, completed("resp_1")));
    let mut second = driver_with(
        with_input(&first_req, &[user_item("Second")]),
        classifying(),
        None,
    );
    // Codex reports a stale previous_response_id as a plain invalid request.
    let stale = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Invalid `previous_response_id`."}});
    let incremental = second.create(Some(&saved));
    assert_eq!(incremental.mode, Mode::Incremental);
    assert!(matches!(
        observe(&mut second, &incremental, stale.clone()),
        Observation::Rejected {
            recovery: Recovery::RetryFull,
            ..
        }
    ));
    // A full send has no continuation to blame.
    let full = second.create(None);
    assert!(matches!(
        observe(&mut second, &full, stale),
        Observation::ProviderFailure(_)
    ));
    // A classified failure keeps the session's own recovery.
    let overflow = json!({"type": "error", "error": {"type": "invalid_request_error", "code": "context_length_exceeded", "message": "Too long"}});
    let c = second.create(Some(&saved));
    match observe(&mut second, &c, overflow) {
        Observation::ProviderFailure(e) => {
            assert!(e.to_string().contains("context_length_exceeded"))
        }
        other => panic!("{other:?}"),
    }
    // Another failure stays one; the transport has dropped the checkpoint,
    // so the session's retry is a full send.
    let internal = json!({"type": "error", "error": {"type": "api_error", "message": "gRPC error: Response with id=resp_1 not found"}});
    let c = second.create(Some(&saved));
    assert!(matches!(
        observe(&mut second, &c, internal),
        Observation::ProviderFailure(_)
    ));
}

/// (adapted) The xAI route's own continuation shape stands for the test's
/// custom one.
#[test]
fn shapes_the_incremental_send_with_the_route_continuation() {
    let first_req = json!({"type": "response.create", "model": "grok-4.6", "store": true, "instructions": "You are terse.", "input": [user_item("First")]});
    let second_req = with_input(&first_req, &[user_item("Second")]);
    let mut first = driver(first_req.clone());
    let c = first.create(None);
    let saved = checkpoint(observe(&mut first, &c, completed("resp_1")));
    let xai = Route::for_provider("xai", "https://api.x.ai/v1");
    let chan = |req: &Value| ws::Channel {
        handshake: Handshake {
            url: String::new(),
            headers: vec![],
        },
        rotate_after: xai.rotate_after(),
        message: req.as_object().unwrap().clone(),
        route: xai,
    };
    let trimmed = chan(&second_req).driver().create(Some(&saved));
    assert_eq!(trimmed.mode, Mode::Incremental);
    assert_eq!(
        message(&trimmed),
        json!({"type": "response.create", "model": "grok-4.6", "store": true, "previous_response_id": "resp_1", "input": [user_item("Second")]})
    );
    // Declining (xAI with `store: false`) sends the step in full.
    let mut unstored = second_req.clone();
    unstored["store"] = json!(false);
    let declined = chan(&unstored).driver().create(Some(&saved));
    assert_eq!(declined.mode, Mode::Full);
    assert_eq!(message(&declined), unstored);
}

// ---- preparing both transports --------------------------------------------------

/// (adapted) CodeSmith has no route metadata; the prepared request is one
/// HTTP body and, from it, one WebSocket exchange.
#[test]
fn prepares_one_openai_responses_route_for_either_transport() {
    let p = provider("https://api.openai.com/v1/");
    let prep = prepared(&p, &request("Say hello."), Route::OpenAI);
    let body = http_body(&prep);
    assert_eq!(body["model"], "gpt-4.1-mini");
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["stream"], true);
    let chan = prep.channel.expect("OpenAI offers a WebSocket");
    assert_eq!(chan.handshake.url, "wss://api.openai.com/v1/responses");
}

#[test]
fn builds_websocket_and_http_fallback_from_the_same_final_request() {
    let mut p = provider("https://api.openai.test/v1/");
    p.query = vec![("mode".into(), "test".into())];
    let mut req = request("Say hello.");
    req.body = json!({"model": "overlaid-model", "metadata": {"source": "overlay"}, "stream_options": {"include_usage": true}, "background": true})
        .as_object()
        .unwrap()
        .clone();
    p.headers = vec![("x-request".into(), "request".into())];
    let prep = prepared(&p, &req, Route::OpenAI);
    let chan = prep.channel.as_ref().unwrap();
    assert_eq!(chan.rotate_after, Duration::from_secs(55 * 60));
    let h = &chan.handshake.headers;
    assert_eq!(header(h, "openai-beta"), Some(ws::PROTOCOL_HEADER));
    assert_eq!(header(h, "content-length"), None);
    assert_eq!(header(h, "authorization"), Some("Bearer test"));
    assert_eq!(header(h, "x-request"), Some("request"));
    assert_eq!(
        chan.handshake.url,
        "wss://api.openai.test/v1/responses?mode=test"
    );
    assert_eq!(
        prep.http.url().as_str(),
        "https://api.openai.test/v1/responses?mode=test"
    );
    let body = http_body(&prep);
    assert_eq!(body["model"], "overlaid-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["background"], true);
    let mut shared = body.as_object().unwrap().clone();
    for k in ["stream", "stream_options", "background"] {
        shared.remove(k);
    }
    shared.insert("type".into(), json!("response.create"));
    let sent: Value = serde_json::from_str(&chan.driver().create(None).message).unwrap();
    assert_eq!(sent, Value::Object(shared));
}

#[tokio::test]
async fn builds_websocket_and_http_fallback_from_the_same_final_request_over_http() {
    // A refused connect falls back to exactly one HTTP request.
    let (base, log) = http_server(sse(&[
        json!({"type": "response.completed", "response": {}}),
    ]))
    .await;
    let script = Scripted::default();
    script
        .0
        .lock()
        .unwrap()
        .open_errors
        .push_back(WsError::new("refused", Phase::Connect).delivery(Delivery::NotSent));
    let t = script.transport();
    let r = generate(&t, &provider(&base), &request("Say hello."), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(r.reason.normalized, crate::Reason::Stop);
    assert_eq!(log.lock().unwrap().len(), 1);
    assert_eq!(script.opened().len(), 1);
    assert!(t.on_http().await);
}

/// (adapted) Rust strings cannot hold lone surrogates; what both transports
/// send is the same text.
#[test]
fn sanitizes_outbound_websocket_requests_and_http_fallback_bodies() {
    let mut req = request("Say hello \u{1F600}.");
    req.body
        .insert("metadata".into(), json!({"source": "overlay\u{FFFD}"}));
    let prep = prepared(
        &provider("https://api.openai.test/v1/"),
        &req,
        Route::OpenAI,
    );
    let sent: Value =
        serde_json::from_str(&prep.channel.as_ref().unwrap().driver().create(None).message)
            .unwrap();
    let body = http_body(&prep);
    for v in [&sent, &body] {
        assert_eq!(v["input"][0]["content"][0]["text"], "Say hello \u{1F600}.");
        assert_eq!(v["metadata"]["source"], "overlay\u{FFFD}");
    }
}

#[test]
fn builds_xai_websocket_requests_without_openai_handshake_headers() {
    let p = provider("https://api.x.ai/v1");
    let route = Route::for_provider("xai", &p.base_url);
    let mut req = request("Say hello.");
    req.model = "grok-4.5".into();
    let chan = prepared(&p, &req, route).channel.unwrap();
    assert_eq!(chan.handshake.url, "wss://api.x.ai/v1/responses");
    assert_eq!(chan.rotate_after, Duration::from_secs(24 * 60));
    assert_eq!(
        header(&chan.handshake.headers, "authorization"),
        Some("Bearer test")
    );
    assert_eq!(header(&chan.handshake.headers, "openai-beta"), None);
    let m = message(&chan.driver().create(None));
    assert_eq!(m["type"], "response.create");
    assert_eq!(m["model"], "grok-4.5");
    assert_eq!(m["store"], false);
}

fn azure(
    api_version: Option<&str>,
    deployments: bool,
    base: Option<&str>,
    auth: Auth,
    key: &str,
) -> Provider {
    let base = base
        .map(str::to_string)
        .unwrap_or_else(|| crate::azure_resource_url("opencode-test", Protocol::OpenaiResponses));
    let (base_url, query) =
        crate::azure_endpoint(&base, "deployment", api_version, &[], deployments);
    Provider {
        protocol: Protocol::OpenaiResponses,
        base_url,
        query,
        api_key: Some(key.into()),
        auth,
        ..Default::default()
    }
}

/// (adapted) An Entra token is a key sent as `Authorization: Bearer`.
#[test]
fn builds_azure_websocket_requests_with_v1_urls_and_the_routes_auth_header() {
    let cases = [
        (
            azure(
                None,
                false,
                None,
                Auth::Header("api-key".into()),
                "azure-key",
            ),
            (Some("azure-key"), None),
        ),
        (
            azure(None, false, None, Auth::Default, "entra-token"),
            (None, Some("Bearer entra-token")),
        ),
    ];
    let mut req = request("Say hello.");
    req.model = "deployment".into();
    for (p, (api_key, authorization)) in cases {
        let route = Route::for_provider("azure", &p.base_url);
        let chan = prepared(&p, &req, route)
            .channel
            .expect("v1 offers a WebSocket");
        assert_eq!(
            chan.handshake.url,
            "wss://opencode-test.openai.azure.com/openai/v1/responses"
        );
        assert_eq!(chan.rotate_after, Duration::from_secs(55 * 60));
        let h = &chan.handshake.headers;
        assert_eq!(header(h, "api-key"), api_key);
        assert_eq!(header(h, "authorization"), authorization);
        assert_eq!(header(h, "openai-beta"), None);
        let m = message(&chan.driver().create(None));
        assert_eq!(m["model"], "deployment");
        assert_eq!(m["store"], false);
    }
}

/// (adapted) No warning is logged.
#[tokio::test]
async fn keeps_unsupported_azure_endpoints_and_api_versions_on_http() {
    let key = || Auth::Header("api-key".into());
    let cases = [
        (
            azure(Some("2025-04-01-preview"), false, None, key(), "k"),
            "https://opencode-test.openai.azure.com/openai/v1/responses?api-version=2025-04-01-preview",
        ),
        (
            azure(None, true, None, key(), "k"),
            "https://opencode-test.openai.azure.com/openai/deployments/deployment/responses?api-version=v1",
        ),
        (
            azure(None, false, Some("https://gateway.example/azure"), key(), "k"),
            "https://gateway.example/azure/responses",
        ),
    ];
    for (p, url) in cases {
        let prep = prepared(&p, &request("Say hello."), Route::Azure);
        assert!(prep.channel.is_none(), "{url} stays on HTTP");
        assert_eq!(prep.http.url().as_str(), url);
    }
    // Over the transport, such an endpoint never opens a socket.
    let (base, log) = http_server(sse(&[
        json!({"type": "response.completed", "response": {}}),
    ]))
    .await;
    let script = Scripted::default();
    let t = script.transport();
    let gateway = azure(None, false, Some(&base), key(), "k");
    generate(&t, &gateway, &request("hi"), Route::Azure)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(script.opened().is_empty());
}

#[tokio::test]
async fn uses_exactly_one_http_request_when_no_websocket_executor_is_supplied() {
    let (base, log) = http_server(sse(&[
        json!({"type": "response.completed", "response": {}}),
    ]))
    .await;
    let mut req = request("hi");
    req.body.insert("input".into(), json!("raw-http-input"));
    let s = provider(&base)
        .stream(&reqwest::Client::new(), &req)
        .await
        .unwrap();
    assemble(s, |_| {}).await.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].1["input"], "raw-http-input");
}

#[test]
fn prepares_the_composed_native_continuation_request() {
    let encrypted = ReasoningMeta {
        encrypted: Some("encrypted-continuation-state".into()),
        ..ReasoningMeta::of(Protocol::OpenaiResponses)
    };
    let req = Request {
        model: "gpt-5.2".into(),
        system: "You are concise. Continue from the provided history.".into(),
        messages: vec![
            Message {
                role: Role::User,
                parts: vec![
                    Part::Text("What is shown here?".into()),
                    Part::Media(Media::base64("image/png", "AAECAw==")),
                ],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Reasoning(Reasoning::new("I inspected the previous turn.", encrypted)),
                    Part::Text("It shows a small test image.".into()),
                ],
            },
            Message::user("Check the weather in Paris before continuing."),
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: "call_weather_1".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "Paris"}),
                }],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "call_weather_1".into(),
                    name: "get_weather".into(),
                    content: "{\"temperature\":22}".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("Paris is 22 degrees.".into())],
            },
            Message::user("Continue from this conversation in one short sentence."),
        ],
        tools: vec![ToolSpec {
            name: "get_weather".into(),
            description: "Weather".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }],
        ..Default::default()
    };
    let p = provider("https://api.openai.com/v1");
    let prep = prepared(&p, &req, Route::OpenAI);
    let body = http_body(&prep);
    assert!(body.get("instructions").is_none());
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let assistant = |t: &str| json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": t}]});
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "developer", "content": "You are concise. Continue from the provided history."},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "What is shown here?"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAECAw=="}
            ]},
            {"type": "reasoning", "encrypted_content": "encrypted-continuation-state", "summary": [{"type": "summary_text", "text": "I inspected the previous turn."}]},
            assistant("It shows a small test image."),
            user_item("Check the weather in Paris before continuing."),
            {"type": "function_call", "call_id": "call_weather_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_weather_1", "output": "{\"temperature\":22}"},
            assistant("Paris is 22 degrees."),
            user_item("Continue from this conversation in one short sentence."),
        ])
    );
    assert_eq!(body["tools"][0]["name"], "get_weather");
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    // The WebSocket message is the same request.
    let m = message(&prep.channel.unwrap().driver().create(None));
    assert_eq!(m["input"], body["input"]);
}

// ---- the exchange ---------------------------------------------------------------

/// (adapted) A local tokio-tungstenite server stands for the injected
/// socket; the connection stays open for the conversation's next request.
#[tokio::test]
async fn streams_openai_responses_over_websocket() {
    let (base, handshakes, messages) = ws_server(vec![vec![
        created("resp_ws"),
        json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hi"}),
        completed("resp_ws"),
    ]])
    .await;
    let mut p = provider(&base);
    p.headers = vec![("openai-beta".into(), "custom-protocol".into())];
    let t = SessionTransport::default();
    let r = generate(&t, &p, &request("Say hello."), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(r.text(), "Hi");
    let h = handshakes.lock().unwrap().clone();
    assert_eq!(h.len(), 1);
    assert_eq!(header(&h[0], ":path"), Some("/v1/responses"));
    assert_eq!(header(&h[0], "authorization"), Some("Bearer test"));
    assert_eq!(header(&h[0], "openai-beta"), Some("custom-protocol"));
    assert_eq!(
        messages.lock().unwrap().clone(),
        vec![json!({
            "type": "response.create",
            "model": "gpt-4.1-mini",
            "input": [user_item("Say hello.")],
            "store": false,
        })]
    );
    t.close().await;
}

#[tokio::test]
async fn rejects_out_of_order_and_mismatched_websocket_response_events() {
    let script = Scripted::new(vec![
        reply(&[
            json!({"type": "response.output_text.delta", "item_id": "late", "delta": "Late"}),
            completed("resp_old"),
        ]),
        reply(&[created("resp_new"), completed("resp_old")]),
    ]);
    let t = script.transport();
    let p = provider("https://api.openai.test/v1/");
    let mut errors = Vec::new();
    for prompt in ["late", "mismatch"] {
        errors.push(
            generate(&t, &p, &request(prompt), Route::OpenAI)
                .await
                .unwrap_err(),
        );
    }
    assert!(matches!(errors[0], Error::Decode(_)), "{:?}", errors[0]);
    assert!(errors[0].to_string().contains("before response.created"));
    assert!(matches!(errors[1], Error::Decode(_)));
    assert!(errors[1].to_string().contains("response ID changed"));
}

#[tokio::test]
async fn tolerates_keepalive_and_provider_notifications_before_response_created() {
    let script = Scripted::new(vec![reply(&[
        json!({"type": "keepalive", "sequence_number": 0}),
        json!({"type": "codex.rate_limits"}),
        created("resp_alive"),
        json!({"type": "response.completed", "response": {"id": "resp_alive", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ])]);
    let r = generate(
        &script.transport(),
        &provider("https://api.openai.test/v1/"),
        &request("hi"),
        Route::OpenAI,
    )
    .await
    .unwrap();
    assert_eq!(r.reason.normalized, crate::Reason::Stop);
}

/// (adapted) The conversation's connection is dropped when a response is
/// abandoned part way, and the next request opens a new one.
#[tokio::test]
async fn closes_a_direct_websocket_execution_after_partial_consumption() {
    let script = Scripted::new(vec![
        reply(&[
            created("resp_ws"),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hi"}),
            completed("resp_ws"),
        ]),
        reply(&[created("resp_2"), completed("resp_2")]),
    ]);
    let t = script.transport();
    let p = provider("https://api.openai.test/v1/");
    let client = reqwest::Client::new();
    let mut s = t
        .stream(&p, &client, &request("Say hello."), Route::OpenAI)
        .await
        .unwrap();
    s.next().await.unwrap().unwrap();
    drop(s);
    generate(&t, &p, &request("again"), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(script.opened().len(), 2);
    assert_eq!(script.sent()[1]["previous_response_id"], Value::Null);
}

/// (adapted) CodeSmith classifies these as provider errors (retryable or
/// not) rather than OpenCode's reason tags.
#[tokio::test]
async fn terminates_websocket_control_events_without_waiting_for_the_socket_to_close() {
    let events = [
        (
            json!({"type": "error", "error": {"code": "slow_down", "message": "Try later"}}),
            "slow_down: Try later",
        ),
        (
            json!({"type": "error", "status_code": 429, "message": "Rate limited", "headers": {"retry-after": 1}}),
            "Rate limited",
        ),
        (
            json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": "Unavailable"}}}),
            "server_error: Unavailable",
        ),
        (
            json!({"type": "error", "status": "not-a-status", "message": "Malformed status"}),
            "Malformed status",
        ),
    ];
    for (event, text) in events {
        let script = Scripted::new(vec![reply(&[event])]);
        let t = script.transport();
        let e = generate(
            &t,
            &provider("https://api.openai.test/v1/"),
            &request("Say hello."),
            Route::OpenAI,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, Error::Provider { .. }), "{e:?}");
        assert!(e.to_string().contains(text), "{e}");
        // The connection is dropped after an error frame.
        assert_eq!(script.closed(), 1);
    }
}

#[tokio::test]
async fn marks_post_send_websocket_failures_with_delivery_state() {
    let failure = || Err(WsError::new("socket closed", Phase::Close));
    let script = Scripted::new(vec![
        Reply {
            frames: vec![failure()],
            end: true,
        },
        Reply {
            frames: vec![
                Ok(Frame::Text(created("resp_observed").to_string())),
                failure(),
            ],
            end: true,
        },
    ]);
    let t = script.transport();
    let p = provider("https://api.openai.test/v1/");
    let mut seen = Vec::new();
    for prompt in ["first", "second"] {
        let e = generate(&t, &p, &request(prompt), Route::OpenAI)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Transport(_)));
        let f = t.last_failure().await.unwrap();
        seen.push((f.phase, f.delivery));
    }
    assert_eq!(
        seen,
        vec![
            (Phase::Close, Some(Delivery::Ambiguous)),
            (Phase::Close, Some(Delivery::Accepted)),
        ]
    );
}

/// (adapted) A server that closes the connection before the upgrade
/// completes stands for a socket already closed.
#[tokio::test]
async fn fails_immediately_when_websocket_is_already_closed() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            drop(s);
        }
    });
    let e = ws::Tungstenite::default()
        .open(Handshake {
            url: format!("ws://{addr}/v1/responses"),
            headers: vec![],
        })
        .await
        .err()
        .expect("the upgrade fails");
    assert_eq!(e.phase, Phase::Connect);
    assert_eq!(e.delivery, Some(Delivery::NotSent));
}

/// (adapted) A refused TCP connect stands for the DNS failure.
#[tokio::test]
async fn keeps_the_system_code_when_a_websocket_fails_to_connect() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let e = ws::Tungstenite::default()
        .open(Handshake {
            url: format!("ws://{addr}/v1/responses"),
            headers: vec![],
        })
        .await
        .err()
        .expect("nothing listens");
    assert_eq!(e.code.as_deref(), Some("ECONNREFUSED"));
    assert_eq!(e.phase, Phase::Connect);
    assert_eq!(e.delivery, Some(Delivery::NotSent));
}

// ---- beyond the ported cases ---------------------------------------------------

/// A conversation reuses its connection and continues each completed
/// response with only the new input; a refused continuation is resent in
/// full on a new connection.
#[tokio::test]
async fn reuses_the_connection_and_recovers_a_rejected_continuation() {
    let script = Scripted::new(vec![
        reply(&[
            created("resp_1"),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "m1", "role": "assistant", "content": [{"type": "output_text", "text": "One"}]}}),
            completed("resp_1"),
        ]),
        reply(&[created("resp_2"), completed("resp_2")]),
        reply(&[
            json!({"type": "error", "error": {"code": "previous_response_not_found", "message": "gone"}}),
        ]),
        reply(&[created("resp_3"), completed("resp_3")]),
    ]);
    let t = script.transport();
    let p = provider("https://api.openai.test/v1/");
    let mut req = request("First");
    generate(&t, &p, &req, Route::OpenAI).await.unwrap();
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::Text("One".into())],
    });
    req.messages.push(Message::user("Second"));
    generate(&t, &p, &req, Route::OpenAI).await.unwrap();
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::Text("Two".into())],
    });
    req.messages.push(Message::user("Third"));
    generate(&t, &p, &req, Route::OpenAI).await.unwrap();
    let sent = script.sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[1]["previous_response_id"], "resp_1");
    assert_eq!(sent[1]["input"], json!([user_item("Second")]));
    assert_eq!(sent[2]["previous_response_id"], "resp_2");
    assert_eq!(sent[3].get("previous_response_id"), None);
    assert_eq!(sent[3]["input"].as_array().unwrap().len(), 5);
    assert_eq!(script.opened().len(), 2);
}

/// A connection is replaced when its handshake changes; repeated socket
/// failures keep the conversation on HTTP.
#[tokio::test]
async fn rotates_on_new_headers_and_falls_back_after_repeated_failures() {
    let script = Scripted::new(vec![
        reply(&[created("a"), completed("a")]),
        reply(&[created("b"), completed("b")]),
    ]);
    let t = script.transport();
    let mut p = provider("https://api.openai.test/v1/");
    generate(&t, &p, &request("hi"), Route::OpenAI)
        .await
        .unwrap();
    p.headers = vec![("x-other".into(), "1".into())];
    generate(&t, &p, &request("hi"), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(script.opened().len(), 2);
    assert_eq!(script.closed(), 1);

    let script = Scripted::default();
    for _ in 0..5 {
        script.0.lock().unwrap().replies.push_back(Reply {
            frames: vec![Err(WsError::new("reset", Phase::Receive))],
            end: true,
        });
    }
    let t = script.transport();
    let (base, log) = http_server(sse(&[
        json!({"type": "response.completed", "response": {}}),
    ]))
    .await;
    let p = provider(&base);
    for _ in 0..5 {
        assert!(generate(&t, &p, &request("hi"), Route::OpenAI)
            .await
            .is_err());
    }
    assert!(t.on_http().await);
    generate(&t, &p, &request("hi"), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
}

/// The provider closing the socket with 1009 (message too big) sends this
/// request and the rest over HTTP.
#[tokio::test]
async fn falls_back_to_http_when_the_message_is_too_large() {
    let script = Scripted::new(vec![Reply {
        frames: vec![Err(WsError::new("too big", Phase::Close).code("1009"))],
        end: true,
    }]);
    let t = script.transport();
    let (base, log) = http_server(sse(&[
        json!({"type": "response.completed", "response": {}}),
    ]))
    .await;
    generate(&t, &provider(&base), &request("hi"), Route::OpenAI)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(t.on_http().await);
}

#[test]
fn converts_http_urls_to_websocket_urls() {
    assert_eq!(
        ws::to_websocket_url("https://a.test/v1/responses?x=1").unwrap(),
        "wss://a.test/v1/responses?x=1"
    );
    assert_eq!(
        ws::to_websocket_url("http://127.0.0.1:1/v1/responses").unwrap(),
        "ws://127.0.0.1:1/v1/responses"
    );
    let e = ws::to_websocket_url("ftp://a.test/x").unwrap_err();
    assert_eq!(e.code.as_deref(), Some("invalid-url"));
    assert_eq!(e.phase, Phase::Prepare);
}
