//! The stateless generate API, ported from OpenCode's `generate.test.ts`
//! (one-shot text calls and their retry rules) and
//! `session-generate.test.ts` (generating from a session's settled context
//! without changing it), MIT.

mod media_harness;

use codesmith_ai::Error;
use codesmith_core::config::{Config, Loaded};
use codesmith_core::generate::{self, Failure, GenerateError};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::{Record, Store};
use media_harness::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FAST: generate::Options = generate::Options {
    retry_scale: 0.001,
    poll: Duration::from_millis(5),
};

/// Runs `call` under the one-shot rules, failing with `errors` in turn and
/// then succeeding; returns the outcome and how many calls were made.
async fn one_shot(errors: Vec<Error>, always: bool) -> (Result<&'static str, Error>, usize) {
    let calls = AtomicUsize::new(0);
    let errors = Mutex::new(errors);
    let r = generate::retry_one_shot(FAST, || {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        let next = {
            let mut e = errors.lock().unwrap();
            if always {
                e.first().map(copy)
            } else if n < e.len() {
                Some(e.remove(0))
            } else {
                None
            }
        };
        async move {
            match next {
                Some(e) => Err(e),
                None => Ok("Fresh answer"),
            }
        }
    })
    .await;
    (r, calls.load(Ordering::SeqCst))
}

fn copy(e: &Error) -> Error {
    match e {
        Error::Status { status, body } => Error::Status {
            status: *status,
            body: body.clone(),
        },
        Error::Transport(m) => Error::Transport(m.clone()),
        other => Error::Decode(other.to_string()),
    }
}

fn status(s: u16, body: &str) -> Error {
    Error::Status {
        status: s,
        body: body.into(),
    }
}

