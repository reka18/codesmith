//! Compaction over a conversation's Responses WebSocket, ported from
//! OpenCode's `session-checkpoint-transport.test.ts` and
//! `session-compaction-transport.test.ts` (MIT). The server takes
//! WebSocket upgrades and plain HTTP on one port; the tests decide what
//! each socket message gets back, when upgrades are refused, and when a
//! socket closes.

use codesmith_ai::ws::{Route, SessionTransport};
use codesmith_ai::{Auth, CompactMechanism, Encoding, Message, Part, Protocol, Provider, Request};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as M;

type Responder = Box<dyn Fn(usize, &Value) -> Option<Vec<Value>> + Send>;

struct State {
    /// Bodies sent over WebSocket, with their connection number.
    socket: Vec<(usize, Value)>,
    http: Vec<Value>,
    connections: usize,
    reject_upgrade: bool,
    /// Close the current socket (an idle disconnect).
    close: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// The frames for the nth socket message; `None` closes the socket
    /// after receipt.
    respond: Responder,
    /// The HTTP reply's events.
    http_reply: Vec<Value>,
}

fn state(respond: Responder, http_reply: Vec<Value>) -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State {
        socket: vec![],
        http: vec![],
        connections: 0,
        reject_upgrade: false,
        close: None,
        respond,
        http_reply,
    }))
}

async fn serve(st: Arc<Mutex<State>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let st = st.clone();
            tokio::spawn(async move {
                let mut head = [0u8; 4096];
                let n = s.peek(&mut head).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&head[..n]).to_lowercase();
                if text.contains("upgrade: websocket") {
                    websocket(s, st).await;
                } else {
                    http(s, st).await;
                }
            });
        }
    });
    url
}

