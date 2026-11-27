//! Hosted (provider-executed) tools in a session, ported from OpenCode's
//! `session-runner.test.ts` (MIT): a hosted call and its result are kept
//! with the session and replayed to the provider that ran them, and a call
//! whose result never came is settled as failed, durably, however the step
//! ended. The provider is a scripted Anthropic Messages endpoint.

mod media_harness;

use codesmith_ai::{Part, Role};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use media_harness::*;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect::<String>()
        .into_bytes()
}

fn start() -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant",
        "content": [], "model": "m", "usage": {"input_tokens": 1, "output_tokens": 1}}})
}

/// A web_search call (block 0) the provider ran.
fn hosted_call(id: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": id, "name": "web_search", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"Effect\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
    ]
}

fn hosted_result(id: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": id,
            "content": [{"type": "web_search_result", "title": "Effect", "url": "https://effect.website", "encrypted_content": "enc"}]}}),
        json!({"type": "content_block_stop", "index": 1}),
    ]
}

fn stop(reason: &str) -> Vec<Value> {
    vec![
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]
}

fn text_reply() -> Reply2 {
    let mut e = vec![start()];
    e.push(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
    e.push(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "done"}}));
    e.push(json!({"type": "content_block_stop", "index": 0}));
    e.extend(stop("end_turn"));
    (200, sse(&e))
}

type Reply2 = (u16, Vec<u8>);

struct H {
    s: Session,
    log: Arc<Mutex<Vec<Value>>>,
    loaded: Loaded,
    db: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

/// A session over an Anthropic model that answers each request with the
/// next of `replies` (then plain text).
async fn harness(replies: Vec<Reply2>) -> H {
    isolate();
    let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    let server = serve(Arc::new(move |s: &Seen| {
        l2.lock().unwrap().push(s.json());
        let (status, body) = queue.lock().unwrap().pop_front().unwrap_or_else(text_reply);
        if status == 200 {
            (200, "text/event-stream", body)
        } else {
            (status, "application/json", body)
        }
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let config: Config = serde_json::from_value(json!({
        "model": "fake/claude-sonnet-4-5", "snapshots": false,
        "providers": {"fake": {"protocol": "anthropic-messages",
            "settings": {"baseURL": format!("{}/v1", server.url), "apiKey": "x"},
            "models": {"claude-sonnet-4-5": {"options": {"hostedTools": ["web_search"]}}}}},
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
        ..Default::default()
    };
    let db = root.join("s.db");
    let s = open(&loaded, &db, None);
    H {
        s,
        log,
        loaded,
        db,
        _dir: dir,
    }
}

fn open(loaded: &Loaded, db: &std::path::Path, resume: Option<String>) -> Session {
    let mut s = Session::new(
        loaded,
        loaded.root.clone(),
        Options {
            agent: None,
            model: None,
            resume,
            title: Some("t".into()),
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(db).unwrap()),
    )
    .unwrap();
    s.set_retry_scale(0.0001);
    s
}

/// The hosted parts of the stored history, after a reload.
fn stored_hosted(h: &H) -> Vec<codesmith_ai::Hosted> {
    Store::open(&h.db)
        .unwrap()
        .history(&h.s.id)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts)
        .filter_map(|p| match p {
            Part::Hosted(h) => Some(h),
            _ => None,
        })
        .collect()
}

fn failed(h: &codesmith_ai::Hosted) -> bool {
    h.result.as_ref().is_some_and(|r| r.is_error)
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_hosted_replays_durable_provider_executed_tool_results_inline_in_the_next_request() {
    let mut e = vec![start()];
    e.extend(hosted_call("srvtoolu_1"));
    e.extend(hosted_result("srvtoolu_1"));
    e.extend(stop("end_turn"));
    let mut h = harness(vec![(200, sse(&e))]).await;
    h.s.prompt("Search first", &mut Quiet).await.unwrap();
    // A reload reads the call and its result back from the store.
    let id = h.s.id.clone();
    h.s = open(&h.loaded, &h.db, Some(id));
    h.s.prompt("Continue", &mut Quiet).await.unwrap();
    let req = h.log.lock().unwrap().last().unwrap().clone();
    let msgs = req["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    let blocks = msgs[1]["content"].as_array().unwrap();
    assert_eq!(
        blocks[0],
        json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "Effect"}})
    );
    assert_eq!(blocks[1]["type"], "web_search_tool_result");
    assert_eq!(blocks[1]["tool_use_id"], "srvtoolu_1");
    assert_eq!(blocks[1]["content"][0]["title"], "Effect");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_hosted_fails_an_unresolved_hosted_tool_before_one_clean_step_end() {
    let mut e = vec![start()];
    e.extend(hosted_call("srvtoolu_clean"));
    e.extend(stop("end_turn"));
    let mut h = harness(vec![(200, sse(&e))]).await;
    h.s.prompt("Settle hosted tool before ending", &mut Quiet)
        .await
        .unwrap();
    let hosted = stored_hosted(&h);
    assert_eq!(hosted.len(), 1);
    assert!(failed(&hosted[0]), "{hosted:?}");
    // Replayed in Anthropic's own failure shape, so the next request is valid.
    h.s.prompt("Next", &mut Quiet).await.unwrap();
    let req = h.log.lock().unwrap().last().unwrap().clone();
    let blocks = req["messages"][1]["content"].as_array().unwrap().clone();
    assert_eq!(
        blocks[1],
        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_clean",
            "content": {"type": "web_search_tool_result_error", "error_code": "unavailable"}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_hosted_keeps_a_paused_turns_call_open_for_the_provider_to_resume() {
    let mut e = vec![start()];
    e.extend(hosted_call("srvtoolu_paused"));
    e.extend(stop("pause_turn"));
    let mut h = harness(vec![(200, sse(&e))]).await;
    h.s.prompt("Search", &mut Quiet).await.unwrap();
    let hosted = stored_hosted(&h);
    assert!(hosted.iter().all(|x| !failed(x)), "{hosted:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_hosted_durably_fails_a_hosted_tool_left_unresolved_by_a_provider_failure() {
    // A non-retryable failure after the call: the step fails, and the call
    // is kept failed with the step's partial output.
    let mut e = vec![start()];
    e.extend(hosted_call("srvtoolu_raw"));
    e.push(json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Invalid request"}}));
    let mut h = harness(vec![(200, sse(&e))]).await;
    let err =
        h.s.prompt("Fail hosted tool on raw failure", &mut Quiet)
            .await
            .unwrap_err();
    assert!(format!("{err:#}").contains("Invalid request"), "{err:#}");
    assert_eq!(h.log.lock().unwrap().len(), 1, "not retried");
    let hosted = stored_hosted(&h);
    assert_eq!(hosted.len(), 1, "{hosted:?}");
    assert_eq!(hosted[0].id, "srvtoolu_raw");
    assert!(failed(&hosted[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_hosted_durably_fails_a_hosted_tool_left_unresolved_at_provider_eof() {
    // Adapted: a stream that ends without finishing was cut off, so it is
    // retried; once the retries run out the call is kept failed.
    let mut e = vec![start()];
    e.extend(hosted_call("srvtoolu_eof"));
    let replies = (0..12).map(|_| (200, sse(&e))).collect();
    let mut h = harness(replies).await;
    assert!(h
        .s
        .prompt("Fail hosted tool at EOF", &mut Quiet)
        .await
        .is_err());
    let hosted = stored_hosted(&h);
    assert!(hosted.iter().all(failed), "{hosted:?}");
    assert!(!hosted.is_empty());
}
