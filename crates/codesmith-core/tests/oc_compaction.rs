//! More session-level ports of OpenCode's compaction tests
//! (`packages/core/test/session-runner.test.ts`, the compaction scenarios,
//! MIT) on the scripted model of `parity.rs`, which can also fail or cut
//! short a summary request. See `crates/codesmith-core/PARITY.md`.

// The harness is the one in `parity.rs`; not every helper is used here.
#![allow(dead_code)]

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::title;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the model does, by kind of request.
#[derive(Default)]
struct Script {
    /// `Err(status)` fails title requests with that status.
    title: Option<Result<String, u16>>,
    /// Summary replies in order; then a filled-in template.
    summaries: VecDeque<String>,
    /// Fail this many main requests with a context-length error first.
    overflow: usize,
    /// Raw HTTP responses that answer summary requests before `summaries`.
    raw_summaries: VecDeque<String>,
    /// Every request body, with its kind.
    log: Vec<(Kind, Value)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Title,
    Summary,
    Main,
}

type Shared = Arc<Mutex<Script>>;

async fn read_body(s: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
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
                return serde_json::from_slice(&buf[i + 4..i + 4 + len]).unwrap();
            }
        }
    }
}

async fn model(script: Shared) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let script = script.clone();
            tokio::spawn(async move {
                let body = read_body(&mut s).await;
                let system = body["messages"][0]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let kind = if system == title::SYSTEM {
                    Kind::Title
                } else if system.starts_with("You are a helpful assistant that writes concise") {
                    Kind::Summary
                } else {
                    Kind::Main
                };
                let raw = {
                    let mut sc = script.lock().unwrap();
                    if kind == Kind::Summary {
                        sc.raw_summaries.pop_front()
                    } else {
                        None
                    }
                };
                if let Some(raw) = raw {
                    script.lock().unwrap().log.push((kind, body));
                    let _ = s.write_all(raw.as_bytes()).await;
                    return;
                }
                let reply: Result<String, (u16, String)> = {
                    let mut sc = script.lock().unwrap();
                    sc.log.push((kind, body));
                    match kind {
                        Kind::Title => match sc.title.clone() {
                            Some(Err(status)) => Err((status, "provider unavailable".into())),
                            Some(Ok(t)) => Ok(t),
                            None => Ok("Generated Title\n".into()),
                        },
                        Kind::Summary => Ok(sc
                            .summaries
                            .pop_front()
                            .unwrap_or_else(|| "## Objective\n- manual summary".into())),
                        Kind::Main if sc.overflow > 0 => {
                            sc.overflow -= 1;
                            Err((
                                400,
                                "This model's maximum context length is 32768 tokens".into(),
                            ))
                        }
                        Kind::Main => Ok("ok".into()),
                    }
                };
                let resp = match reply {
                    Ok(t) => {
                        let chunk = json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]});
                        let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len())
                    }
                    Err((status, msg)) => {
                        let b = json!({"error": {"message": msg}}).to_string();
                        format!("HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len())
                    }
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

fn loaded(dir: &std::path::Path, base: &str, extra: Value) -> Loaded {
    let mut v = json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    Loaded {
        config: serde_json::from_value::<Config>(v).unwrap(),
        files: vec![],
        root: dir.to_path_buf(),
    }
}

async fn session(extra: Value, title: Option<&str>) -> (Session, Shared, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script: Shared = Arc::default();
    let base = model(script.clone()).await;
    let l = loaded(dir.path(), &base, extra);
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: title.map(String::from),
        parent: None,
        questioner: None,
        mcp: None,
    };
    let s = Session::new(
        &l,
        dir.path().to_path_buf(),
        opts,
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap();
    (s, script, dir)
}

fn requests(script: &Shared, kind: Kind) -> Vec<Value> {
    script
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, b)| b.clone())
        .collect()
}

fn last_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .unwrap_or_default()
}

fn text_of(m: &codesmith_ai::Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            codesmith_ai::Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn http(status: u16, content_type: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn finish(reason: &str, content: &str) -> String {
    let chunk = json!({"choices": [{"delta": {"content": content}, "finish_reason": reason}]});
    http(
        200,
        "text/event-stream",
        &format!("data: {chunk}\n\ndata: [DONE]\n\n"),
    )
}

fn rejected(message: &str) -> String {
    http(
        400,
        "application/json",
        &json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string(),
    )
}

/// "preserves provider errors from manual compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_preserves_provider_errors_from_manual_compaction() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    let before = s.messages.clone();
    script
        .lock()
        .unwrap()
        .raw_summaries
        .push_back(rejected("summary unavailable"));
    let e = s.compact(&mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("summary unavailable"), "{e:#}");
    assert_eq!(s.messages, before, "a failed compaction changes nothing");
}

/// "explains a compaction blocked by the provider" (adapted: without a
/// refusal category). Fixed: the blocked reply was nudged like a reply
/// without the template.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_explains_a_compaction_blocked_by_the_provider() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    let before = s.messages.clone();
    script
        .lock()
        .unwrap()
        .raw_summaries
        .push_back(finish("content_filter", ""));
    let e = s.compact(&mut Quiet).await.unwrap_err();
    assert_eq!(
        format!("{e:#}"),
        "Compaction summary was blocked by the provider"
    );
    assert_eq!(requests(&script, Kind::Summary).len(), 1, "no nudge");
    assert_eq!(s.messages, before);
}

/// "rejects compaction ${response} without retrying or committing its
/// draft" (length and content-filter). Fixed: the summary's finish reason
/// was ignored, so a truncated summary replaced the history.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_rejects_a_cut_short_summary_without_retrying_or_committing_it() {
    for reason in ["length", "content_filter"] {
        let (mut s, script, _d) = session(json!({}), Some("t")).await;
        s.prompt("Earlier question", &mut Quiet).await.unwrap();
        script
            .lock()
            .unwrap()
            .raw_summaries
            .push_back(finish(reason, "## Objective\n- Incomplete summary"));
        assert!(s.compact(&mut Quiet).await.is_err(), "{reason}");
        assert_eq!(requests(&script, Kind::Summary).len(), 1, "{reason}");
        s.prompt("Continue", &mut Quiet).await.unwrap();
        let last = requests(&script, Kind::Main).last().unwrap().to_string();
        assert!(last.contains("Earlier question"), "{reason}");
        assert!(!last.contains("Incomplete summary"), "{reason}");
    }
}

/// "publishes the original overflow when recovery summarization fails".
/// Fixed: the summary's error was reported instead.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_reports_the_original_overflow_when_recovery_fails() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    {
        let mut sc = script.lock().unwrap();
        sc.overflow = 1;
        sc.raw_summaries.push_back(rejected("summary unavailable"));
    }
    let e = s.prompt("Continue", &mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("maximum context length"), "{e:#}");
    assert_eq!(
        requests(&script, Kind::Main).len(),
        2,
        "one, then the overflow"
    );
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
}

/// "persists a second context overflow after one recovery"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_second_overflow_after_one_recovery_fails_the_turn() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    script.lock().unwrap().overflow = 2;
    let e = s.prompt("Continue", &mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("maximum context length"), "{e:#}");
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
    assert_eq!(requests(&script, Kind::Main).len(), 3);
    assert!(text_of(&s.messages[0]).starts_with("<conversation-checkpoint>"));
}