async fn websocket(mut s: tokio::net::TcpStream, st: Arc<Mutex<State>>) {
    if st.lock().unwrap().reject_upgrade {
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let body = "Upgrade rejected";
        let _ = s
            .write_all(
                format!(
                    "HTTP/1.1 503 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return;
    }
    let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
        return;
    };
    let (tx, mut closed) = tokio::sync::mpsc::unbounded_channel();
    let conn = {
        let mut g = st.lock().unwrap();
        g.connections += 1;
        g.close = Some(tx);
        g.connections
    };
    loop {
        tokio::select! {
            _ = closed.recv() => {
                let _ = ws.close(None).await;
                return;
            }
            msg = ws.next() => {
                let Some(Ok(M::Text(t))) = msg else { return };
                let body: Value = serde_json::from_str(t.as_str()).unwrap();
                let reply = {
                    let mut g = st.lock().unwrap();
                    g.socket.push((conn, body.clone()));
                    let n = g.socket.len();
                    (g.respond)(n, &body)
                };
                let Some(frames) = reply else {
                    let _ = ws.close(None).await;
                    return;
                };
                for f in frames {
                    if ws.send(M::text(f.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn http(mut s: tokio::net::TcpStream, st: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    let body = loop {
        let n = s.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        if buf.len() >= i + 4 + len {
            break serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len]).unwrap();
        }
    };
    let events = {
        let mut g = st.lock().unwrap();
        g.http.push(body);
        g.http_reply.clone()
    };
    let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let _ = s
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            )
            .as_bytes(),
        )
        .await;
}

fn provider(base: &str) -> Provider {
    Provider {
        protocol: Protocol::OpenaiResponses,
        name: "fixture".into(),
        base_url: base.into(),
        api_key: Some("fixture".into()),
        headers: vec![("chatgpt-account-id".into(), "account".into())],
        query: vec![],
        auth: Auth::Default,
        encoding: Encoding::None,
        compat: Default::default(),
        hooks: None,
    }
}

fn request(messages: Vec<Message>) -> Request {
    Request {
        model: "fixture".into(),
        messages,
        prompt_cache_key: Some("session-key".into()),
        ..Default::default()
    }
}

const CHECKPOINT: &str = "opaque";

fn checkpoint() -> Value {
    json!({"type": "compaction", "encrypted_content": CHECKPOINT})
}

fn hello(n: usize) -> Vec<Value> {
    let id = format!("resp_{n}");
    let item = json!({"type": "message", "id": format!("msg_{n}"), "role": "assistant",
        "content": [{"type": "output_text", "text": "Hello"}]});
    vec![
        json!({"type": "response.created", "response": {"id": id}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        json!({"type": "response.output_text.delta", "item_id": format!("msg_{n}"), "delta": "Hello"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": id, "output": [item]}}),
    ]
}

/// Hello for a step, a checkpoint (or two, `invalid`) for a trigger.
fn checkpointing(invalid: bool) -> Responder {
    Box::new(move |n, body| {
        let triggered = body["input"]
            .as_array()
            .and_then(|i| i.last())
            .is_some_and(|i| i["type"] == "compaction_trigger");
        if !triggered {
            return Some(hello(n));
        }
        let id = format!("resp_{n}");
        let output = if invalid {
            json!([checkpoint(), {"type": "compaction", "encrypted_content": "second"}])
        } else {
            json!([checkpoint()])
        };
        let mut frames = vec![
            json!({"type": "response.created", "response": {"id": id}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": checkpoint()}),
        ];
        if invalid {
            frames.push(json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "compaction", "encrypted_content": "second"}}));
        }
        frames.push(
            json!({"type": "response.completed", "response": {"id": id, "output": output,
            "usage": {"input_tokens": 100, "output_tokens": 10}}}),
        );
        Some(frames)
    })
}

async fn assistant(t: &SessionTransport, p: &Provider, req: &Request) -> Message {
    let client = reqwest::Client::new();
    let s = t.stream(p, &client, req, Route::OpenAI).await.unwrap();
    let r = codesmith_ai::assemble(s, |_| {}).await.unwrap();
    Message {
        role: codesmith_ai::Role::Assistant,
        parts: r.parts,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_trigger_reuses_the_append_baseline_and_clears_it_before_the_next_generation() {
    let st = state(checkpointing(false), vec![]);
    let base = serve(st.clone()).await;
    let p = provider(&base);
    let t = SessionTransport::default();
    let client = reqwest::Client::new();
    let first = request(vec![Message::user("First")]);
    let reply = assistant(&t, &p, &first).await;
    let compacted = t
        .compact(
            &p,
            &client,
            &request(vec![Message::user("First"), reply]),
            Route::OpenAI,
        )
        .await
        .unwrap();
    {
        let g = st.lock().unwrap();
        let second = &g.socket[1].1;
        assert_eq!(second["previous_response_id"], "resp_1");
        assert_eq!(second["input"], json!([{"type": "compaction_trigger"}]));
        assert!(second.get("stream").is_none());
        assert_eq!(second["store"], false);
    }
    assert_eq!((compacted.usage.input, compacted.usage.output), (100, 10));
    let window = compacted.window;
    assert!(matches!(&window[0].parts[0], Part::Compaction(c) if c.encrypted == CHECKPOINT));
    // The checkpoint installs a new window: the next request goes in full.
    let mut next = window.clone();
    next.push(Message::user("Continue"));
    assistant(&t, &p, &request(next)).await;
    let g = st.lock().unwrap();
    let third = &g.socket[2].1;
    assert!(third.get("previous_response_id").is_none(), "{third}");
    let input = third["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "compaction");
    assert_eq!(input[0]["encrypted_content"], CHECKPOINT);
    assert_eq!(input[1]["role"], "user");
    assert_eq!(g.connections, 1);
    assert!(g.http.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_invalid_trigger_output_does_not_retry_or_commit_a_continuation_checkpoint() {
    let st = state(checkpointing(true), vec![]);
    let base = serve(st.clone()).await;
    let p = provider(&base);
    let t = SessionTransport::default();
    let client = reqwest::Client::new();
    let reply = assistant(&t, &p, &request(vec![Message::user("First")])).await;
    let input = request(vec![Message::user("First"), reply]);
    let e = t
        .compact(&p, &client, &input, Route::OpenAI)
        .await
        .unwrap_err();
    assert!(matches!(e, codesmith_ai::Error::Decode(_)), "{e}");
    assert!(!e.retryable());
    assert_eq!(st.lock().unwrap().socket.len(), 2);
    assistant(&t, &p, &input).await;
    let g = st.lock().unwrap();
    let third = &g.socket[2].1;
    assert!(third.get("previous_response_id").is_none(), "{third}");
    assert_eq!(third["input"].as_array().unwrap().len(), 2);
    assert!(g.http.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_trigger_recovers_over_sse_with_complete_input_after_the_socket_closes_and_reconnect_is_rejected(
) {
    let http_reply = vec![
        json!({"type": "response.completed", "response": {"id": "resp_http",
        "output": [checkpoint()], "usage": {"input_tokens": 70, "output_tokens": 7}}}),
    ];
    let st = state(checkpointing(false), http_reply);
    let base = serve(st.clone()).await;
    let p = provider(&base);
    let t = SessionTransport::default();
    let client = reqwest::Client::new();
    let reply = assistant(&t, &p, &request(vec![Message::user("First")])).await;
    {
        let mut g = st.lock().unwrap();
        g.reject_upgrade = true;
        let _ = g.close.as_ref().unwrap().send(());
    }
    // Let the close reach the client before the next send.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let compacted = t
        .compact(
            &p,
            &client,
            &request(vec![Message::user("First"), reply]),
            Route::OpenAI,
        )
        .await
        .unwrap();
    assert_eq!((compacted.usage.input, compacted.usage.output), (70, 7));
    let g = st.lock().unwrap();
    assert_eq!(g.http.len(), 1);
    let body = &g.http[0];
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
    assert_eq!(g.socket.len(), 1);
}

/// A step whose response compacts in-stream (context management): the
/// first carries a checkpoint, later ones nothing; the third message gets
/// the socket closed after receipt when `ambiguous`.
fn in_stream(ambiguous: bool) -> Responder {
    Box::new(move |n, _| {
        if ambiguous && n == 3 {
            return None;
        }
        let id = format!("resp_{n}");
        let cp =
            json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "opaque-context"});
        let mut frames = vec![json!({"type": "response.created", "response": {"id": id}})];
        let output = if n == 1 {
            frames
                .push(json!({"type": "response.output_item.done", "output_index": 0, "item": cp}));
            json!([cp])
        } else {
            json!([])
        };
        frames
            .push(json!({"type": "response.completed", "response": {"id": id, "output": output}}));
        Some(frames)
    })
}

async fn survives(recovery: &str) {
    let http_reply =
        vec![json!({"type": "response.completed", "response": {"id": "resp_http", "output": []}})];
    let st = state(in_stream(recovery == "ambiguous-send"), http_reply);
    let base = serve(st.clone()).await;
    let p = provider(&base);
    let t = SessionTransport::default();
    let client = reqwest::Client::new();
    let mut messages = vec![Message::user("first")];
    let compacted = assistant(&t, &p, &request(messages.clone())).await;
    assert!(compacted
        .parts
        .iter()
        .any(|p| matches!(p, Part::Compaction(_))));
    messages.push(compacted);
    messages.push(Message::user("second"));
    let second = assistant(&t, &p, &request(messages.clone())).await;
    {
        let g = st.lock().unwrap();
        let body = &g.socket[1].1;
        assert_eq!(body["previous_response_id"], "resp_1", "{body}");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"].as_array().unwrap().len(), 1, "{body}");
        assert_eq!(body["input"][0]["content"][0]["text"], "second");
    }
    messages.push(second);
    messages.push(Message::user("third"));
    let third = request(messages.clone());
    if recovery == "ambiguous-send" {
        let s = t.stream(&p, &client, &third, Route::OpenAI).await.unwrap();
        let e = codesmith_ai::assemble(s, |_| {}).await.unwrap_err();
        assert!(
            e.error.to_string().to_lowercase().contains("closed"),
            "{}",
            e.error
        );
        let g = st.lock().unwrap();
        assert!(g.http.is_empty());
        assert_eq!(g.socket.len(), 3);
        drop(g);
        st.lock().unwrap().reject_upgrade = true;
    } else {
        let mut g = st.lock().unwrap();
        g.reject_upgrade = recovery == "http-fallback";
        let _ = g.close.as_ref().unwrap().send(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assistant(&t, &p, &third).await;
    let g = st.lock().unwrap();
    let body = if recovery == "reconnect" {
        g.socket.last().unwrap().1.clone()
    } else {
        g.http[0].clone()
    };
    assert!(body.get("previous_response_id").is_none(), "{body}");
    assert_eq!(body["store"], false);
    let input = body["input"].as_array().unwrap();
    let kinds: Vec<String> = input
        .iter()
        .map(|i| match i["type"].as_str() {
            Some("compaction") => "compaction".to_string(),
            _ => i["content"][0]["text"].as_str().unwrap_or("?").to_string(),
        })
        .collect();
    assert_eq!(kinds, ["first", "compaction", "second", "third"], "{body}");
    assert_eq!(g.http.len(), if recovery == "reconnect" { 0 } else { 1 });
    assert_eq!(g.connections, if recovery == "reconnect" { 2 } else { 1 });
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_survives_reconnect_without_stale_response_ids() {
    survives("reconnect").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_survives_http_fallback_without_stale_response_ids() {
    survives("http-fallback").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_survives_ambiguous_send_without_stale_response_ids() {
    survives("ambiguous-send").await;
}

#[test]
fn trigger_is_a_compaction_mechanism() {
    // The HTTP path stays: providers without a socket compact over SSE.
    assert!(provider("http://x/v1").can_compact(CompactMechanism::Trigger));
}
