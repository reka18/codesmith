//! A fake Bifrost gateway for tests: its model list per virtual key (in
//! pages), every inference surface streaming (the unified `/v1`, the
//! `/openai`, `/anthropic` and `/genai` integrations, or any mount ending
//! in the same paths), the routing and request-id response headers, and
//! the virtual key's quota. It can refuse routes, as a gateway that serves
//! only Bifrost's integration routes does. Every request is kept. Shared
//! by codesmith-core's and the CLI's tests.
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
    /// Answer with this tool call (name, JSON arguments) instead of text.
    pub tool_call: Option<(String, Value)>,
    /// Paths answered with this status and body, whatever the method
    /// (a route the gateway doesn't serve).
    pub refused: Vec<(String, u16, String)>,
    /// Every POST answered with this status and body.
    pub refuse_posts: Option<(u16, String)>,
    /// Where the model list is served (default `/v1/models`).
    pub models_path: Option<String>,
    /// Where the quota is served (default
    /// `/api/governance/virtual-keys/quota`).
    pub quota_path: Option<String>,
    pub seen: Vec<Seen>,
    pub quota_calls: usize,
}

#[derive(Clone)]
pub struct Gateway {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    down: Arc<std::sync::atomic::AtomicBool>,
}

impl Gateway {
    pub async fn start(state: State) -> Gateway {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let state = Arc::new(Mutex::new(state));
        let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (st, dn) = (state.clone(), down.clone());
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                if dn.load(std::sync::atomic::Ordering::SeqCst) {
                    // Down: the connection is closed unanswered.
                    drop(s);
                    continue;
                }
                let st = st.clone();
                tokio::spawn(async move {
                    let _ = serve(s, st).await;
                });
            }
        });
        Gateway { url, state, down }
    }

    /// The gateway is down from now on: every new connection is closed
    /// unanswered. The port stays held, so another test's gateway can't
    /// take it and answer in its place (a freed port can be reused at once).
    pub fn stop(&self) {
        self.down.store(true, std::sync::atomic::Ordering::SeqCst);
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

/// What a hostname that serves inference only through Bifrost's
/// integration routes answers on the unified ones.
pub const INFERENCE_ONLY: &str = r#"{"error":"This hostname serves inference only. Use the admin hostname for administration."}"#;

impl State {
    /// A gateway serving inference only on Bifrost's integration routes:
    /// the unified `/v1/chat/completions` and `/v1/responses` answer 403,
    /// while `/v1/models`, the quota, `/openai/v1/…`, `/anthropic/v1/messages`
    /// and `/genai/…` answer.
    pub fn integration_only(mut self) -> State {
        for p in ["/v1/chat/completions", "/v1/responses"] {
            self.refused.push((p.into(), 403, INFERENCE_ONLY.into()));
        }
        self
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
    let (refused, models_path, quota_path) = {
        let g = st.lock().unwrap();
        (
            g.refused
                .iter()
                .find(|(p, _, _)| *p == req.path)
                .map(|(_, c, b)| (*c, b.clone()))
                .or_else(|| g.refuse_posts.clone().filter(|_| req.method == "POST")),
            g.models_path.clone().unwrap_or_else(|| "/v1/models".into()),
            g.quota_path
                .clone()
                .unwrap_or_else(|| "/api/governance/virtual-keys/quota".into()),
        )
    };
    if let Some((code, body)) = refused {
        respond(&mut s, code, "application/json", &[], &body).await;
        return Some(());
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", p) if p == models_path => {
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
        // Gemini's own model list, for a real gateway's upstream.
        ("GET", p) if p.ends_with("/models") => {
            let models = st
                .lock()
                .unwrap()
                .models
                .get("")
                .cloned()
                .unwrap_or_default();
            let list: Vec<Value> = models
                .iter()
                .filter_map(|m| m["id"].as_str())
                .filter(|id| id.starts_with("test-gemini"))
                .map(|id| {
                    json!({"name": format!("models/{id}"), "displayName": id,
                                 "supportedGenerationMethods": ["generateContent"]})
                })
                .collect();
            let body = json!({"models": list}).to_string();
            respond(&mut s, 200, "application/json", &[], &body).await;
        }
        ("GET", "/datasheet") => {
            let body = st.lock().unwrap().datasheet.clone();
            match body {
                Some(b) => respond(&mut s, 200, "application/json", &[], &b).await,
                None => respond(&mut s, 404, "application/json", &[], "{}").await,
            }
        }
        ("GET", p) if p == quota_path => {
            let answer = {
                let mut g = st.lock().unwrap();
                let i = g.quota_calls.min(g.quota.len().saturating_sub(1));
                g.quota_calls += 1;
                g.quota.get(i).cloned()
            };
            let (status, body) = answer.unwrap_or((404, json!({"error": "no"})));
            respond(&mut s, status, "application/json", &[], &body.to_string()).await;
        }
        // Any mount of the four APIs: Bifrost's unified `/v1/…`, its
        // `/openai`, `/anthropic` and `/genai` integrations, a gateway that
        // mounts them elsewhere, and (for a real gateway's upstream)
        // Anthropic's and Gemini's own paths.
        ("POST", p)
            if p.ends_with("/chat/completions")
                || p.ends_with("/v1/messages")
                || p.ends_with("/responses")
                || p.ends_with(":streamGenerateContent") =>
        {
            let (fail, headers, cost, tokens, tool) = {
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
                (fail, h, g.cost.clone(), g.tokens, g.tool_call.clone())
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
            let tool = tool.as_ref();
            // Bifrost's OpenAI integration writes `usage.cost` as the bare
            // total, as OpenAI's SDKs expect; its unified API, as an object.
            let cost = match cost {
                Some(c) if req.path.contains("/openai/") => Some(c["total_cost"].clone()),
                c => c,
            };
            let body = match req.path.as_str() {
                p if p.ends_with("/chat/completions") => chat_stream(&tokens, cost.as_ref(), tool),
                p if p.ends_with("/responses") => responses_stream(&tokens, cost.as_ref(), tool),
                p if p.ends_with(":streamGenerateContent") => gemini_stream(&tokens, tool),
                _ => anthropic_stream(&tokens, tool),
            };
            respond(&mut s, 200, "text/event-stream", &headers, &body).await;
        }
        _ => respond(&mut s, 404, "application/json", &[], "{}").await,
    }
    Some(())
}

fn sse(events: Vec<(&str, Value)>) -> String {
    let mut out = String::new();
    for (name, data) in events {
        if name.is_empty() {
            out.push_str(&format!("data: {data}\n\n"));
        } else {
            out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
        }
    }
    out
}

fn chat_stream(
    t: &(u64, u64, u64, u64),
    cost: Option<&Value>,
    tool: Option<&(String, Value)>,
) -> String {
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
    let chunk = |choices: Value| json!({"id": "c1", "object": "chat.completion.chunk", "model": "m", "choices": choices});
    let mut chunks = match tool {
        None => vec![
            chunk(
                json!([{"index": 0, "delta": {"role": "assistant", "content": "Hello"}, "finish_reason": null}]),
            ),
            chunk(json!([{"index": 0, "delta": {"content": " there."}, "finish_reason": null}])),
            chunk(json!([{"index": 0, "delta": {}, "finish_reason": "stop"}])),
        ],
        Some((name, args)) => vec![
            chunk(
                json!([{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1",
                "type": "function", "function": {"name": name, "arguments": ""}}]}, "finish_reason": null}]),
            ),
            chunk(json!([{"index": 0, "delta": {"tool_calls": [{"index": 0,
                "function": {"arguments": args.to_string()}}]}, "finish_reason": null}])),
            chunk(json!([{"index": 0, "delta": {}, "finish_reason": "tool_calls"}])),
        ],
    };
    let mut last = chunk(json!([]));
    last["usage"] = usage;
    chunks.push(last);
    let mut out = sse(chunks.into_iter().map(|c| ("", c)).collect());
    out.push_str("data: [DONE]\n\n");
    out
}

