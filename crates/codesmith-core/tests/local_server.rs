//! A local, single-request server (oMLX, LM Studio, llama.cpp, Ollama):
//! the session title never competes with the turn, is capped and asks for
//! no thinking; `small_model` and the title switches choose how titles are
//! made. A background compaction summary runs only while the session is
//! idle, and is stopped when a turn starts. And the request prefix (system
//! prompt, tools, everything before the first user message) stays
//! byte-identical across steps, turns, sessions of one project and days,
//! so the server's prefix cache keeps it: the date comes after it.

mod support;

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use codesmith_core::title;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::Quiet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request a fake server answered: its body and when it arrived and
/// was answered.
#[derive(Clone, Debug)]
struct Seen {
    body: Value,
    start: Instant,
    end: Instant,
}

impl Seen {
    fn is_title(&self) -> bool {
        self.body["messages"][0]["content"].as_str() == Some(title::SYSTEM)
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A fake OpenAI-compatible server. A main request is answered after
/// `delay` (a title at once): the first one with a call to a tool that does
/// not exist (so the turn takes a second step), the next with "ok". A title
/// request gets "Local title".
async fn server(log: Log, delay: Duration) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let log = log.clone();
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
                let start = Instant::now();
                let is_title = body["messages"][0]["content"].as_str() == Some(title::SYSTEM);
                let chunk = if is_title {
                    json!({"choices": [{"delta": {"content": "Local title"}, "finish_reason": "stop"}]})
                } else {
                    tokio::time::sleep(delay).await;
                    let after_tool = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m["role"] == "tool");
                    if after_tool {
                        json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
                    } else {
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "no_such_tool", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]})
                    }
                };
                log.lock().unwrap().push(Seen {
                    body,
                    start,
                    end: Instant::now(),
                });
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes())
                    .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// A session of model `main/<model>` on `base`, with `extra` config merged
