//! Ports of OpenCode's session HTTP hook tests
//! (`packages/core/test/session-model-request-hooks.test.ts`, MIT): plugin
//! hooks on the raw HTTP requests and responses of every session request
//! kind, and on the Responses WebSocket handshake and frames. Its own test
//! binary, because the plugin registry is process-wide. See
//! `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_ai::ws::{
    Connection, Connector, Frame, Handshake, Outbound, Route, SessionTransport, WsError,
};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::{self, reqwest, Plugin, RequestKind, RequestScope};
use codesmith_core::session::Session;
use futures_util::future::BoxFuture;
use plugin_support::{loaded, options, serve, text, Collect};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    seen: Mutex<Vec<String>>,
    replace: AtomicBool,
}

struct Hooks(Arc<State>);

impl Plugin for Hooks {
    fn id(&self) -> &str {
        "hooks"
    }
    fn http_request(&self, scope: &RequestScope, req: &mut reqwest::Request) {
        self.0
            .seen
            .lock()
            .unwrap()
            .push(format!("request:{}:{}", scope.kind.name(), scope.agent));
        if self.0.replace.load(Ordering::SeqCst) {
            *req.body_mut() = Some(r#"{"replaced":true}"#.into());
            req.headers_mut().remove(reqwest::header::CONTENT_LENGTH);
            return;
        }
        req.headers_mut().insert(
            "x-plugin",
            reqwest::header::HeaderValue::from_static("kept"),
        );
    }
    fn http_response(&self, scope: &RequestScope, res: &mut reqwest::Response) {
        assert!(res.status().is_success());
        self.0
            .seen
            .lock()
            .unwrap()
            .push(format!("response:{}:{}", scope.kind.name(), scope.agent));
    }
    fn ws_handshake(
        &self,
        scope: &RequestScope,
        url: &mut String,
        headers: &mut Vec<(String, String)>,
    ) {
        self.0
            .seen
            .lock()
            .unwrap()
            .push(format!("handshake:{}:{url}", scope.kind.name()));
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
        headers.push(("authorization".into(), "Bearer minted".into()));
    }
    fn ws_send(&self, scope: &RequestScope, frame: &mut String) {
        let v: Value = serde_json::from_str(frame).unwrap();
        self.0.seen.lock().unwrap().push(format!(
            "send:{}:{}",
            scope.kind.name(),
            v["type"].as_str().unwrap()
        ));
        let mut v = v;
        v["plugin"] = json!(true);
        *frame = v.to_string();
    }
    fn ws_receive(&self, scope: &RequestScope, frame: &mut String) {
        let v: Value = serde_json::from_str(frame).unwrap();
        self.0.seen.lock().unwrap().push(format!(
            "receive:{}:{}",
            scope.kind.name(),
            v["type"].as_str().unwrap()
        ));
        *frame = frame.replace("\"hi\"", "\"HI\"");
    }
}

/// The plugin, installed once for this binary, and a lock the tests take
/// in turn, since they share it.
fn state() -> (Arc<State>, tokio::sync::MutexGuard<'static, ()>) {
    static STATE: std::sync::OnceLock<Arc<State>> = std::sync::OnceLock::new();
    let s = STATE
        .get_or_init(|| {
            let s = Arc::new(State::default());
            plugin::install(vec![Arc::new(Hooks(s.clone()))]);
            s
        })
        .clone();
    let guard = plugin_support::serial();
    s.seen.lock().unwrap().clear();
    s.replace.store(false, Ordering::SeqCst);
    (s, guard)
}

fn session(l: &codesmith_core::config::Loaded, dir: &std::path::Path, title: bool) -> Session {
    let mut o = options();
    if !title {
        o.title = None;
    }
    Session::new(l, dir.to_path_buf(), o, Arc::new(Fixed(Reply::Once)), None).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_tags_every_session_request_kind_on_http_request_and_http_response() {
    let (state, _lock) = state();
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|r| {
        if r.last_text().contains("summar") {
            text("## Goal\n- summary")
        } else {
            text("Done")
        }
    })
    .await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), false);
    let mut out = Collect::default();
    s.prompt("first", &mut out).await.unwrap();
    s.prompt("second", &mut out).await.unwrap();
    s.compact(&mut out).await.unwrap();
    s.generate("Say something").await.unwrap();
    let seen = state.seen.lock().unwrap().clone();
    for kind in [
        RequestKind::Primary,
        RequestKind::Title,
        RequestKind::Compaction,
        RequestKind::Generate,
    ] {
        let k = kind.name();
        let mine: Vec<&String> = seen
            .iter()
            .filter(|e| e.split(':').nth(1) == Some(k))
            .collect();
        assert!(!mine.is_empty(), "{k}: {seen:?}");
        for pair in mine.chunks(2) {
            assert_eq!(
                pair,
                [
                    &format!("request:{k}:build"),
                    &format!("response:{k}:build")
                ],
                "{seen:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_sends_a_kept_requests_original_body_and_a_replaced_requests_new_body() {
    let (state, _lock) = state();
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), true);
    s.prompt("Start", &mut Collect::default()).await.unwrap();
    let kept = m.requests()[0].clone();
    assert_eq!(
        kept.headers.get("x-plugin").map(String::as_str),
        Some("kept")
    );
    assert_eq!(
        kept.body["messages"].as_array().unwrap().last().unwrap()["content"],
        "Start"
    );

    state.replace.store(true, Ordering::SeqCst);
    let _ = s.prompt("Again", &mut Collect::default()).await;
    let replaced = m.requests()[1].clone();
    assert_eq!(replaced.body, json!({"replaced": true}));
    assert!(!replaced.headers.contains_key("x-plugin"));
}

