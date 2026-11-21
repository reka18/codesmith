//! Shared by the session feature ports: an isolated environment, a fake
//! OpenAI-compatible model and a stored session to drive against it.
#![allow(dead_code)]

use codesmith_ai::{Message, Part, Role};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Keep the user's own config, data and skills out of the tests.
pub fn isolate() -> PathBuf {
    // A new folder with a random name, created exclusively (0700): a
    // name made from the pid in the shared temporary directory could be
    // created or linked first by another local user.
    static BASE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let base = tempfile::Builder::new()
            .prefix("codesmith-session-feat-")
            .tempdir()
            .unwrap()
            .keep();
        for d in ["home", "config", "data"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::env::set_var("HOME", base.join("home"));
        std::env::set_var("CODESMITH_CONFIG_DIR", base.join("config"));
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        base
    })
    .clone()
}

/// The response chunk for a request body (`None`: the default behaviour).
pub type Reply_ = Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

/// A fake model: a prompt `call NAME ARGS` calls tool NAME with ARGS, a
/// tool result is answered with "done", anything else with "ok"; `reply`
/// can answer first. Every request body is logged.
pub async fn model(log: Arc<Mutex<Vec<Value>>>, reply: Option<Reply_>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let log = log.clone();
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let body = loop {
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
                            break serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len])
                                .unwrap();
                        }
                    }
                };
                log.lock().unwrap().push(body.clone());
                let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                let content = last["content"].as_str().unwrap_or("").to_string();
                let chunk = if let Some(c) = reply.as_ref().and_then(|r| r(&body)) {
                    c
                } else if body["messages"]
                    .to_string()
                    .contains("Do not continue the task or call tools.")
                {
                    // A summary request: a summary in the template's form.
                    json!({"choices": [{"delta": {"content": "## Objective\n- summary"}, "finish_reason": "stop"}]})
                } else if body.get("tools").is_none() {
                    json!({"choices": [{"delta": {"content": "Title"}, "finish_reason": "stop"}]})
                } else if let Some(rest) = content
                    .strip_prefix("call ")
                    .filter(|_| last["role"] == "user")
                {
                    let (name, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": name, "arguments": args}}]}, "finish_reason": "tool_calls"}]})
                } else if last["role"] == "tool" {
                    json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]})
                } else {
                    json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
                };
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes())
                    .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

pub struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// Collects what a turn shows.
#[derive(Default)]
pub struct Collect {
    pub text: String,
    pub notices: Vec<String>,
}
impl Sink for Collect {
    fn text(&mut self, t: &str) {
        self.text.push_str(t);
    }
    fn notice(&mut self, n: &str) {
        self.notices.push(n.to_string());
    }
}

/// A stored session in a temporary project, against the fake model.
pub struct H {
    pub s: Session,
    pub mcp: Option<Arc<codesmith_core::mcp::Mcp>>,
    pub log: Arc<Mutex<Vec<Value>>>,
    pub loaded: Loaded,
    pub db: PathBuf,
    pub project: tempfile::TempDir,
    pub outside: tempfile::TempDir,
}

impl H {
    pub async fn new() -> H {
        H::with(json!({}), None).await
    }

    /// With `extra` config merged in and `reply` answering the model first.
    pub async fn with(extra: Value, reply: Option<Reply_>) -> H {
        isolate();
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let base = model(log.clone(), reply).await;
        let mut v = json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"},
                                   "models": {"m": {}}}},
        });
        for (k, val) in extra.as_object().unwrap() {
            match k.as_str() {
                // The fake model's own config.
                "fakeModel" => v["providers"]["fake"]["models"]["m"] = val.clone(),
                _ => v[k] = val.clone(),
            }
        }
        let config: Config = serde_json::from_value(v).unwrap();
        let loaded = Loaded {
            config,
            files: vec![],
            root: root.clone(),
        };
        let db = outside.path().join("s.db");
        let mcp = match extra.get("mcp") {
            Some(_) => {
                Some(codesmith_core::mcp::Mcp::connect(&loaded.config.mcp, &loaded.root).await)
            }
            None => None,
        };
        let s = Session::new(
            &loaded,
            root,
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("Parent".into()),
                parent: None,
                id: None,
                questioner: None,
                mcp: mcp.clone(),
            },
            Arc::new(Fixed(Reply::Always)),
            Some(Store::open(&db).unwrap()),
        )
        .unwrap();
        H {
            s,
            mcp,
            log,
            loaded,
            db,
            project,
            outside,
        }
    }

    /// Start over with a new session (one that sees files written since).
    pub fn renew(&mut self) {
        self.s = Session::new(
            &self.loaded,
            self.loaded.root.clone(),
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("Parent".into()),
                parent: None,
                id: None,
                questioner: None,
                mcp: self.mcp.clone(),
            },
            Arc::new(Fixed(Reply::Always)),
            Some(self.store()),
        )
        .unwrap();
    }

    pub fn root(&self) -> PathBuf {
        self.loaded.root.clone()
    }

    pub async fn prompt(&mut self, text: &str) {
        self.s.prompt(text, &mut Quiet).await.unwrap();
    }

    pub fn store(&self) -> Store {
        Store::open(&self.db).unwrap()
    }

    /// Open stored session `id`.
    pub fn open(&self, id: &str) -> Session {
        Session::new(
            &self.loaded,
            self.loaded.root.clone(),
            Options {
                agent: None,
                model: None,
                resume: Some(id.into()),
                title: None,
                parent: None,
                id: None,
                questioner: None,
                mcp: self.mcp.clone(),
            },
            Arc::new(Fixed(Reply::Always)),
            Some(self.store()),
        )
        .unwrap()
    }

    /// Model requests (title requests left out).
    pub fn requests(&self) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.get("tools").is_some())
            .cloned()
            .collect()
    }
}

/// The text of a message's text parts.
pub fn text(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A history as (role, text) pairs, system updates left out.
pub fn convo(messages: &[Message]) -> Vec<(&'static str, String)> {
    messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|m| {
            (
                match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                },
                text(m),
            )
        })
        .collect()
}
