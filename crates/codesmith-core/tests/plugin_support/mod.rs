//! A scripted OpenAI-compatible model for the plugin API tests: each
//! request (headers and JSON body) is recorded and answered by a closure.
#![allow(dead_code)]

use codesmith_core::config::{Config, Loaded};
use codesmith_core::session::Sink;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How the model answers one request.
pub enum Reply {
    Text(String),
    /// One tool call: id, name, arguments.
    Tool(String, String, Value),
    /// A non-2xx response with this body.
    Status(u16, String),
    /// Answer with the inner reply once the gate opens.
    After(Arc<tokio::sync::Notify>, Box<Reply>),
}

pub fn text(t: &str) -> Reply {
    Reply::Text(t.to_string())
}

pub fn tool(id: &str, name: &str, args: Value) -> Reply {
    Reply::Tool(id.into(), name.into(), args)
}

/// A context overflow, as OpenAI reports one.
pub fn overflow() -> Reply {
    Reply::Status(
        400,
        json!({"error": {"message": "This model's maximum context length is 1000 tokens", "code": "context_length_exceeded"}}).to_string(),
    )
}

#[derive(Debug, Clone)]
pub struct Seen {
    /// Lowercased names.
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

impl Seen {
    /// The text of the last message.
    pub fn last_text(&self) -> String {
        self.body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .map(|m| m["content"].to_string())
            .unwrap_or_default()
    }
}

pub type Respond = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

pub struct Model {
    pub base: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Model {
    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Serve `respond` on a local port; the task ends with the test's runtime.
pub async fn serve(respond: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Model {
    let respond: Respond = Arc::new(respond);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (log, respond) = (log.clone(), respond.clone());
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
                                serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len])
                                    .unwrap_or(Value::Null),
                            );
                        }
                    }
                };
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                let req = Seen { headers, body };
                log.lock().unwrap().push(req.clone());
                let mut reply = respond(&req);
                while let Reply::After(gate, inner) = reply {
                    gate.notified().await;
                    reply = *inner;
                }
                let (status, ctype, payload) = match reply {
                    Reply::Text(t) => {
                        let chunk = json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]});
                        (
                            200,
                            "text/event-stream",
                            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                        )
                    }
                    Reply::Tool(id, name, args) => {
                        let chunk = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]});
                        (
                            200,
                            "text/event-stream",
                            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                        )
                    }
                    Reply::Status(code, body) => (code, "application/json", body),
                    Reply::After(..) => unreachable!(),
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Model {
        base: format!("http://{addr}/v1"),
        seen,
    }
}

/// Config for a session on the scripted model, with `extra` merged in.
pub fn loaded(dir: &std::path::Path, base: &str, extra: Value) -> Loaded {
    let mut v = json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    let mut config: Config = serde_json::from_value(v).unwrap();
    codesmith_core::plugin::active().transform(&mut config);
    Loaded {
        config,
        files: vec![],
        root: dir.to_path_buf(),
    }
}

pub fn options() -> codesmith_core::session::Options {
    codesmith_core::session::Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        questioner: None,
        mcp: None,
    }
}

#[derive(Default)]
pub struct Collect(pub String);
impl Sink for Collect {
    fn text(&mut self, d: &str) {
        self.0.push_str(d);
    }
}
