//! Session-level ports of OpenCode's compaction and title tests
//! (`packages/core/test/session-compaction.test.ts`, `session-title.test.ts`),
//! against a scripted OpenAI-compatible model on a raw TCP socket. The pure
//! rules are tested in `compaction.rs` and `title.rs`; the deliberate
//! differences are listed in those modules' headers.

use codesmith_core::compaction;
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
                } else if body["messages"]
                    .to_string()
                    .contains("Do not continue the task or call tools.")
                {
                    Kind::Summary
                } else {
                    Kind::Main
                };
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

// --- Titles ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_title_generates_a_title_from_the_sole_user_message() {
    let (mut s, script, _d) = session(json!({}), None).await;
    s.prompt("Help me debug the failing build", &mut Quiet)
        .await
        .unwrap();
    let t = requests(&script, Kind::Title);
    assert_eq!(t.len(), 1);
    assert!(
        t[0].get("tools").is_none(),
        "a title request offers no tools"
    );
    assert!(last_user_text(&t[0]).contains("Help me debug the failing build"));
    assert_eq!(s.title.as_deref(), Some("Generated Title"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_title_generates_from_the_first_user_message_only_once() {
    let (mut s, script, _d) = session(json!({}), None).await;
    s.prompt("First message", &mut Quiet).await.unwrap();
    s.prompt("Second message", &mut Quiet).await.unwrap();
    let t = requests(&script, Kind::Title);
    assert_eq!(t.len(), 1);
    assert!(last_user_text(&t[0]).contains("First message"));
    assert!(!last_user_text(&t[0]).contains("Second message"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_title_uses_a_given_title_without_a_model_request() {
    let (mut s, script, _d) = session(json!({}), Some("Plugin Title")).await;
    s.prompt("Hello", &mut Quiet).await.unwrap();
    assert!(requests(&script, Kind::Title).is_empty());
    assert_eq!(s.title.as_deref(), Some("Plugin Title"));
}

/// Deliberate difference: OpenCode leaves the session untitled.
#[tokio::test(flavor = "multi_thread")]
async fn oc_title_a_failed_request_falls_back_to_the_first_line() {
    let (mut s, script, _d) = session(json!({}), None).await;
    script.lock().unwrap().title = Some(Err(400));
    s.prompt("Fail this title\nsecond line", &mut Quiet)
        .await
        .unwrap();
    assert_eq!(requests(&script, Kind::Title).len(), 1);
    assert_eq!(s.title.as_deref(), Some("Fail this title"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_title_clips_a_long_generated_title() {
    let (mut s, script, _d) = session(json!({}), None).await;
    script.lock().unwrap().title = Some(Ok(format!("{}😀{}", "a".repeat(96), "b".repeat(20))));
    s.prompt("Name this session", &mut Quiet).await.unwrap();
    assert_eq!(s.title, Some(format!("{}...", "a".repeat(96))));
}

// --- Compaction ---

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_manual_compaction_summarizes_short_context() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    s.prompt("Read it", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let r = requests(&script, Kind::Summary);
    assert_eq!(r.len(), 1);
    // The session's own request (its tools and the whole conversation, the
    // kept tail included), so the summary reads the prompt cache.
    let main = requests(&script, Kind::Main);
    assert_eq!(r[0]["tools"], main.last().unwrap()["tools"]);
    let msgs = r[0]["messages"].as_array().unwrap();
    let normal = main.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(
        msgs[..normal.len()],
        normal[..],
        "the session's request is the summary request's prefix"
    );
    assert!(msgs[1]["content"].to_string().contains("Earlier question"));
    assert!(msgs
        .iter()
        .any(|m| m["content"].to_string().contains("Read it")));
    assert_eq!(
        msgs.last().unwrap()["content"],
        compaction::prompt(false),
        "the summary instruction comes last"
    );
    assert_eq!(s.messages.len(), 3, "checkpoint, then the latest exchange");
    let checkpoint = text_of(&s.messages[0]);
    assert!(checkpoint.starts_with("<conversation-checkpoint>"));
    assert!(checkpoint.contains("## Objective\n- manual summary"));
    assert_eq!(text_of(&s.messages[1]), "Read it");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_updates_an_existing_checkpoint() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    s.prompt("two", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    s.prompt("three", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let r = requests(&script, Kind::Summary);
    assert_eq!(r.len(), 2);
    let msgs = r[1]["messages"].as_array().unwrap();
    assert!(msgs[1]["content"]
        .to_string()
        .contains("<conversation-checkpoint>"));
    assert_eq!(msgs.last().unwrap()["content"], compaction::prompt(true));
    assert_eq!(text_of(&s.messages[1]), "three");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_nudges_a_reply_without_the_template() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    script.lock().unwrap().summaries = VecDeque::from([
        "Sure! Here's what we did.".to_string(),
        "## Objective\n- fixed".into(),
    ]);
    s.prompt("one", &mut Quiet).await.unwrap();
    s.prompt("two", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let r = requests(&script, Kind::Summary);
    assert_eq!(r.len(), 2);
    assert_eq!(
        r[1]["messages"].as_array().unwrap().last().unwrap()["content"],
        compaction::NUDGE
    );
    assert!(text_of(&s.messages[0]).contains("## Objective\n- fixed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_nothing_to_compact_in_an_empty_session() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    assert!(s.compact(&mut Quiet).await.is_err());
    assert!(requests(&script, Kind::Summary).is_empty());
}

/// Fills the window past its ceiling with a second, large message.
async fn overfill(s: &mut Session) {
    s.prompt("hi", &mut Quiet).await.unwrap();
    s.model.context = Some(s.estimate_prompt() + 2_000);
    let _ = s.prompt(&"x".repeat(8_000), &mut Quiet).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_auto_compacts_past_the_buffered_ceiling() {
    let (mut s, script, _d) = session(json!({"compaction": {"buffer": 1000}}), Some("t")).await;
    overfill(&mut s).await;
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
    let main = requests(&script, Kind::Main);
    let last = main.last().unwrap()["messages"].as_array().unwrap().clone();
    assert!(
        last[1]["content"]
            .to_string()
            .contains("<conversation-checkpoint>"),
        "the request after compaction starts from the checkpoint"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_stays_off_when_auto_is_disabled() {
    let (mut s, script, _d) = session(
        json!({"compaction": {"auto": false, "buffer": 1000}}),
        Some("t"),
    )
    .await;
    overfill(&mut s).await;
    assert!(requests(&script, Kind::Summary).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_an_overflow_compacts_and_retries() {
    let (mut s, script, _d) = session(json!({}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    script.lock().unwrap().overflow = 1;
    s.prompt("two", &mut Quiet).await.unwrap();
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
    assert!(text_of(&s.messages[0]).starts_with("<conversation-checkpoint>"));
}

/// Not from OpenCode: when the current turn itself was summarized, the
/// engine's note to carry on is a system update, which Chat receives as
/// `<system-update>` text on the checkpoint's user message.
#[tokio::test(flavor = "multi_thread")]
async fn compaction_of_the_current_turn_continues_with_a_system_update() {
    let (mut s, script, _d) =
        session(json!({"compaction": {"keep": {"tokens": 1}}}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    script.lock().unwrap().overflow = 1;
    s.prompt("a second prompt too long to keep", &mut Quiet)
        .await
        .unwrap();
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
    assert!(text_of(&s.messages[0]).starts_with("<conversation-checkpoint>"));
    assert_eq!(
        s.messages[1],
        codesmith_ai::Message::system(compaction::CONTINUE)
    );
    let main = requests(&script, Kind::Main);
    let last = main.last().unwrap()["messages"].as_array().unwrap().clone();
    let checkpoint = last[1]["content"].as_str().unwrap();
    assert!(checkpoint.starts_with("<conversation-checkpoint>"));
    assert!(checkpoint.ends_with(&format!(
        "\n<system-update>\n{}\n</system-update>",
        compaction::CONTINUE
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_overflow_is_left_to_the_user_when_auto_is_disabled() {
    let (mut s, script, _d) = session(json!({"compaction": {"auto": false}}), Some("t")).await;
    s.prompt("one", &mut Quiet).await.unwrap();
    script.lock().unwrap().overflow = 1;
    let e = s.prompt("two", &mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("maximum context length"), "{e:#}");
    assert!(requests(&script, Kind::Summary).is_empty());
}
