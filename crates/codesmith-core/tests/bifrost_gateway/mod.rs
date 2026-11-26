//! A fake Bifrost gateway for tests: its model list per virtual key (in
//! pages), both inference surfaces streaming, the routing and request-id
//! response headers, and the virtual key's quota. Every request is kept.
//! Shared by codesmith-core's and the CLI's tests.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the gateway saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the gateway answers.
#[derive(Default)]
pub struct State {
    /// The model list each virtual key sees.
    pub models: HashMap<String, Vec<Value>>,
    /// Models per page of the list (0: all at once).
    pub page: usize,
    /// The routed provider and model to name in the response headers.
    pub routed: Option<(String, String)>,
    /// Whether to send `x-bifrost-request-id` (an older gateway doesn't).
    pub no_request_id: bool,
    /// The `usage.cost` to report (`None`: none).
    pub cost: Option<Value>,
    /// Token counts to report: input, output, cache read, cache write.
    pub tokens: (u64, u64, u64, u64),
    /// Quota answers, in turn (the last one repeats); empty: 404.
    pub quota: Vec<(u16, Value)>,
    /// Answer every inference request with this status first, this many
    /// times (a retry test).
    pub fail_first: usize,
    /// Answer the model list with this status.
    pub list_status: Option<u16>,
    /// A real gateway's pricing datasheet to serve at `/datasheet`, when
    /// this fake is its upstream.
    pub datasheet: Option<String>,
    pub seen: Vec<Seen>,
    pub quota_calls: usize,
}

#[derive(Clone)]
pub struct Gateway {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    stop: Arc<tokio::sync::Notify>,
}

impl Gateway {
    pub async fn start(state: State) -> Gateway {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(tokio::sync::Notify::new());
        let (st, sp) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    a = l.accept() => a,
                    _ = sp.notified() => return,
                };
                let Ok((s, _)) = accepted else { return };
                let st = st.clone();
                tokio::spawn(async move {
                    let _ = serve(s, st).await;
                });
            }
        });
        Gateway { url, state, stop }
    }

    /// Stop listening: the gateway is down from now on.
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn set_models(&self, key: &str, models: Vec<Value>) {
        self.state
            .lock()
            .unwrap()
            .models
            .insert(key.to_string(), models);
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

/// A listed model, Bifrost style.
pub fn model(id: &str) -> Value {
    json!({"id": id, "name": id, "context_length": 200000, "max_output_tokens": 32000})
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16384];
    let (head, mut body) = loop {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break (
                String::from_utf8_lossy(&buf[..i]).to_string(),
                buf[i + 4..].to_vec(),
            );
        }
    };
    let mut lines = head.lines();
    let first = lines.next()?.to_string();
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while body.len() < len {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Seen {
        method,
        path,
        query,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn query_param(q: &str, name: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

async fn respond(
    s: &mut tokio::net::TcpStream,
    status: u16,
    kind: &str,
    headers: &[(String, String)],
    body: &str,
) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.write_all(body.as_bytes()).await;
    let _ = s.shutdown().await;
}

async fn serve(mut s: tokio::net::TcpStream, st: Arc<Mutex<State>>) -> Option<()> {
    let req = read_request(&mut s).await?;
    st.lock().unwrap().seen.push(req.clone());
    let key = req.header("x-bf-vk").unwrap_or("").to_string();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/models") => {
            let (status, body) = {
                let g = st.lock().unwrap();
                if let Some(code) = g.list_status {
                    (code, json!({"error": {"message": "nope"}}))
                } else {
                    let list = g.models.get(&key).cloned();
                    match list {
                        None => (401, json!({"error": {"message": "virtual key not found"}})),
                        Some(all) => {
                            let start: usize = query_param(&req.query, "page_token")
                                .and_then(|t| t.parse().ok())
                                .unwrap_or(0);
                            let size = if g.page == 0 {
                                all.len().max(1)
                            } else {
                                g.page
                            };
                            let end = (start + size).min(all.len());
                            let mut v = json!({"data": all[start.min(end)..end]});
                            if end < all.len() {
                                v["next_page_token"] = json!(end.to_string());
                            }
                            (200, v)
                        }
                    }
                }
            };
            respond(&mut s, status, "application/json", &[], &body.to_string()).await;
        }
        ("GET", "/datasheet") => {
            let body = st.lock().unwrap().datasheet.clone();
            match body {
                Some(b) => respond(&mut s, 200, "application/json", &[], &b).await,
                None => respond(&mut s, 404, "application/json", &[], "{}").await,
            }
        }
        ("GET", "/api/governance/virtual-keys/quota") => {
            let answer = {
                let mut g = st.lock().unwrap();
                let i = g.quota_calls.min(g.quota.len().saturating_sub(1));
                g.quota_calls += 1;
                g.quota.get(i).cloned()
            };
            let (status, body) = answer.unwrap_or((404, json!({"error": "no"})));
            respond(&mut s, status, "application/json", &[], &body.to_string()).await;
        }
        // `/v1/messages` is Anthropic's own path, for a real gateway's
        // upstream.
        ("POST", "/v1/chat/completions")
        | ("POST", "/anthropic/v1/messages")
        | ("POST", "/v1/messages") => {
            let (fail, headers, cost, tokens) = {
                let mut g = st.lock().unwrap();
                let fail = g.fail_first > 0;
                if fail {
                    g.fail_first -= 1;
                }
                let mut h = Vec::new();
                if !g.no_request_id {
                    let id = req
                        .header("x-request-id")
                        .map(|i| format!("bf-{i}"))
                        .unwrap_or_else(|| "bf-generated".into());
                    h.push(("x-bifrost-request-id".to_string(), id));
                }
                if let Some((p, m)) = &g.routed {
                    h.push(("x-bifrost-routing-info-provider".to_string(), p.clone()));
                    h.push(("x-bifrost-routing-info-model".to_string(), m.clone()));
                }
                (fail, h, g.cost.clone(), g.tokens)
            };
            if fail {
                respond(
                    &mut s,
                    503,
                    "application/json",
                    &headers,
                    r#"{"error": {"message": "upstream overloaded"}}"#,
                )
                .await;
                return Some(());
            }
            let body = if req.path == "/v1/chat/completions" {
                chat_stream(&tokens, cost.as_ref())
            } else {
                anthropic_stream(&tokens)
            };
            respond(&mut s, 200, "text/event-stream", &headers, &body).await;
        }
        _ => respond(&mut s, 404, "application/json", &[], "{}").await,
    }
    Some(())
}

fn chat_stream(t: &(u64, u64, u64, u64), cost: Option<&Value>) -> String {
    let mut usage = json!({
        "prompt_tokens": t.0 + t.2 + t.3,
        "completion_tokens": t.1,
        "total_tokens": t.0 + t.1 + t.2 + t.3,
        // Bifrost writes both its own names and OpenAI's.
        "prompt_tokens_details": {"cached_read_tokens": t.2, "cached_tokens": t.2,
                                  "cached_write_tokens": t.3, "cache_write_tokens": t.3}
    });
    if let Some(c) = cost {
        usage["cost"] = c.clone();
    }
    let chunks = [
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "m",
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "m",
               "choices": [{"index": 0, "delta": {"content": " there."}, "finish_reason": null}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "m",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": [], "usage": usage}),
    ];
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn anthropic_stream(t: &(u64, u64, u64, u64)) -> String {
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "m",
            "usage": {"input_tokens": t.0, "output_tokens": 1, "cache_read_input_tokens": t.2, "cache_creation_input_tokens": t.3}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello from Claude."}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": t.1}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let mut out = String::new();
    for (name, data) in events {
        out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    out
}