// ---- a scripted WebSocket ----

#[derive(Clone, Default)]
struct Socket {
    opened: Arc<Mutex<Vec<Handshake>>>,
    sent: Arc<Mutex<Vec<String>>>,
}

struct Out {
    socket: Socket,
    tx: tokio::sync::mpsc::Sender<Result<Frame, WsError>>,
}

impl Outbound for Out {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), WsError>> {
        self.socket.sent.lock().unwrap().push(text);
        for f in [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message", "id": "m1"}}),
            json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "hi"}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ] {
            let _ = self.tx.try_send(Ok(Frame::Text(f.to_string())));
        }
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

impl Connector for Socket {
    fn open(&self, handshake: Handshake) -> BoxFuture<'static, Result<Connection, WsError>> {
        self.opened.lock().unwrap().push(handshake);
        let (tx, inbound) = tokio::sync::mpsc::channel(16);
        let out = Out {
            socket: self.clone(),
            tx,
        };
        Box::pin(async move {
            Ok(Connection {
                outbound: Box::new(out),
                inbound,
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_offers_the_websocket_executor_alongside_http_hooks_and_routes_the_websocket_hooks() {
    let (state, _lock) = state();
    let config = loaded(std::path::Path::new("/"), "http://unused/v1", json!({})).config;
    let scope = RequestScope {
        session_id: "ses_ws".into(),
        agent: "build".into(),
        model: "openai/gpt-5.5".into(),
        kind: RequestKind::Primary,
    };
    let hooks = plugin::active().http_hooks(&config, scope);
    assert!(hooks.is_some(), "an enabled plugin gets the hooks");
    let provider = codesmith_ai::Provider {
        protocol: codesmith_ai::Protocol::OpenaiResponses,
        base_url: "https://example.test/v1".into(),
        api_key: Some("k".into()),
        hooks,
        ..Default::default()
    };
    let socket = Socket::default();
    let transport = SessionTransport::new(Arc::new(socket.clone()));
    let req = codesmith_ai::Request {
        model: "gpt-5.5".into(),
        messages: vec![codesmith_ai::Message::user("hello")],
        ..Default::default()
    };
    let stream = transport
        .stream(&provider, &reqwest::Client::new(), &req, Route::OpenAI)
        .await
        .unwrap();
    let r = codesmith_ai::assemble(stream, |_| {}).await.unwrap();
    assert_eq!(
        r.text(),
        "HI",
        "the receive hook changed the frame before decoding"
    );

    let opened = socket.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].url, "wss://example.test/v1/responses");
    let auth: Vec<&(String, String)> = opened[0]
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .collect();
    assert_eq!(
        auth,
        [&("authorization".to_string(), "Bearer minted".to_string())]
    );
    let sent: Value = serde_json::from_str(&socket.sent.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["type"], "response.create");
    assert_eq!(sent["plugin"], true, "the send hook's frame was sent");
    let seen = state.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            "handshake:primary:wss://example.test/v1/responses",
            "send:primary:response.create",
            "receive:primary:response.created",
            "receive:primary:response.output_item.added",
            "receive:primary:response.output_text.delta",
            "receive:primary:response.completed",
        ]
    );
}
