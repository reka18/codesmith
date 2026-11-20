//! The text an `edit` or `write` call changed is read only once the call is
//! allowed, and only from a regular file: a denied write to a FIFO never
//! opens it, and an allowed write records what the file held before.

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Asker, Reply, Request};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::{Record, Store};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// A model that writes `x` to `path` once, then answers.
async fn model(path: String) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let path = path.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                let body = loop {
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
                            break serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len])
                                .unwrap();
                        }
                    }
                };
                let msgs = body["messages"].as_array().unwrap();
                let first =
                    !msgs.iter().any(|m| m["role"] == "tool") && body.get("tools").is_some();
                let args = json!({"path": path, "content": "x"}).to_string();
                let chunk = if first {
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "write", "arguments": args}}]}, "finish_reason": "tool_calls"}]})
                } else {
                    json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]})
                };
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                            sse.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Answer(Reply, Arc<Mutex<Vec<String>>>);
impl Asker for Answer {
    fn ask(&self, r: &Request) -> Reply {
        self.1
            .lock()
            .unwrap()
            .push(format!("{} {:?}", r.action, r.resources));
        self.0.clone()
    }
}

fn session(dir: &std::path::Path, base: &str, permissions: Value, asker: Answer) -> Session {
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false, "permissions": permissions,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"},
            "models": {"m": {}}}},
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: dir.to_path_buf(),
    };
    Session::new(
        &loaded,
        dir.to_path_buf(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(asker),
        Some(Store::open(&dir.join("s.db")).unwrap()),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_write_never_opens_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fifo = root.join("pipe");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    // Opening a FIFO for writing blocks until someone opens it for reading.
    let opened = Arc::new(Mutex::new(false));
    let (o2, f2) = (opened.clone(), fifo.clone());
    std::thread::spawn(move || {
        let _w = std::fs::OpenOptions::new().write(true).open(&f2).unwrap();
        *o2.lock().unwrap() = true;
    });
    let base = model(fifo.display().to_string()).await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let deny = json!([{"action": "edit", "resource": "*", "effect": "deny"}]);
    let mut s = session(&root, &base, deny, Answer(Reply::Once, asked.clone()));
    let r = tokio::time::timeout(Duration::from_secs(20), s.prompt("go", &mut Quiet)).await;
    assert!(r.is_ok(), "the turn hung on the FIFO");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!*opened.lock().unwrap(), "the FIFO was opened for reading");
    assert!(asked.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allowed_write_records_the_text_before() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = root.join("f.txt");
    std::fs::write(&file, "before").unwrap();
    let base = model(file.display().to_string()).await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(&root, &base, json!([]), Answer(Reply::Once, asked.clone()));
    s.prompt("go", &mut Quiet).await.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
    let store = Store::open(&root.join("s.db")).unwrap();
    let changes: Vec<_> = store
        .entries(&s.id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Change { path, old, new, .. } => Some((path, old, new)),
            _ => None,
        })
        .collect();
    assert_eq!(changes, vec![(file, Some("before".into()), "x".into())]);
}
