//! A scripted OpenAI Chat model for the compaction ports: main replies
//! report the prompt sizes a test sets, summary requests can be rejected
//! as too long or held back, and every request body is logged.

#![allow(dead_code)]

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::title;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Title,
    Summary,
    Main,
}

/// A main reply: its text, reasoning, and the prompt tokens it reports.
#[derive(Debug, Clone, Default)]
pub struct MainReply {
    pub text: String,
    pub reasoning: String,
    pub prompt_tokens: u64,
    pub cache_read: u64,
}

pub fn reply(text: &str, prompt_tokens: u64) -> MainReply {
    MainReply {
        text: text.into(),
        prompt_tokens,
        ..Default::default()
    }
}

/// Which summary requests to reject as too long.
pub type Rejector = Box<dyn Fn(&Value) -> bool + Send>;

#[derive(Default)]
pub struct Script {
    pub main: VecDeque<MainReply>,
    pub summaries: VecDeque<String>,
    /// Summary requests this rejects as too long.
    pub reject: Option<Rejector>,
    /// Summary requests wait for a permit here, when set.
    pub hold: Option<Arc<tokio::sync::Semaphore>>,
    /// Summary requests this fails with a server error.
    pub fail_summaries: usize,
    pub log: Vec<(Kind, Value)>,
}

pub type Shared = Arc<Mutex<Script>>;

pub fn is_summary(body: &Value) -> bool {
    body.to_string()
        .contains("Do not continue the task or call tools.")
}

/// The protocol a request speaks, from its path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Wire {
    Chat,
    Responses,
    Anthropic,
}

/// A streamed reply in `wire`'s format: reasoning, text, prompt tokens.
fn stream(wire: Wire, reasoning: &str, text: &str, prompt: u64, cached: u64) -> String {
    let events: Vec<Value> = match wire {
        Wire::Chat => {
            let mut c = Vec::new();
            if !reasoning.is_empty() {
                c.push(json!({"choices": [{"delta": {"reasoning_content": reasoning}}]}));
            }
            c.push(json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]}));
            if prompt > 0 {
                c.push(
                    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": 1,
                    "prompt_tokens_details": {"cached_tokens": cached}}}),
                );
            }
            c
        }
        Wire::Responses => vec![
            json!({"type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "id": "m", "role": "assistant", "content": []}}),
            json!({"type": "response.output_text.delta", "item_id": "m", "output_index": 0, "delta": text}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": prompt, "output_tokens": 1,
                "input_tokens_details": {"cached_tokens": cached}}}}),
        ],
        Wire::Anthropic => vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": prompt}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ],
    };
    let mut body = String::new();
    for e in &events {
        if let Some(t) = e["type"].as_str().filter(|_| wire == Wire::Anthropic) {
            body.push_str(&format!("event: {t}\n"));
        }
        body.push_str(&format!("data: {e}\n\n"));
    }
    if wire == Wire::Chat {
        body.push_str("data: [DONE]\n\n");
    }
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

async fn read_body(s: &mut tokio::net::TcpStream) -> Option<(String, Value)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    loop {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
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
                let line = text.lines().next().unwrap_or_default().to_string();
                return Some((line, serde_json::from_slice(&buf[i + 4..i + 4 + len]).ok()?));
            }
        }
    }
}

fn error(status: u16, message: &str) -> String {
    let b = json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string();
    format!("HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())
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
                let Some((line, body)) = read_body(&mut s).await else {
                    return;
                };
                let wire = if line.contains("/responses") {
                    Wire::Responses
                } else if line.contains("/messages") {
                    Wire::Anthropic
                } else {
                    Wire::Chat
                };
                let system = body["messages"][0]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let kind = if system == title::SYSTEM {
                    Kind::Title
                } else if is_summary(&body) {
                    Kind::Summary
                } else {
                    Kind::Main
                };
                let hold = {
                    let mut sc = script.lock().unwrap();
                    sc.log.push((kind, body.clone()));
                    if kind == Kind::Summary {
                        sc.hold.clone()
                    } else {
                        None
                    }
                };
                if let Some(h) = hold {
                    let _permit = h.acquire().await.unwrap();
                }
                let resp = {
                    let mut sc = script.lock().unwrap();
                    match kind {
                        Kind::Title => stream(wire, "", "Title", 0, 0),
                        Kind::Summary if sc.reject.as_ref().is_some_and(|r| r(&body)) => {
                            error(400, "This model's maximum context length is 32768 tokens")
                        }
                        Kind::Summary if sc.fail_summaries > 0 => {
                            sc.fail_summaries -= 1;
                            error(400, "summary unavailable")
                        }
                        Kind::Summary => {
                            let text = sc
                                .summaries
                                .pop_front()
                                .unwrap_or_else(|| "## Objective\n- manual summary".into());
                            stream(wire, "", &text, 15, 3)
                        }
                        Kind::Main => {
                            let r = sc.main.pop_front().unwrap_or_else(|| reply("ok", 0));
                            stream(wire, &r.reasoning, &r.text, r.prompt_tokens, r.cache_read)
                        }
                    }
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

pub struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// A session on the scripted model, with the model's window and output
/// limit, and `extra` merged into the config.
pub async fn session(
    context: u64,
    output: u32,
    extra: Value,
) -> (Session, Shared, tempfile::TempDir) {
    session_on("openai-chat", "m", context, output, extra).await
}

/// [`session`] on another protocol and model id.
pub async fn session_on(
    protocol: &str,
    id: &str,
    context: u64,
    output: u32,
    extra: Value,
) -> (Session, Shared, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let mut v = json!({
        "model": format!("fake/{id}"), "snapshots": false,
        "providers": {"fake": {"protocol": protocol,
            "settings": {"baseURL": base, "apiKey": "x"},
            "models": {id: {"limit": {"context": context, "output": output}}}}},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    let l = Loaded {
        config: serde_json::from_value::<Config>(v).unwrap(),
        files: vec![],
        root: dir.path().to_path_buf(),
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        questioner: None,
        mcp: None,
    };
    let mut s = Session::new(
        &l,
        dir.path().to_path_buf(),
        opts,
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap();
    s.set_retry_scale(0.0);
    (s, script, dir)
}

pub fn requests(script: &Shared, kind: Kind) -> Vec<Value> {
    script
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, b)| b.clone())
        .collect()
}

pub fn push(script: &Shared, replies: impl IntoIterator<Item = MainReply>) {
    script.lock().unwrap().main.extend(replies);
}

/// Text of `tokens` estimated tokens (four characters each).
pub fn token_text(tokens: u64) -> String {
    "text".repeat(tokens as usize)
}
