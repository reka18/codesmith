//! Images and PDFs through a session, against a scripted OpenAI Chat model:
//! `read` on an image reaches the next request as vision input, a model
//! configured without image input gets a note instead, attachments ride on
//! the user message, MCP tool images reach the model too, and all of it
//! survives the session store.

use codesmith_ai::{Media, Part};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PIXEL: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// A model that reads `pixel.png` when asked to ("read it") and otherwise
/// answers; it records every request body.
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
                log.lock().unwrap().push(body.clone());
                let msgs = body["messages"].as_array().unwrap();
                let asks = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
                let read = asks["content"] == "read it"
                    && !msgs.iter().any(|m| m["role"] == "tool")
                    && body.get("tools").is_some();
                let mcp = asks["content"] == "mcp it"
                    && !msgs.iter().any(|m| m["role"] == "tool")
                    && body.get("tools").is_some();
                let chunk = if read {
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"path\": \"pixel.png\"}"}}]}, "finish_reason": "tool_calls"}]})
                } else if mcp {
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "fake_echo", "arguments": "{\"text\": \"pixel\"}"}}]}, "finish_reason": "tool_calls"}]})
                } else {
                    json!({"choices": [{"delta": {"content": "seen"}, "finish_reason": "stop"}]})
                };
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn session(dir: &std::path::Path, base: &str, model: Value, resume: Option<String>) -> Session {
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"},
            "models": {"m": model}}},
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
            resume,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&dir.join("s.db")).unwrap()),
    )
    .unwrap()
}

fn write_pixel(dir: &std::path::Path) {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PIXEL)
        .unwrap();
    std::fs::write(dir.join("pixel.png"), bytes).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_images_reach_the_model() {
    let dir = tempfile::tempdir().unwrap();
    write_pixel(dir.path());
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let mut s = session(dir.path(), &base, json!({}), None);
    s.prompt("read it", &mut Quiet).await.unwrap();

    let req = log.lock().unwrap().last().unwrap().clone();
    let msgs = req["messages"].as_array().unwrap();
    assert!(msgs.contains(
        &json!({"role": "tool", "tool_call_id": "c1", "content": "Image read successfully"})
    ));
    assert_eq!(
        msgs.last().unwrap(),
        &json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PIXEL}")}}]})
    );
    // The image is stored with the result, and a resumed session sends it.
    let id = s.id.clone();
    drop(s);
    let mut again = session(dir.path(), &base, json!({}), Some(id));
    assert!(again.messages.iter().any(|m| m
        .parts
        .iter()
        .any(|p| matches!(p, Part::ToolResult { media, .. } if media.len() == 1))));
    again.prompt("thanks", &mut Quiet).await.unwrap();
    let req = log.lock().unwrap().last().unwrap().clone();
    assert!(req.to_string().contains(PIXEL));
}

#[tokio::test(flavor = "multi_thread")]
async fn models_without_vision_are_told_instead() {
    let dir = tempfile::tempdir().unwrap();
    write_pixel(dir.path());
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let mut s = session(
        dir.path(),
        &base,
        json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        None,
    );
    s.prompt("read it", &mut Quiet).await.unwrap();
    let req = log.lock().unwrap().last().unwrap().clone();
    assert!(!req.to_string().contains(PIXEL));
    let tool = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert_eq!(
        tool["content"],
        "Image read successfully\nERROR: Cannot read \"pixel.png\" (this model does not support image input). Inform the user."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_ride_on_the_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let mut s = session(dir.path(), &base, json!({}), None);
    let pdf = Media::base64("application/pdf", "JVBERi0=").named("report.pdf");
    s.prompt_with("summarize", vec![pdf], &mut Quiet)
        .await
        .unwrap();
    let req = log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.get("tools").is_some())
        .unwrap()
        .clone();
    assert_eq!(
        req["messages"][1],
        json!({"role": "user", "content": [
            {"type": "text", "text": "summarize"},
            {"type": "file", "file": {"filename": "report.pdf", "file_data": "data:application/pdf;base64,JVBERi0="}},
        ]})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tool_images_reach_the_model() {
    let Some(py) = ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
        "mcp": {"servers": {"fake": {"type": "local", "command": [py, fixture], "codemode": false}}},
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: dir.path().to_path_buf(),
    };
    let mcp = codesmith_core::mcp::Mcp::connect(&loaded.config.mcp, &loaded.root).await;
    let mut s = Session::new(
        &loaded,
        dir.path().to_path_buf(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: Some(mcp),
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&dir.path().join("s.db")).unwrap()),
    )
    .unwrap();

    /// Records the media each call reports.
    struct Seen(Vec<(String, Vec<Media>)>);
    impl Sink for Seen {
        fn text(&mut self, _: &str) {}
        fn tool_media(&mut self, id: &str, media: &[Media]) {
            self.0.push((id.to_string(), media.to_vec()));
        }
    }
    let mut seen = Seen(vec![]);
    s.prompt("mcp it", &mut seen).await.unwrap();
    assert_eq!(
        seen.0,
        vec![("c1".to_string(), vec![Media::base64("image/png", PIXEL)])]
    );
    let req = log.lock().unwrap().last().unwrap().clone();
    let msgs = req["messages"].as_array().unwrap();
    assert!(msgs.contains(&json!({"role": "tool", "tool_call_id": "c1", "content": "echo: pixel"})));
    assert_eq!(
        msgs.last().unwrap(),
        &json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PIXEL}")}}]})
    );
}