fn anthropic_stream(t: &(u64, u64, u64, u64), tool: Option<&(String, Value)>) -> String {
    let (block, delta, stop) = match tool {
        None => (
            json!({"type": "text", "text": ""}),
            json!({"type": "text_delta", "text": "Hello from Claude."}),
            "end_turn",
        ),
        Some((name, args)) => (
            json!({"type": "tool_use", "id": "toolu_1", "name": name, "input": {}}),
            json!({"type": "input_json_delta", "partial_json": args.to_string()}),
            "tool_use",
        ),
    };
    sse(vec![
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
            "role": "assistant", "content": [], "model": "m",
            "usage": {"input_tokens": t.0, "output_tokens": 1, "cache_read_input_tokens": t.2,
                      "cache_creation_input_tokens": t.3}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": block}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": delta}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": stop},
            "usage": {"output_tokens": t.1}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ])
}

fn responses_stream(
    t: &(u64, u64, u64, u64),
    cost: Option<&Value>,
    tool: Option<&(String, Value)>,
) -> String {
    let mut usage = json!({
        "input_tokens": t.0 + t.2 + t.3,
        "input_tokens_details": {"cached_tokens": t.2, "cache_write_tokens": t.3},
        "output_tokens": t.1,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": t.0 + t.1 + t.2 + t.3
    });
    if let Some(c) = cost {
        usage["cost"] = c.clone();
    }
    let mut events = vec![(
        "response.created",
        json!({"type": "response.created", "sequence_number": 0,
               "response": {"id": "resp_1", "object": "response", "status": "in_progress", "output": []}}),
    )];
    let done_item = match tool {
        None => {
            events.push((
                "response.output_item.added",
                json!({"type": "response.output_item.added",
                "output_index": 0, "item": {"id": "msg_1", "type": "message", "role": "assistant",
                "status": "in_progress", "content": []}}),
            ));
            events.push((
                "response.content_part.added",
                json!({"type": "response.content_part.added",
                "item_id": "msg_1", "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}}),
            ));
            for d in ["Hello", " there."] {
                events.push((
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta",
                    "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": d}),
                ));
            }
            events.push((
                "response.output_text.done",
                json!({"type": "response.output_text.done",
                "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": "Hello there."}),
            ));
            json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                   "content": [{"type": "output_text", "text": "Hello there.", "annotations": []}]})
        }
        Some((name, args)) => {
            events.push(("response.output_item.added", json!({"type": "response.output_item.added",
                "output_index": 0, "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                "name": name, "arguments": "", "status": "in_progress"}})));
            events.push((
                "response.function_call_arguments.delta",
                json!({"type":
                "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0,
                "delta": args.to_string()}),
            ));
            events.push((
                "response.function_call_arguments.done",
                json!({"type":
                "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 0,
                "arguments": args.to_string()}),
            ));
            json!({"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": name,
                   "arguments": args.to_string(), "status": "completed"})
        }
    };
    events.push((
        "response.output_item.done",
        json!({"type": "response.output_item.done",
        "output_index": 0, "item": done_item.clone()}),
    ));
    events.push((
        "response.completed",
        json!({"type": "response.completed",
        "response": {"id": "resp_1", "object": "response", "status": "completed",
                     "output": [done_item], "usage": usage}}),
    ));
    sse(events)
}

fn gemini_stream(t: &(u64, u64, u64, u64), tool: Option<&(String, Value)>) -> String {
    let usage = json!({"promptTokenCount": t.0 + t.2, "candidatesTokenCount": t.1,
                       "cachedContentTokenCount": t.2, "totalTokenCount": t.0 + t.1 + t.2});
    let chunks = match tool {
        None => vec![
            json!({"candidates": [{"content": {"parts": [{"text": "Hello"}], "role": "model"}, "index": 0}]}),
            json!({"candidates": [{"content": {"parts": [{"text": " from Gemini."}], "role": "model"},
                   "finishReason": "STOP", "index": 0}], "usageMetadata": usage}),
        ],
        Some((name, args)) => vec![
            json!({"candidates": [{"content": {"parts": [{"functionCall":
            {"name": name, "args": args}}], "role": "model"}, "finishReason": "STOP", "index": 0}],
            "usageMetadata": usage}),
        ],
    };
    sse(chunks.into_iter().map(|c| ("", c)).collect())
}