/// over the defaults.
fn session(dir: &std::path::Path, base: &str, model: &str, extra: Value) -> Session {
    support::isolate();
    let mut v = json!({
        "model": format!("main/{model}"), "snapshots": false,
        "providers": {"main": {"protocol": "openai-chat", "settings": {"baseURL": base},
                               "models": {model: {}}}},
    });
    codesmith_core::config::merge(&mut v, extra);
    let l = Loaded {
        config: serde_json::from_value::<Config>(v).unwrap(),
        files: vec![],
        root: dir.to_path_buf(),
    };
    Session::new(
        &l,
        dir.to_path_buf(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: None,
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap()
}

fn split(log: &Log) -> (Vec<Seen>, Vec<Seen>) {
    log.lock()
        .unwrap()
        .iter()
        .cloned()
        .partition(Seen::is_title)
}

const DELAY: Duration = Duration::from_millis(300);

#[tokio::test(flavor = "multi_thread")]
async fn a_local_title_waits_for_the_first_response_and_never_overlaps_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), DELAY).await;
    let mut s = session(dir.path(), &base, "qwen3.5-122b-a10b", json!({}));
    s.prompt("Fix the flaky login test", &mut Quiet)
        .await
        .unwrap();
    let (titles, main) = split(&log);
    assert_eq!(titles.len(), 1, "one title request");
    assert_eq!(main.len(), 2, "the turn took two steps");
    let t = &titles[0];
    assert!(
        t.start >= main[0].end,
        "the title starts after the first response"
    );
    assert!(
        t.end <= main[1].start,
        "the title is done before the next step"
    );
    assert_eq!(s.title.as_deref(), Some("Local title"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_title_is_capped_and_asks_for_no_thinking() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), Duration::ZERO).await;
    // The model thinks by default, and its config asks for thinking too.
    let extra = json!({"providers": {"main": {"models": {"qwen3.5-122b-a10b": {
        "body": {"chat_template_kwargs": {"enable_thinking": true, "keep": 1}},
        "options": {"reasoningEffort": "high"},
    }}}}});
    let mut s = session(dir.path(), &base, "qwen3.5-122b-a10b", extra);
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (titles, main) = split(&log);
    let t = &titles[0].body;
    assert_eq!(t["max_tokens"], 64);
    assert_eq!(t["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(t["chat_template_kwargs"]["keep"], 1, "other kwargs stay");
    assert!(t.get("reasoning_effort").is_none(), "{t}");
    assert!(t.get("tools").is_none());
    assert!(t.get("prompt_cache_key").is_none());
    // The turn itself keeps its settings.
    let m = &main[0].body;
    assert_eq!(m["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(m["reasoning_effort"], "high");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_none_variant_says_how_to_turn_thinking_off() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), Duration::ZERO).await;
    let extra = json!({"providers": {"main": {"models": {"thinker": {
        "variants": {"none": {"body": {"think": false}}},
    }}}}});
    let mut s = session(dir.path(), &base, "thinker", extra);
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (titles, _) = split(&log);
    let t = &titles[0].body;
    assert_eq!(t["think"], false);
    assert!(
        t.get("chat_template_kwargs").is_none(),
        "an unknown model gets no template switch: {t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_title_still_runs_beside_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), DELAY).await;
    // Marked not local: titles run in parallel, as in OpenCode.
    let extra = json!({"providers": {"main": {"local": false}}});
    let mut s = session(dir.path(), &base, "qwen3.5-122b-a10b", extra);
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (titles, main) = split(&log);
    assert!(
        titles[0].start < main[0].end,
        "the title ran while the first step did"
    );
    let t = &titles[0].body;
    assert_eq!(t["max_tokens"], title::MAX_TOKENS);
    assert!(
        t.get("chat_template_kwargs").is_none(),
        "hosted requests are left alone: {t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn small_model_writes_the_title_on_its_own_server() {
    let dir = tempfile::tempdir().unwrap();
    let main_log = Log::default();
    let small_log = Log::default();
    let base = server(main_log.clone(), DELAY).await;
    let small = server(small_log.clone(), Duration::ZERO).await;
    let extra = json!({
        "small_model": "small/tiny",
        "providers": {"small": {"protocol": "openai-chat", "settings": {"baseURL": small},
                                "models": {"tiny": {}}}},
    });
    let mut s = session(dir.path(), &base, "big", extra);
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (titles, main) = split(&main_log);
    assert!(titles.is_empty(), "the turn's server writes no title");
    let small = small_log.lock().unwrap().clone();
    assert_eq!(small.len(), 1);
    assert_eq!(small[0].body["model"], "tiny");
    // Another server: nothing to wait for.
    assert!(small[0].start < main[0].end);
    assert_eq!(s.title.as_deref(), Some("Local title"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_title_agent_model_wins_over_small_model() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), Duration::ZERO).await;
    let extra = json!({
        "small_model": "main/other",
        "agents": {"title": {"model": "main/namer"}},
        "providers": {"main": {"models": {"other": {}, "namer": {}}}},
    });
    let mut s = session(dir.path(), &base, "big", extra);
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (titles, _) = split(&log);
    assert_eq!(titles[0].body["model"], "namer");
    // The `title` key configures titles; it is not an agent to pick.
    let l =
        serde_json::from_value::<Config>(json!({"agents": {"title": {"model": "a/b"}}})).unwrap();
    assert!(!codesmith_core::agent::list(&l).contains(&"title".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_can_be_turned_off() {
    for extra in [
        json!({"title": false}),
        json!({"agents": {"title": {"disable": true}}}),
        json!({"agents": {"title": {"disabled": true}}}),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::default();
        let base = server(log.clone(), Duration::ZERO).await;
        let mut s = session(dir.path(), &base, "big", extra.clone());
        s.prompt("Rename the config loader\nand more", &mut Quiet)
            .await
            .unwrap();
        let (titles, _) = split(&log);
        assert!(titles.is_empty(), "{extra}: no title request");
        assert_eq!(
            s.title.as_deref(),
            Some("Rename the config loader"),
            "{extra}"
        );
    }
}

// --- Background summaries ---

/// A request a summary server saw: when it arrived and (once `done`)
/// ended, and whether the client hung up before the answer.
#[derive(Clone, Debug)]
struct Call {
    summary: bool,
    start: Instant,
    end: Instant,
    done: bool,
    cancelled: bool,
}

type Calls = Arc<Mutex<Vec<Call>>>;

fn is_summary(body: &Value) -> bool {
    body["messages"].as_array().unwrap().iter().any(|m| {
        m["content"]
            .as_str()
            .is_some_and(|c| c.contains("Summarize only what the user and the assistant said"))
    })
}

/// A fake server whose turns take two steps of `DELAY` each, reporting a
/// small prompt on the first request and one near the compaction ceiling
/// (of a 100k window) after, so a background summary starts. A summary is
/// answered after `summary_delay`, unless the client hangs up first.
async fn summary_server(calls: Calls, summary_delay: Duration) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (calls, served) = (calls.clone(), served.clone());
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
                let start = Instant::now();
                let summary = is_summary(&body);
                let delay = if summary { summary_delay } else { DELAY };
                // Seen as it arrives; its end is filled in after.
                let at = {
                    let mut c = calls.lock().unwrap();
                    c.push(Call {
                        summary,
                        start,
                        end: start,
                        done: false,
                        cancelled: false,
                    });
                    c.len() - 1
                };
                // The client hanging up ends the wait.
                let cancelled = tokio::select! {
                    _ = tokio::time::sleep(delay) => false,
                    n = s.read(&mut tmp) => n.unwrap_or(0) == 0,
                };
                {
                    let mut c = calls.lock().unwrap();
                    c[at].end = Instant::now();
                    c[at].done = true;
                    c[at].cancelled = cancelled;
                }
                if cancelled {
                    return;
                }
                let content = if summary {
                    json!({"choices": [{"delta": {"content": "## Objective\n- Fix it"}, "finish_reason": "stop"}]})
                } else if body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
                    json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
                } else {
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("c{}", served.load(std::sync::atomic::Ordering::SeqCst)), "type": "function", "function": {"name": "no_such_tool", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]})
                };
                let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let prompt = if n == 0 { 1_000 } else { 60_000 };
                let usage = json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": 10}});
                let sse = format!("data: {content}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes())
                    .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn summary_session(dir: &std::path::Path, base: &str, local: bool) -> Session {
    let extra = json!({
        "title": false,
        "providers": {"main": {"local": local, "models": {"qwen": {"limit": {"context": 100_000}}}}},
    });
    session(dir, base, "qwen", extra)
}

fn calls(c: &Calls) -> (Vec<Call>, Vec<Call>) {
    c.lock().unwrap().iter().cloned().partition(|c| c.summary)
}

/// Wait (at most five seconds) until the server saw `n` summary requests
/// arrive.
async fn summaries(c: &Calls, n: usize) -> Vec<Call> {
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while calls(c).0.len() < n {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    calls(c).0
}

/// Wait (at most five seconds) until every request the server saw ended.
async fn settled(c: &Calls) -> (Vec<Call>, Vec<Call>) {
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while c.lock().unwrap().iter().any(|c| !c.done) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    calls(c)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_background_summary_runs_only_while_the_session_is_idle() {
    let dir = tempfile::tempdir().unwrap();
    let log = Calls::default();
    let base = summary_server(log.clone(), Duration::from_millis(1_500)).await;
    let mut s = summary_session(dir.path(), &base, true);
    s.prompt("Fix the flaky login test", &mut Quiet)
        .await
        .unwrap();
    // The prompt neared the ceiling on the turn's second step: the summary
    // starts once the turn is over, not beside it.
    let first = summaries(&log, 1).await;
    let turn1 = calls(&log).1;
    assert_eq!(turn1.len(), 2);
    assert_eq!(first.len(), 1, "one background summary");
    assert!(first[0].start >= turn1[1].end, "it waited for the turn");
    assert!(!first[0].done, "still being written");
    // The next turn starts while it is being written: it is stopped before
    // the step goes out.
    s.prompt("Now the logout test", &mut Quiet).await.unwrap();
    let main = calls(&log).1;
    assert_eq!(main.len(), 4);
    let first = settled(&log).await.0[0].clone();
    assert!(first.cancelled, "the summary was stopped for the turn");
    assert!(
        first.end <= main[2].start + Duration::from_millis(100),
        "it was stopped as the step started"
    );
    // Idle again: a new one starts after the turn.
    let all = summaries(&log, 2).await;
    assert_eq!(all.len(), 2, "the summary was started again");
    assert!(all[1].start >= main[3].end);
    let (all, main) = settled(&log).await;
    assert!(!all[1].cancelled, "written while idle");
    for m in &main {
        for c in all.iter().filter(|c| !c.cancelled) {
            assert!(
                c.end <= m.start || c.start >= m.end,
                "no overlap: {c:?} {m:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_background_summary_still_runs_beside_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let log = Calls::default();
    let base = summary_server(log.clone(), Duration::from_millis(1_500)).await;
    let mut s = summary_session(dir.path(), &base, false);
    s.prompt("Fix the flaky login test", &mut Quiet)
        .await
        .unwrap();
    s.prompt("Now the logout test", &mut Quiet).await.unwrap();
    let (first, main) = settled(&log).await;
    assert_eq!(main.len(), 4);
    assert!(!first[0].cancelled, "nothing stopped it");
    assert!(
        first[0].start < main[2].end && first[0].end > main[2].start,
        "it ran while the next turn did"
    );
}

// --- A stable prefix ---

/// Everything of a request a prefix cache sees before the first user
/// message: the body without `messages`, then the messages before it.
fn prefix(body: &Value) -> (Value, Vec<Value>) {
    let mut rest = body.clone();
    let messages = rest.as_object_mut().unwrap().remove("messages").unwrap();
    let before = messages
        .as_array()
        .unwrap()
        .iter()
        .take_while(|m| m["role"] != "user")
        .cloned()
        .collect();
    (rest, before)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prefix_is_byte_stable_across_steps_turns_and_sessions() {
    use support::H;
    // Instructions, a skill, a reference, a websearch tool and, where
    // Python runs the fixture, two MCP servers (one through Code Mode, one
    // as direct tools): each adds to the prefix.
    let mut extra = json!({
        "websearch": {"url": "http://127.0.0.1:9"},
        "references": {"docs": {"path": "/tmp", "description": "Shared docs"}},
    });
    let py = ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if let Some(py) = py {
        let fixture = format!("{}/tests/fixtures/mcp_rich.py", env!("CARGO_MANIFEST_DIR"));
        extra["mcp"] = json!({"servers": {
            "zeta": {"type": "local", "command": [py, fixture]},
            "alpha": {"type": "local", "command": [py, fixture], "codemode": false},
        }});
    }
    let mut h = H::with(extra, None).await;
    // The day the sessions are told, changed below.
    let day = Arc::new(Mutex::new("Thu Nov 19 2026".to_string()));
    let today = |day: &Arc<Mutex<String>>| {
        let day = day.clone();
        move || day.lock().unwrap().clone()
    };
    let root = h.root();
    std::fs::write(root.join("AGENTS.md"), "Run the tests before committing.").unwrap();
    std::fs::create_dir_all(root.join(".codesmith/skills/deploy")).unwrap();
    std::fs::write(
        root.join(".codesmith/skills/deploy/SKILL.md"),
        "---\ndescription: Deploy the app\n---\nSteps.",
    )
    .unwrap();
    h.renew();
    h.s.set_today(today(&day));
    // Two steps (a tool call, then an answer), then a second turn.
    h.prompt("call read {\"filePath\": \"AGENTS.md\"}").await;
    h.prompt("thanks").await;
    // Past midnight, a third turn in the same session.
    *day.lock().unwrap() = "Fri Nov 20 2026".into();
    h.prompt("one more").await;
    let first_session = h.requests();
    assert_eq!(first_session.len(), 4);
    // A new session in the same project the next day, with its MCP servers
    // started afresh as a new process would.
    if h.mcp.is_some() {
        let mcp = codesmith_core::mcp::Mcp::connect(&h.loaded.config.mcp, &h.loaded.root).await;
        h.mcp = Some(mcp);
    }
    h.renew();
    h.s.set_today(today(&day));
    h.prompt("hello again").await;
    let all = h.requests();
    let second = all.last().unwrap();

    let p0 = prefix(&first_session[0]);
    assert!(p0.1.len() == 1 && p0.1[0]["role"] == "system", "{:?}", p0.1);
    let system = p0.1[0]["content"].as_str().unwrap();
    assert!(system.contains("Run the tests before committing."));
    assert!(system.contains("Deploy the app"));
    let tools: Vec<&str> = p0.0["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"websearch"), "{tools:?}");
    if py.is_some() {
        assert!(tools.iter().any(|t| t.starts_with("alpha_")), "{tools:?}");
        assert!(tools.contains(&"execute"), "{tools:?}");
    }
    // A local server gets no per-session cache key.
    assert!(p0.0.get("prompt_cache_key").is_none());
    for (i, b) in first_session.iter().chain([second]).enumerate() {
        let p = prefix(b);
        assert_eq!(
            serde_json::to_string(&p.0).unwrap(),
            serde_json::to_string(&p0.0).unwrap(),
            "request {i}: the body around the messages changed"
        );
        assert_eq!(
            serde_json::to_string(&p.1).unwrap(),
            serde_json::to_string(&p0.1).unwrap(),
            "request {i}: the messages before the first user message changed"
        );
    }
    // The date is not in that prefix: it comes first after it, as a system
    // update, so the prefix is the same from one day to the next.
    assert!(!system.contains("Today's date"), "{system}");
    let date = |b: &Value| b["messages"][1]["content"].as_str().unwrap().to_string();
    assert_eq!(
        date(&first_session[0]),
        "<system-update>\nToday's date: Thu Nov 19 2026\n</system-update>"
    );
    assert_eq!(
        date(second),
        "<system-update>\nToday's date: Fri Nov 20 2026\n</system-update>"
    );
    // Within a session, each request extends the one before, the day
    // change included: the new date is an update after the history.
    let msgs = |b: &Value| serde_json::to_string(&b["messages"].as_array().unwrap()[..3]).unwrap();
    for pair in first_session.windows(2) {
        assert_eq!(msgs(&pair[0]), msgs(&pair[1]));
    }
    let late = serde_json::to_string(&first_session[3]["messages"]).unwrap();
    assert!(
        late.contains("Today's date is now: Fri Nov 20 2026"),
        "{late}"
    );
    let earlier = serde_json::to_string(&first_session[2]["messages"]).unwrap();
    let earlier = earlier.trim_end_matches(']');
    assert!(
        late.starts_with(earlier),
        "the history before it is unchanged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_provider_keeps_the_date_in_the_system_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::default();
    let base = server(log.clone(), Duration::ZERO).await;
    let extra = json!({"title": false, "providers": {"main": {"local": false}}});
    let mut s = session(dir.path(), &base, "m", extra);
    s.set_today(|| "Thu Nov 19 2026".to_string());
    s.prompt("hello", &mut Quiet).await.unwrap();
    let (_, main) = split(&log);
    let messages = main[0].body["messages"].as_array().unwrap().clone();
    let system = messages[0]["content"].as_str().unwrap();
    assert!(
        system.contains("Today's date: Thu Nov 19 2026"),
        "as in OpenCode: {system}"
    );
    assert!(!serde_json::to_string(&messages[1..])
        .unwrap()
        .contains("Today's date"));
}
