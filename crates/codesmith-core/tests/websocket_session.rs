//! A session's steps over the OpenAI Responses WebSocket (after OpenCode's
//! `session-model-transport.test.ts`): one connection for the whole turn,
//! and each step after the first sends only its new input items against the
//! previous response. The model is a local tokio-tungstenite server.

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message as M;

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

#[derive(Default)]
struct Server {
    connections: usize,
    messages: Vec<Value>,
    replies: VecDeque<Vec<Value>>,
}

async fn serve(server: Arc<Mutex<Server>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let server = server.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                    return;
                };
                server.lock().unwrap().connections += 1;
                while let Some(Ok(msg)) = ws.next().await {
                    let M::Text(t) = msg else { continue };
                    let reply = {
                        let mut s = server.lock().unwrap();
                        s.messages.push(serde_json::from_str(t.as_str()).unwrap());
                        s.replies.pop_front().unwrap_or_default()
                    };
                    for e in reply {
                        if ws.send(M::text(e.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    format!("http://{addr}/v1")
}

fn config(base: &str, transport: Option<&str>) -> Config {
    let mut settings = json!({"baseURL": base, "apiKey": "x"});
    if let Some(t) = transport {
        settings["transport"] = json!(t);
    }
    serde_json::from_value(json!({
        "model": "fake/gpt-5.2", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-responses", "settings": settings}},
    }))
    .unwrap()
}

fn session(config: Config, root: &std::path::Path) -> Session {
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.to_path_buf(),
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("Test".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    Session::new(
        &loaded,
        root.to_path_buf(),
        opts,
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reuses_its_connection_and_continues_each_step() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    let call = json!({"type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "read", "arguments": "{\"path\": \"a.txt\"}"});
    let server: Arc<Mutex<Server>> = Arc::default();
    server.lock().unwrap().replies = VecDeque::from(vec![
        vec![
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": call}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": call}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "output": [call]}}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "resp_2"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "It says hello."}),
            json!({"type": "response.completed", "response": {"id": "resp_2"}}),
        ],
    ]);
    let base = serve(server.clone()).await;
    let mut s = session(config(&base, Some("websocket")), &root);
    s.prompt("What is in a.txt?", &mut Quiet).await.unwrap();

    let sv = server.lock().unwrap();
    assert_eq!(sv.connections, 1, "both steps share one connection");
    assert_eq!(sv.messages.len(), 2);
    let first = &sv.messages[0];
    assert_eq!(first["type"], "response.create");
    assert!(first.get("previous_response_id").is_none());
    assert!(first.get("stream").is_none());
    let second = &sv.messages[1];
    assert_eq!(second["previous_response_id"], "resp_1");
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "only the tool output is new: {input:?}");
    assert_eq!(input[0]["type"], "function_call_output");
    assert_eq!(input[0]["call_id"], "call_1");
    assert!(input[0]["output"].as_str().unwrap().contains("hello"));
}

/// Read one HTTP request; its request line.
async fn read_request(s: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
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
                return text.lines().next().map(str::to_string);
            }
        }
    }
}

#[tokio::test]
async fn a_session_falls_back_to_http_when_the_upgrade_fails() {
    // A plain HTTP server: the upgrade is refused, and the step goes over
    // HTTP.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Ok((mut s, _)) = l.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let Some(line) = read_request(&mut s).await else {
                    return;
                };
                seen.lock().unwrap().push(line.clone());
                let resp = if line.starts_with("GET") {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let body = [
                        json!({"type": "response.output_text.delta", "item_id": "m", "delta": "ok"}),
                        json!({"type": "response.completed", "response": {}}),
                    ]
                    .iter()
                    .map(|e| format!("data: {e}\n\n"))
                    .collect::<String>();
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut s = session(
        config(&format!("http://{addr}/v1"), Some("websocket")),
        &root,
    );
    s.prompt("hi", &mut Quiet).await.unwrap();
    s.prompt("again", &mut Quiet).await.unwrap();
    let r = requests.lock().unwrap().clone();
    let gets = r.iter().filter(|l| l.starts_with("GET")).count();
    let posts = r.iter().filter(|l| l.starts_with("POST")).count();
    assert_eq!(
        gets, 1,
        "one failed upgrade pins the session to HTTP: {r:?}"
    );
    assert_eq!(posts, 2);
}

#[test]
fn transport_defaults_follow_opencode() {
    let c: Config = serde_json::from_value(json!({
        "providers": {
            "openai": {"settings": {"apiKey": "k"}},
            "plain": {"protocol": "openai-responses", "settings": {"baseURL": "http://x/v1"}},
            "off": {"protocol": "openai-responses", "settings": {"baseURL": "https://api.openai.com/v1", "transport": "http"}},
            "azure": {"protocol": "openai-responses", "settings": {"resourceName": "r", "apiKey": "k"}},
            "chat": {"protocol": "openai-chat", "settings": {"baseURL": "http://x/v1", "transport": "websocket"}},
        }
    }))
    .unwrap();
    let route = |spec: &str| c.resolve_model(spec).unwrap().websocket;
    use codesmith_ai::ws::Route;
    assert_eq!(route("openai/gpt-5.2"), Some(Route::OpenAI));
    assert_eq!(route("plain/m"), None);
    assert_eq!(route("off/m"), None);
    assert_eq!(route("azure/d"), Some(Route::Azure));
    assert_eq!(route("chat/m"), None, "only Responses has a WebSocket");
    let bad: Config = serde_json::from_value(json!({
        "providers": {"p": {"protocol": "openai-responses", "settings": {"baseURL": "http://x/v1", "transport": "tcp"}}}
    }))
    .unwrap();
    assert!(bad.resolve_model("p/m").is_err());
}