#[tokio::test]
async fn oc_generate_retries_a_stateless_call_the_provider_never_received() {
    let refused = Error::Transport(
        "error sending request: client error (Connect): tcp connect error: Connection refused (os error 61)".into(),
    );
    let (r, calls) = one_shot(vec![refused], false).await;
    assert_eq!(r.unwrap(), "Fresh answer");
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn oc_generate_does_not_retry_a_stateless_failure_the_provider_may_have_run() {
    // The provider may already have run (and billed) these.
    for e in [
        Error::Transport(
            "error decoding response body: connection closed before message completed".into(),
        ),
        Error::Transport("error sending request: operation timed out".into()),
        status(500, r#"{"error": {"message": "Internal error"}}"#),
    ] {
        let (r, calls) = one_shot(vec![copy(&e)], true).await;
        assert!(r.is_err());
        assert_eq!(calls, 1, "{e}");
    }
}

#[tokio::test]
async fn oc_generate_does_not_retry_stateless_authentication_quota_or_invalid_request_failures() {
    let cases = [
        (
            status(
                401,
                r#"{"error": {"message": "Invalid credentials", "type": "authentication_error"}}"#,
            ),
            Some(Failure::Authentication),
            "Invalid credentials",
        ),
        (
            status(
                429,
                r#"{"error": {"message": "No quota", "code": "insufficient_quota"}}"#,
            ),
            Some(Failure::RateLimit),
            "No quota",
        ),
        (
            status(400, r#"{"error": {"message": "Invalid prompt"}}"#),
            None,
            "Invalid prompt",
        ),
    ];
    for (e, reason, message) in cases {
        let (r, calls) = one_shot(vec![copy(&e)], true).await;
        assert_eq!(calls, 1, "{e}");
        let err = generate::provider_error("test", &r.unwrap_err());
        match (reason, err) {
            (
                Some(want),
                GenerateError::ProviderRequest {
                    reason, message: m, ..
                },
            ) => {
                assert_eq!(reason, want);
                assert_eq!(m, message);
            }
            (None, GenerateError::InvalidRequest { message: m }) => assert_eq!(m, message),
            (want, got) => panic!("{want:?} {got:?}"),
        }
    }
}

#[tokio::test]
async fn oc_generate_waits_out_a_throttle_and_cancels_without_another_generation() {
    // Adapted: provider errors carry no headers, so the schedule's own
    // backoff applies instead of `retry-after`.
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let run = generate::retry_one_shot(generate::Options::default(), move || {
        c.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>(status(429, r#"{"error": {"message": "Wait"}}"#)) }
    });
    // Dropping the call while it waits cancels it.
    assert!(tokio::time::timeout(Duration::from_millis(200), run)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oc_generate_retries_an_overloaded_provider_at_most_three_times() {
    let (r, calls) = one_shot(
        vec![status(529, r#"{"error": {"message": "Overloaded"}}"#)],
        true,
    )
    .await;
    assert_eq!(calls, 4);
    assert!(matches!(
        generate::provider_error("test", &r.unwrap_err()),
        GenerateError::ProviderRequest {
            reason: Failure::Overloaded,
            ..
        }
    ));
}

/// A Responses model answering "OK", recording each body.
async fn responses_ok() -> Server {
    serve(Arc::new(|_: &Seen| {
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "OK"}),
            json!({"type": "response.completed", "response": {"id": "resp_1"}}),
        ];
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        (200, "text/event-stream", body.into_bytes())
    }))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_attributes_each_stateless_completion_without_creating_a_stored_session() {
    isolate();
    let server = responses_ok().await;
    let config: Config = serde_json::from_value(json!({
        "providers": {"fake": {"protocol": "openai-responses",
            "settings": {"baseURL": format!("{}/v1", server.url), "apiKey": "x"}, "models": {"m": {}}}},
    }))
    .unwrap();
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let (text, _) = generate::text(&config, &client, Some("fake/m"), "Answer privately", FAST)
            .await
            .unwrap();
        assert_eq!(text, "OK");
    }
    let keys: Vec<String> = server
        .requests()
        .iter()
        .map(|r| r.json()["prompt_cache_key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.starts_with("ses_")));
    assert_ne!(keys[0], keys[1], "each call is its own");
    let store = Store::open(&isolate().join("data/codesmith/sessions.db"));
    if let Ok(s) = store {
        assert!(s.list(None, true).unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_generate_text_rejects_an_image_model_and_an_unknown_one() {
    let server = responses_ok().await;
    let config = config(
        vec![provider(
            "test",
            "",
            vec![model(
                "painter",
                "image",
                "@opencode/ai/providers/openai/images",
                &server.url,
                &["text"],
                1,
            )],
        )],
        &["test"],
        json!({}),
    );
    let client = reqwest::Client::new();
    match generate::text(&config, &client, Some("test/painter"), "x", FAST).await {
        Err(GenerateError::UnsupportedModel { message, .. }) => {
            assert_eq!(message, "test/painter is a image model, not a chat model")
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        generate::text(&config, &client, Some("nope/m"), "x", FAST).await,
        Err(GenerateError::ModelNotFound { .. })
    ));
    assert!(matches!(
        generate::text(&config, &client, None, "x", FAST).await,
        Err(GenerateError::Unavailable { .. })
    ));
}

// session-generate.test.ts

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

/// A Chat Completions model: "Summarize privately" gets "Transient
/// answer" (the first `cut` of those requests stop mid-answer), anything
/// else "Settled partial answer". Records each body.
async fn chat(log: Arc<Mutex<Vec<Value>>>, cut: usize) -> Server {
    let cuts = Arc::new(AtomicUsize::new(cut));
    serve(Arc::new(move |s: &Seen| {
        let body = s.json();
        log.lock().unwrap().push(body.clone());
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        let summarize = last["content"] == "Summarize privately";
        let mut sse = String::new();
        if summarize && cuts.load(Ordering::SeqCst) > 0 {
            cuts.fetch_sub(1, Ordering::SeqCst);
            // A partial answer, then the stream ends without finishing.
            let chunk = json!({"choices": [{"delta": {"content": "Discard this"}}]});
            return (
                200,
                "text/event-stream",
                format!("data: {chunk}\n\n").into_bytes(),
            );
        }
        let text = if summarize {
            "Transient answer"
        } else {
            "Settled partial answer"
        };
        let chunk = json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]});
        sse.push_str(&format!("data: {chunk}\n\n"));
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 60, "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 30}}});
        sse.push_str(&format!("data: {usage}\n\ndata: [DONE]\n\n"));
        (200, "text/event-stream", sse.into_bytes())
    }))
    .await
}

struct H {
    s: Session,
    log: Arc<Mutex<Vec<Value>>>,
    db: std::path::PathBuf,
    context: std::path::PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn harness(cut: usize, context: &str) -> H {
    isolate();
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let file = outside.path().canonicalize().unwrap().join("context.md");
    std::fs::write(&file, context).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let server = chat(log.clone(), cut).await;
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false,
        // A hosted provider, as in OpenCode, though it listens on loopback.
        "providers": {"fake": {"protocol": "openai-chat", "local": false,
            "settings": {"baseURL": format!("{}/v1", server.url), "apiKey": "x"}, "models": {"m": {}}}},
        "instructions": [file.display().to_string()],
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
    };
    let db = outside.path().join("s.db");
    let mut s = Session::new(
        &loaded,
        root,
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();
    s.set_retry_scale(0.001);
    // The test server is kept alive by its listener task.
    std::mem::forget(server);
    H {
        s,
        log,
        db,
        context: file,
        _dirs: (project, outside),
    }
}

fn records(db: &std::path::Path, id: &str) -> Vec<Record> {
    Store::open(db)
        .unwrap()
        .entries(id)
        .unwrap()
        .into_iter()
        .map(|e| e.record)
        .collect()
}

fn texts(body: &Value, role: &str) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == role)
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_generates_from_fresh_settled_context_and_records_only_its_usage() {
    let mut h = harness(0, "Initial context").await;
    h.s.prompt("Existing durable context", &mut Quiet)
        .await
        .unwrap();
    std::fs::write(&h.context, "Changed context").unwrap();
    let messages = h.s.messages.clone();
    let before = records(&h.db, &h.s.id);
    let usage_before = h.s.usage;

    let text = h.s.generate("Summarize privately").await.unwrap();
    assert_eq!(text, "Transient answer");
    let req = h.log.lock().unwrap().last().unwrap().clone();
    let system = texts(&req, "system");
    assert!(system[0].contains("Initial context"), "{system:?}");
    // The change since the last step reaches the model as an update.
    let users = texts(&req, "user");
    let n = users.len();
    assert!(n >= 2, "{users:?}");
    assert_eq!(users[0], "Existing durable context");
    assert_eq!(users[n - 1], "Summarize privately");
    // Chat has no mid-conversation system role: the update may go as user text.
    assert!(
        system
            .iter()
            .skip(1)
            .chain(&users[1..n - 1])
            .any(|t| t.contains("Changed context")),
        "{system:?} {users:?}"
    );
    assert_eq!(texts(&req, "assistant"), ["Settled partial answer"]);
    assert!(req["tools"].as_array().is_some_and(|t| !t.is_empty()));

    // History is untouched; only the call's usage was recorded.
    assert_eq!(h.s.messages, messages);
    let after = records(&h.db, &h.s.id);
    assert_eq!(after.len(), before.len() + 1);
    assert!(matches!(after.last().unwrap(), Record::Step { usage: u, .. } if u.output == 10));
    assert_eq!(h.s.usage.output, usage_before.output + 10);
    let stored = Store::open(&h.db).unwrap().get(&h.s.id).unwrap().unwrap();
    assert_eq!(stored.usage.input, h.s.usage.input);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_session_regenerates_a_failed_partial_answer_without_mutating_history() {
    let mut h = harness(1, "Initial context").await;
    h.s.prompt("Existing durable context", &mut Quiet)
        .await
        .unwrap();
    let messages = h.s.messages.clone();
    let before = records(&h.db, &h.s.id);
    let text = h.s.generate("Summarize privately").await.unwrap();
    assert_eq!(
        text,
        "Fresh answer".replace("Fresh answer", "Transient answer")
    );
    let log = h.log.lock().unwrap().clone();
    let (a, b) = (&log[log.len() - 2], &log[log.len() - 1]);
    assert_eq!(a, b, "the retry sends the same request");
    assert_eq!(h.s.messages, messages);
    // The retried attempt counts once.
    assert_eq!(records(&h.db, &h.s.id).len(), before.len() + 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_generate_blocks_unavailable_initial_instructions_before_generation() {
    use std::os::unix::fs::PermissionsExt;
    let mut h = harness(0, "Initial context").await;
    std::fs::set_permissions(&h.context, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&h.context).is_ok() {
        return; // running as root
    }
    let e = h.s.generate("Summarize privately").await.unwrap_err();
    assert!(
        matches!(
            e.downcast_ref::<codesmith_core::instruction_state::Error>(),
            Some(codesmith_core::instruction_state::Error::Blocked(_))
        ),
        "{e:#}"
    );
    assert!(h.log.lock().unwrap().is_empty());
    assert!(Store::open(&h.db).unwrap().get(&h.s.id).unwrap().is_none());
    std::fs::set_permissions(&h.context, std::fs::Permissions::from_mode(0o644)).unwrap();
}
