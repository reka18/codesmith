//! Variant switches in a running session become stored effort updates (as
//! OpenCode's session lowers a `model-switched` message), against a scripted
//! Anthropic model that takes them natively: the request's top-level
//! effort, and with it the cached prefix, stays the one the conversation
//! started with.

use codesmith_ai::{Part, Role};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// An Anthropic model that answers "ok" (and fails a prompt of "fail")
/// and records every request body.
async fn model(log: Arc<Mutex<Vec<Value>>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let log = log.clone();
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
                let last = &body["messages"].as_array().unwrap().last().unwrap()["content"];
                let fail = last.as_array().unwrap().last().unwrap()["text"] == "fail";
                log.lock().unwrap().push(body);
                if fail {
                    let e = r#"{"type":"error","error":{"type":"invalid_request_error","message":"no"}}"#;
                    let _ = s.write_all(format!("HTTP/1.1 400 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{e}", e.len()).as_bytes()).await;
                    return;
                }
                let sse: String = [
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
                    json!({"type": "message_stop"}),
                ]
                .iter()
                .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
                .collect();
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct T {
    dir: tempfile::TempDir,
    base: String,
    log: Arc<Mutex<Vec<Value>>>,
}

impl T {
    async fn new() -> T {
        let log = Arc::default();
        let base = model(Arc::clone(&log)).await;
        T {
            dir: tempfile::tempdir().unwrap(),
            base,
            log,
        }
    }

    /// The session with `model`, continuing `resume`.
    fn session(&self, model: &str, resume: Option<&str>) -> Session {
        let effort = |e: &str| json!({"effort": e});
        let config: Config = serde_json::from_value(json!({
            "model": "fake/claude-opus-5", "snapshots": false,
            "providers": {"fake": {"protocol": "anthropic-messages", "settings": {"baseURL": self.base, "apiKey": "x"},
                "models": {
                    "claude-opus-5": {"variants": {"low": effort("low"), "high": effort("high"), "deep": effort("max")}},
                    "claude-opus-5.1": {"variants": {"low": effort("low"), "high": effort("high")}},
                }}},
        }))
        .unwrap();
        let loaded = Loaded {
            config,
            files: vec![],
            root: self.dir.path().to_path_buf(),
        };
        Session::new(
            &loaded,
            self.dir.path().to_path_buf(),
            Options {
                agent: None,
                model: Some(model.into()),
                resume: resume.map(String::from),
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            Arc::new(Fixed(Reply::Once)),
            Some(Store::open(&self.dir.path().join("s.db")).unwrap()),
        )
        .unwrap()
    }

    /// A turn with `model` in session `id` (a new one when `None`); its id.
    async fn turn(&self, model: &str, id: Option<&str>, text: &str) -> String {
        let mut s = self.session(model, id);
        s.prompt(text, &mut Quiet).await.unwrap();
        s.id.clone()
    }

    fn last_request(&self) -> Value {
        self.log.lock().unwrap().last().cloned().unwrap()
    }

    /// The effort updates stored in session `id`: (effort, previous).
    fn updates(&self, id: &str) -> Vec<(Option<String>, Option<String>)> {
        let store = Store::open(&self.dir.path().join("s.db")).unwrap();
        store
            .history(id)
            .unwrap()
            .iter()
            .filter_map(|m| match (&m.role, m.parts.as_slice()) {
                (Role::System, [Part::Effort { effort, previous }]) => {
                    Some((effort.clone(), previous.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// The request's native effort updates, in order.
fn native_updates(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system" && m.get("output_config").is_some())
        .map(|m| m["output_config"]["effort"].clone())
        .collect()
}

/// The request's messages without the date, a native system update after
/// the first user message.
fn without_date(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| !m["content"].to_string().contains("Today's date: "))
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_variant_switch_is_an_effort_update_and_keeps_the_top_level_effort() {
    let t = T::new().await;
    let id = t.turn("fake/claude-opus-5#high", None, "one").await;
    let first = t.last_request();
    assert_eq!(first["output_config"]["effort"], "high");
    assert!(native_updates(&first).is_empty());

    // `/variant low`: the session is rebuilt with the new spec.
    t.turn("fake/claude-opus-5#low", Some(&id), "two").await;
    assert_eq!(t.updates(&id), [(some("low"), some("high"))]);
    let second = t.last_request();
    assert_eq!(
        second["output_config"]["effort"], "high",
        "the top-level effort stays, so the cached prefix does: {second:#}"
    );
    assert_eq!(native_updates(&second), [json!("low")]);
    // Everything before the update is sent as before.
    let messages = without_date(&second);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "system");

    // The same variant again writes nothing more.
    t.turn("fake/claude-opus-5#low", Some(&id), "three").await;
    assert_eq!(t.updates(&id).len(), 1);

    // Back to the default effort.
    t.turn("fake/claude-opus-5", Some(&id), "four").await;
    assert_eq!(
        t.updates(&id),
        [(some("low"), some("high")), (None, some("low"))]
    );
    let last = t.last_request();
    assert_eq!(last["output_config"]["effort"], "high");
    assert_eq!(native_updates(&last).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_switches_make_no_effort_update() {
    let t = T::new().await;
    let id = t.turn("fake/claude-opus-5#high", None, "one").await;
    // A variant that is not an effort name.
    t.turn("fake/claude-opus-5#deep", Some(&id), "two").await;
    assert!(t.updates(&id).is_empty());
    assert_eq!(t.last_request()["output_config"]["effort"], "max");
    // Another model: its own effort, from the top.
    t.turn("fake/claude-opus-5.1#low", Some(&id), "three").await;
    assert!(t.updates(&id).is_empty());
    let r = t.last_request();
    assert_eq!(r["output_config"]["effort"], "low");
    assert!(native_updates(&r).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_back_and_forth_between_turns_writes_nothing() {
    let t = T::new().await;
    let id = t.turn("fake/claude-opus-5#high", None, "one").await;
    // Switched away and back before the next turn: nothing changed.
    let s = t.session("fake/claude-opus-5#low", Some(&id));
    assert!(s.effort_switch().is_some());
    drop(s);
    t.turn("fake/claude-opus-5#high", Some(&id), "two").await;
    assert!(t.updates(&id).is_empty());
    // A new session has nothing to update.
    let s = t.session("fake/claude-opus-5#low", None);
    assert!(s.effort_switch().is_none());
}

// The update is stored before the turn's model call, so a turn that made
// no step still counts: reopening the session does not write it again.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_is_written_once_even_when_its_turn_fails() {
    let t = T::new().await;
    let id = t.turn("fake/claude-opus-5#high", None, "one").await;
    let mut s = t.session("fake/claude-opus-5#low", Some(&id));
    assert!(s.prompt("fail", &mut Quiet).await.is_err());
    drop(s);
    assert_eq!(t.updates(&id), [(some("low"), some("high"))]);
    let s = t.session("fake/claude-opus-5#low", Some(&id));
    assert!(s.effort_switch().is_none());
    drop(s);
    t.turn("fake/claude-opus-5#low", Some(&id), "two").await;
    assert_eq!(t.updates(&id), [(some("low"), some("high"))]);
    let r = t.last_request();
    assert_eq!(r["output_config"]["effort"], "high");
    assert_eq!(native_updates(&r), [json!("low")]);
}
