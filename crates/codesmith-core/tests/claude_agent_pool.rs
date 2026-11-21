//! The Claude Agent process pool (CodeSmith's own, beside the OpenCode
//! ports in `oc_claude_agent.rs`): prewarming, spares and the process cap,
//! against the fake `claude` (see `claude_fake`). The real `claude` never
//! runs.
//!
//! What every test holds to: a process serves one session for good once
//! it carried a turn, a spare is never-used until one new session claims
//! it, each session keeps its own Claude session, model and effort, and no
//! more fake `claude`s are ever alive than the cap. Waits are on the fake's
//! and the registry's own signals, under one deadline each.

mod claude_fake;

use claude_fake::{expect_interrupt, expect_startup, expect_turn, replay, turn, Fake, Gate, Step};
use codesmith_ai::claude_agent::{self as agent, registry, Io, Pool, Registry, Timing};
use codesmith_ai::{Error, Event, Message, Part, Protocol, Provider, Request, Role};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const A_TEXT: &str = "ALPHA-MARKER: remember COPPER-OTTER. Reply only with that marker.";
const B_TEXT: &str = "BRAVO-MARKER: remember COPPER-OTTER. Reply only with that marker.";
const C_TEXT: &str = "CHARLIE-MARKER: remember COPPER-OTTER. Reply only with that marker.";
const D_TEXT: &str = "DELTA-MARKER: remember COPPER-OTTER. Reply only with that marker.";
const FOLLOWUP: &str = "What was the marker? Reply only with it.";
const DEADLINE: Duration = Duration::from_secs(30);

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, f)
        .await
        .unwrap_or_else(|_| panic!("{what} within {DEADLINE:?}"))
}

struct Test {
    dir: tempfile::TempDir,
    fake: Fake,
    registry: Registry,
    pool: Pool,
    timing: Timing,
    _server: tokio::task::JoinHandle<()>,
}

fn io_for(dir: &std::path::Path) -> Io {
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), dir.join("home").display().to_string());
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        dir.join("claude").display().to_string(),
    );
    env.insert(
        "FAKE_CLAUDE_SOCKET".into(),
        dir.join("s").display().to_string(),
    );
    Io {
        which: Arc::new(|| Some(claude_fake::executable())),
        env: Arc::new(move || env.clone()),
        storage: dir.join("storage"),
        observe: None,
        recheck: false,
    }
}

fn pool(prewarm: bool, spare: usize, max_processes: usize) -> Pool {
    Pool {
        prewarm,
        spare,
        max_processes,
    }
}

impl Test {
    async fn new(script: Vec<Step>, pool: Pool, timing: Timing) -> Test {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        let fake = Fake::new(script, false, &dir.path().join("claude"));
        let server = fake.serve(&dir.path().join("s"));
        let registry = Registry::with_pool(io_for(dir.path()), timing, pool);
        Test {
            dir,
            fake,
            registry,
            pool,
            timing,
            _server: server,
        }
    }

    /// A second registry over the same storage, as after a restart.
    fn restart(&mut self) {
        self.registry = Registry::with_pool(io_for(self.dir.path()), self.timing, self.pool);
    }

    fn work(&self) -> String {
        self.dir.path().join("work").display().to_string()
    }

    fn request(&self, ask: &Ask) -> registry::Request {
        ask.request(&self.work())
    }

    fn stream(&self, ask: &Ask) -> codesmith_ai::EventStream {
        agent::stream_with(&self.registry, self.request(ask))
    }

    fn spawn(&self, ask: &Ask) -> tokio::task::JoinHandle<Parts> {
        let s = self.stream(ask);
        tokio::spawn(collect(s))
    }

    async fn reply(&self, ask: Ask) -> Parts {
        within("the reply finished", collect(self.stream(&ask))).await
    }

    async fn settled(&self) {
        within("prewarms and spares settled", self.registry.settled()).await
    }

    async fn close(&self) {
        within("the registry closed", self.registry.close()).await;
    }

    fn record(&self, session: &str) -> agent::store::Record {
        self.registry
            .records()
            .get(session)
            .unwrap_or_else(|| panic!("no record for {session}"))
    }

    /// No more fake `claude`s alive than the cap, now or ever.
    fn assert_capped(&self) {
        let max = self.registry.pool().max_processes;
        assert!(self.fake.peak() <= max, "peak {} > {max}", self.fake.peak());
        assert!(self.registry.live() <= max);
    }
}

fn check(fake: &Fake) {
    if let Err(e) = fake.check() {
        panic!("{e}");
    }
}

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::text(text)],
    }
}

fn followup(first: &str) -> Vec<Message> {
    vec![user(first), assistant("COPPER-OTTER"), user(FOLLOWUP)]
}

/// One model request, as CodeSmith's session sends it.
#[derive(Clone)]
struct Ask {
    model: String,
    session: String,
    messages: Vec<Message>,
    options: serde_json::Map<String, Value>,
}

impl Ask {
    fn new(session: &str, messages: Vec<Message>) -> Ask {
        Ask {
            model: "haiku".into(),
            session: session.into(),
            messages,
            options: Default::default(),
        }
    }
    fn model(mut self, m: &str) -> Ask {
        self.model = m.into();
        self
    }
    fn effort(mut self, e: &str) -> Ask {
        self.options.insert("effort".into(), json!(e));
        self
    }
    /// The agent's own prompt (a subagent's differs from its parent's).
    fn role(mut self, r: &str) -> Ask {
        self.options.insert(agent::AGENT_OPTION.into(), json!(r));
        self
    }

    fn request(&self, dir: &str) -> registry::Request {
        let mut options = self.options.clone();
        options.insert(agent::DIRECTORY_OPTION.into(), json!(dir));
        let req = Request {
            model: self.model.clone(),
            system: "You are a concise test assistant.".into(),
            messages: self.messages.clone(),
            options: codesmith_ai::Options::from_map(options),
            headers: vec![
                (agent::SESSION_HEADER.to_string(), self.session.clone()),
                (agent::KIND_HEADER.to_string(), "primary".to_string()),
            ],
            ..Default::default()
        };
        let p = Provider {
            protocol: Protocol::ClaudeAgent,
            name: agent::ID.into(),
            ..Default::default()
        };
        agent::agent_request(&p, &req).unwrap()
    }
}

type Parts = Vec<Result<Event, String>>;

async fn collect(mut s: codesmith_ai::EventStream) -> Parts {
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e.map_err(|e: Error| e.to_string()));
    }
    out
}

fn text(parts: &Parts) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Ok(Event::TextDelta(d)) => Some(d.as_str()),
            _ => None,
        })
        .collect()
}

fn error(parts: &Parts) -> Option<String> {
    parts.iter().find_map(|p| p.as_ref().err().cloned())
}

fn answered(parts: &Parts) {
    assert_eq!(error(parts), None);
    assert_eq!(text(parts), "COPPER-OTTER");
}

/// The texts of a run's user messages.
fn said(run: &claude_fake::Run) -> Vec<String> {
    run.users()
        .iter()
        .map(|u| {
            u["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

/// A run heard only `own` of the session markers: no other session's
/// words ever reached it, in any message.
fn heard_only(run: &claude_fake::Run, own: &str) {
    let all = serde_json::to_string(&run.received()).unwrap();
    for marker in [
        "ALPHA-MARKER",
        "BRAVO-MARKER",
        "CHARLIE-MARKER",
        "DELTA-MARKER",
    ] {
        assert_eq!(
            all.contains(marker),
            own.contains(marker),
            "run {} and {marker}",
            run.pid
        );
    }
}

fn controls(subtype: &str) -> Step {
    replay("controls")
        .into_iter()
        .find(|s| s.is_exchange_of(subtype))
        .unwrap_or_else(|| panic!("no {subtype} in the controls recording"))
}

// Two sessions running at once each have their own `claude`: distinct
// processes, each hearing only its own session, each with its own Claude
// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_running_at_once_never_share_a_process() {
    let held = Gate::default();
    let a_turn = expect_turn().before_result(&held);
    let t = Test::new(
        vec![
            expect_startup(),
            a_turn.clone(),
            expect_startup(),
            expect_turn(),
            expect_interrupt(),
            expect_interrupt(),
        ],
        pool(false, 0, 4),
        Timing::default(),
    )
    .await;
    let a = t.spawn(&Ask::new("session-a", vec![user(A_TEXT)]));
    within("A's turn started", a_turn.started.wait()).await;
    // B runs a whole turn while A's is still going.
    answered(&t.reply(Ask::new("session-b", vec![user(B_TEXT)])).await);
    assert!(!held.is_open());
    held.open();
    answered(&within("A finished", a).await.unwrap());

    let (ra, rb) = (t.fake.run(0), t.fake.run(1));
    assert_eq!(t.fake.runs().len(), 2);
    assert_ne!(ra.pid, rb.pid);
    assert_eq!(said(&ra), [A_TEXT]);
    assert_eq!(said(&rb), [B_TEXT]);
    heard_only(&ra, A_TEXT);
    heard_only(&rb, B_TEXT);
    let pids = t.registry.pids().await;
    assert_eq!(pids["session-a"], ra.pid);
    assert_eq!(pids["session-b"], rb.pid);
    assert_eq!(
        t.record("session-a").claude_session_id,
        ra.session().unwrap()
    );
    assert_eq!(
        t.record("session-b").claude_session_id,
        rb.session().unwrap()
    );
    assert_ne!(ra.session(), rb.session());
    t.assert_capped();
    t.close().await;
    check(&t.fake);
}

// Each new session claims a never-used spare, which leaves the pool for
// good; a process that served a session is never handed to another, even
// once it stopped: at the cap the least recently used idle one stops, and
// its session resumes its own Claude session in a new process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spare_serves_one_new_session_and_a_used_process_never_serves_another() {
    let t = Test::new(
        vec![
            expect_startup(),       // 0: A's process (no spare yet)
            expect_turn(),          // 1: A
            expect_startup(),       // 2: spare 1
            expect_turn(),          // 3: B, on spare 1
            expect_startup(),       // 4: spare 2
            expect_turn(),          // 5: C, on spare 2 (no room for another)
            expect_interrupt(),     // 6: A's process stops for D
            expect_startup(),       // 7: D's process
            expect_turn(),          // 8: D
            expect_interrupt(),     // 9: B's process stops for A
            expect_startup(),       // 10: A resumes in a new process
            turn("text-resume", 0), // 11: A's follow-up
            expect_interrupt(),     // 12-14: closing C, D and A
            expect_interrupt(),
            expect_interrupt(),
        ],
        pool(false, 1, 3),
        Timing::default(),
    )
    .await;
    answered(&t.reply(Ask::new("session-a", vec![user(A_TEXT)])).await);
    t.settled().await;
    assert_eq!(t.registry.spares(), 1);
    let spare1 = t.fake.run(1);
    assert!(spare1.users().is_empty());
    assert!(!spare1.initialize().is_null());

    answered(&t.reply(Ask::new("session-b", vec![user(B_TEXT)])).await);
    t.settled().await;
    assert_eq!(t.fake.runs().len(), 3, "B claimed the spare");
    answered(&t.reply(Ask::new("session-c", vec![user(C_TEXT)])).await);
    t.settled().await;
    // Three processes are alive: no room for another spare.
    assert_eq!(t.registry.spares(), 0);
    assert_eq!(t.fake.runs().len(), 3);
    t.assert_capped();

    // D: no spare and no room, so the least recently used idle process
    // (A's) stops.
    answered(&t.reply(Ask::new("session-d", vec![user(D_TEXT)])).await);
    let (ra, rb, rc, rd) = (t.fake.run(0), t.fake.run(1), t.fake.run(2), t.fake.run(3));
    within("A's process stopped", ra.closed.wait()).await;
    assert_eq!(ra.requests("interrupt").len(), 1);
    t.assert_capped();

    // A again: B's process (now the least recently used) stops, and A
    // resumes its own Claude session in a new process, not in any other
    // session's.
    answered(&t.reply(Ask::new("session-a", followup(A_TEXT))).await);
    within("B's process stopped", rb.closed.wait()).await;
    let ra2 = t.fake.run(4);
    assert_eq!(t.fake.runs().len(), 5);
    assert_eq!(ra2.arg("--resume="), ra.session());
    assert_eq!(
        t.record("session-a").claude_session_id,
        ra.session().unwrap()
    );
    assert_eq!(said(&ra2), [FOLLOWUP]);

    // Each process heard one session only, from its first message.
    assert_eq!(said(&ra), [A_TEXT]);
    assert_eq!(said(&rb), [B_TEXT]);
    assert_eq!(said(&rc), [C_TEXT]);
    assert_eq!(said(&rd), [D_TEXT]);
    for (run, own) in [
        (&ra, A_TEXT),
        (&rb, B_TEXT),
        (&rc, C_TEXT),
        (&rd, D_TEXT),
        (&ra2, ""),
    ] {
        heard_only(run, own);
    }
    // A spare was claimed after it started and before it heard anything.
    for spare in [&rb, &rc] {
        assert_eq!(spare.received()[0]["request"]["subtype"], "initialize");
        assert_eq!(spare.users().len(), 1);
    }
    let pids: Vec<u32> = t.fake.runs().iter().map(|r| r.pid).collect();
    let distinct: std::collections::HashSet<_> = pids.iter().collect();
    assert_eq!(distinct.len(), pids.len());
    let bound = t.registry.pids().await;
    assert_eq!(bound.get("session-a"), Some(&ra2.pid));
    assert_eq!(bound.get("session-c"), Some(&rc.pid));
    assert_eq!(bound.get("session-d"), Some(&rd.pid));
    assert_eq!(bound.get("session-b"), None);
    let ids: std::collections::HashSet<String> =
        ["session-a", "session-b", "session-c", "session-d"]
            .iter()
            .map(|s| t.record(s).claude_session_id)
            .collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(t.fake.peak(), 3);
    t.assert_capped();
    t.close().await;
    check(&t.fake);
}

/// What a flow showed Claude Code, without the random ids.
#[derive(Debug, PartialEq)]
struct Seen {
    texts: Vec<String>,
    runs: usize,
    said: Vec<Vec<String>>,
    args: Vec<Vec<String>>,
    initialize: Vec<Value>,
    history: Vec<String>,
}

async fn first_turn_then_resume(prewarm: bool) -> Seen {
    let mut t = Test::new(
        vec![
            expect_startup(),
            expect_turn(),
            expect_interrupt(),
            expect_startup(),
            turn("text-resume", 0),
            expect_interrupt(),
        ],
        pool(prewarm, 0, 4),
        Timing::default(),
    )
    .await;
    let first = Ask::new("session-a", vec![user(A_TEXT)]);
    if prewarm {
        t.registry.prewarm(t.request(&first));
        t.settled().await;
        // Started and bound before the turn, having heard nothing.
        assert_eq!(t.fake.runs().len(), 1);
        assert!(t.fake.run(0).users().is_empty());
        assert_eq!(t.registry.pids().await["session-a"], t.fake.run(0).pid);
    }
    let one = t.reply(first).await;
    // The prewarmed process served the first turn.
    assert_eq!(t.fake.runs().len(), 1);
    t.close().await;
    let id = t.fake.run(0).session().unwrap();

    t.restart();
    let next = Ask::new("session-a", followup(A_TEXT));
    if prewarm {
        t.registry.prewarm(t.request(&next));
        t.settled().await;
        assert_eq!(t.fake.runs().len(), 2);
        assert_eq!(t.fake.run(1).arg("--resume="), Some(id.clone()));
        assert!(t.fake.run(1).users().is_empty());
    }
    let two = t.reply(next).await;
    assert_eq!(t.fake.runs().len(), 2);
    assert_eq!(t.fake.run(1).arg("--resume="), Some(id.clone()));
    let record = t.record("session-a");
    assert_eq!(record.claude_session_id, id);
    t.close().await;
    check(&t.fake);
    let runs = t.fake.runs();
    Seen {
        texts: vec![text(&one), text(&two)],
        runs: runs.len(),
        said: runs.iter().map(|r| said(r)).collect(),
        args: runs
            .iter()
            .map(|r| {
                r.args
                    .iter()
                    .map(|a| match a.split_once('=') {
                        Some((k @ ("--session-id" | "--resume"), _)) => format!("{k}=*"),
                        _ => a.clone(),
                    })
                    .collect()
            })
            .collect(),
        initialize: runs.iter().map(|r| r.initialize()).collect(),
        history: record
            .history
            .unwrap_or_default()
            .iter()
            .map(|c| c.hash.clone())
            .collect(),
    }
}

// Prewarming changes when Claude Code starts, nothing else: the same
// processes, flags, start-up requests, messages, replies and checkpoints,
// a new session starting a new Claude session and a known one resuming
// its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prewarm_changes_nothing_but_when_claude_code_starts() {
    let cold = first_turn_then_resume(false).await;
    let warm = first_turn_then_resume(true).await;
    assert_eq!(cold.texts, ["COPPER-OTTER", "COPPER-OTTER"]);
    assert_eq!(cold.said, [vec![A_TEXT.to_string()], vec![FOLLOWUP.into()]]);
    assert_eq!(warm, cold);
}

// A prewarmed process the turn cannot use (here the agent changed, so the
// system prompt did) is stopped unused, without an interrupt, and the turn
// starts its own; the prewarmed one never hears anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prewarmed_process_the_turn_cannot_use_stops_having_heard_nothing() {
    let t = Test::new(
        vec![
            expect_startup(),
            expect_startup(),
            expect_turn(),
            expect_interrupt(),
        ],
        pool(true, 0, 4),
        Timing::default(),
    )
    .await;
    let ask = Ask::new("session-a", vec![user(A_TEXT)]);
    t.registry
        .prewarm(t.request(&ask.clone().role("You plan.")));
    t.settled().await;
    answered(&t.reply(ask.role("You build.")).await);
    let (warm, used) = (t.fake.run(0), t.fake.run(1));
    within("the prewarmed process stopped", warm.closed.wait()).await;
    assert!(warm.users().is_empty());
    assert!(warm.requests("interrupt").is_empty());
    assert!(warm.initialize()["appendSystemPrompt"]
        .as_str()
        .unwrap()
        .contains("You plan."));
    assert!(used.initialize()["appendSystemPrompt"]
        .as_str()
        .unwrap()
        .contains("You build."));
    assert_eq!(said(&used), [A_TEXT]);
    t.close().await;
    check(&t.fake);
}

// Two sessions with different models and efforts, running at once, each
// get theirs: a claimed spare is switched to the claiming session's model
// and effort on its own process, nothing is sent to the other session's,
// and a subagent's child session (its own agent, so its own system
// prompt, model and effort) gets a process of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_and_subagents_keep_their_own_model_effort_and_process() {
    let held = Gate::default();
    let a_second = turn("text", 1).before_result(&held);
    let t = Test::new(
        vec![
            expect_startup(),                // 0: A (haiku, low)
            expect_turn(),                   // 1: A
            expect_startup(),                // 2: spare, started like A
            a_second.clone(),                // 3: A again, held
            controls("set_model"),           // 4: B claims the spare...
            controls("apply_flag_settings"), // 5: ...and sets its own
            expect_turn(),                   // 6: B
            expect_startup(),                // 7: a new spare, started like B
            expect_startup(),                // 8: the subagent's own process
            expect_turn(),                   // 9: the subagent
            expect_interrupt(),              // 10-12: closing A, B and the subagent
            expect_interrupt(),
            expect_interrupt(),
        ],
        pool(false, 1, 4),
        Timing::default(),
    )
    .await;
    let a = Ask::new("session-a", vec![user(A_TEXT)]).effort("low");
    answered(&t.reply(a).await);
    t.settled().await;
    let a = t.spawn(&Ask::new("session-a", followup(A_TEXT)).effort("low"));
    within("A's second turn started", a_second.started.wait()).await;
    let b = Ask::new("session-b", vec![user(B_TEXT)])
        .model("sonnet")
        .effort("high");
    answered(&t.reply(b).await);
    held.open();
    answered(&within("A finished", a).await.unwrap());
    t.settled().await;
    let child = Ask::new("session-a-subagent", vec![user(C_TEXT)])
        .role("You explore the code.")
        .model("haiku");
    answered(&t.reply(child).await);

    let (ra, rb, spare, rc) = (t.fake.run(0), t.fake.run(1), t.fake.run(2), t.fake.run(3));
    assert_eq!(t.fake.runs().len(), 4);
    // A: started with its own model and effort, never switched.
    assert!(ra.args.contains(&"--model=haiku".to_string()));
    assert!(ra.args.contains(&"--effort=low".to_string()));
    assert!(ra.requests("set_model").is_empty());
    assert!(ra.requests("apply_flag_settings").is_empty());
    assert_eq!(said(&ra), [A_TEXT, FOLLOWUP]);
    // B: the spare (started like A), switched to B's before B's input.
    let kinds: Vec<Value> = rb
        .received()
        .iter()
        .map(|m| {
            if m["type"] == "user" {
                json!("user")
            } else {
                m["request"].clone()
            }
        })
        .collect();
    assert_eq!(kinds[0]["subtype"], "initialize");
    assert_eq!(
        kinds[1..],
        [
            json!({"subtype": "set_model", "model": "sonnet"}),
            json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": "high"}}),
            json!("user"),
        ]
    );
    assert_eq!(said(&rb), [B_TEXT]);
    heard_only(&ra, A_TEXT);
    heard_only(&rb, B_TEXT);
    heard_only(&rc, C_TEXT);
    // The next spare starts like B, the last to claim one.
    assert!(spare.args.contains(&"--model=sonnet".to_string()));
    assert!(spare.args.contains(&"--effort=high".to_string()));
    assert!(spare.users().is_empty());
    // The subagent: its own prompt, so not the spare; its own model and
    // no effort.
    assert_eq!(t.registry.spares(), 1);
    assert!(rc.args.contains(&"--model=haiku".to_string()));
    assert!(!rc.args.iter().any(|a| a.starts_with("--effort=")));
    assert!(rc.initialize()["appendSystemPrompt"]
        .as_str()
        .unwrap()
        .contains("You explore the code."));
    assert!(!ra.initialize()["appendSystemPrompt"]
        .as_str()
        .unwrap()
        .contains("You explore the code."));
    assert_eq!(said(&rc), [C_TEXT]);
    let pids = t.registry.pids().await;
    assert_eq!(pids["session-a"], ra.pid);
    assert_eq!(pids["session-b"], rb.pid);
    assert_eq!(pids["session-a-subagent"], rc.pid);
    assert_eq!(
        t.record("session-b").claude_session_id,
        rb.session().unwrap()
    );
    assert_eq!(
        t.record("session-a-subagent").claude_session_id,
        rc.session().unwrap()
    );
    t.close().await;
    within("the spare stopped", spare.closed.wait()).await;
    assert!(spare.requests("interrupt").is_empty());
    check(&t.fake);
}

// A burst of sessions never has more `claude`s alive than the cap: the
// sessions over it wait, a busy process (mid-turn) is never stopped for
// them, and each idle one that finished makes room for the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_sessions_never_exceeds_the_cap_and_never_stops_a_busy_process() {
    let (ga, gb) = (Gate::default(), Gate::default());
    let a_turn = expect_turn().before_result(&ga);
    let b_turn = expect_turn().before_result(&gb);
    let t = Test::new(
        vec![
            expect_startup(),
            a_turn.clone(),
            expect_startup(),
            b_turn.clone(),
            expect_interrupt(), // A, finished, stops for C or D
            expect_startup(),
            expect_turn(),
            expect_interrupt(), // that one, finished, stops for the other
            expect_startup(),
            expect_turn(),
            expect_interrupt(), // closing B and the last
            expect_interrupt(),
        ],
        pool(false, 0, 2),
        Timing::default(),
    )
    .await;
    let a = t.spawn(&Ask::new("session-a", vec![user(A_TEXT)]));
    within("A's turn started", a_turn.started.wait()).await;
    let b = t.spawn(&Ask::new("session-b", vec![user(B_TEXT)]));
    within("B's turn started", b_turn.started.wait()).await;
    let c = t.spawn(&Ask::new("session-c", vec![user(C_TEXT)]));
    let d = t.spawn(&Ask::new("session-d", vec![user(D_TEXT)]));
    let mut queued = t.registry.queued();
    within("C and D queued", queued.wait_for(|n| *n == 2))
        .await
        .unwrap();
    assert_eq!(t.fake.runs().len(), 2);
    assert_eq!(t.registry.live(), 2);
    t.assert_capped();

    ga.open();
    answered(&within("A finished", a).await.unwrap());
    answered(&within("C finished", c).await.unwrap());
    answered(&within("D finished", d).await.unwrap());
    // B is mid-turn the whole time: never stopped.
    let rb = t.fake.run(1);
    assert!(!rb.closed.is_open());
    assert!(rb.requests("interrupt").is_empty());
    gb.open();
    answered(&within("B finished", b).await.unwrap());

    assert_eq!(t.fake.runs().len(), 4);
    assert!(t.fake.run(0).closed.is_open());
    assert_eq!(t.fake.peak(), 2);
    t.assert_capped();
    for (i, own) in [(0, A_TEXT), (1, B_TEXT)] {
        assert_eq!(said(&t.fake.run(i)), [own]);
    }
    // C and D each had a process of their own, in either order.
    let mut later: Vec<String> = [2, 3].iter().flat_map(|i| said(&t.fake.run(*i))).collect();
    later.sort();
    assert_eq!(later, [C_TEXT, D_TEXT]);
    t.close().await;
    check(&t.fake);
}

// With every process busy a request waits, and fails after the queue time
// with what to change, or ends at once when cancelled; neither starts a
// process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_over_the_cap_waits_then_fails_or_is_cancelled_without_starting_claude() {
    let held = Gate::default();
    let a_turn = expect_turn().before_result(&held);
    let t = Test::new(
        vec![expect_startup(), a_turn.clone(), expect_interrupt()],
        pool(false, 0, 1),
        Timing {
            queue: Duration::from_millis(300),
            ..Timing::default()
        },
    )
    .await;
    let a = t.spawn(&Ask::new("session-a", vec![user(A_TEXT)]));
    within("A's turn started", a_turn.started.wait()).await;
    let failed = t.reply(Ask::new("session-b", vec![user(B_TEXT)])).await;
    let e = error(&failed).unwrap();
    assert!(e.contains("claude_agent.max_processes"), "{e}");
    assert_eq!(t.fake.runs().len(), 1);

    let mut queued = t.registry.queued();
    let s = t.stream(&Ask::new("session-c", vec![user(C_TEXT)]));
    within("C queued", queued.wait_for(|n| *n == 1))
        .await
        .unwrap();
    drop(s);
    within("C gave up", queued.wait_for(|n| *n == 0))
        .await
        .unwrap();
    assert_eq!(t.fake.runs().len(), 1);
    assert!(t.registry.records().get("session-b").is_none());
    assert!(t.registry.records().get("session-c").is_none());

    held.open();
    answered(&within("A finished", a).await.unwrap());
    t.assert_capped();
    t.close().await;
    check(&t.fake);
}

// Idle stop still applies: a prewarmed process that no turn used stops
// after the idle time without an interrupt (it has nothing to drain), a
// session's process stops after it as before, and so does an unclaimed
// spare.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unused_prewarmed_and_spare_processes_stop_after_the_idle_time() {
    let t = Test::new(
        vec![
            expect_startup(), // 0: prewarmed, never used
            expect_startup(), // 1: A's turn
            expect_turn(),
            expect_startup(),   // 2: the spare
            expect_interrupt(), // A's process, idle
        ],
        pool(true, 1, 4),
        Timing {
            idle: Duration::from_secs(1),
            ..Timing::default()
        },
    )
    .await;
    let ask = Ask::new("session-a", vec![user(A_TEXT)]);
    t.registry.prewarm(t.request(&ask));
    t.settled().await;
    let warm = t.fake.run(0);
    within("the prewarmed process idled out", warm.closed.wait()).await;
    assert!(warm.users().is_empty());
    assert!(warm.requests("interrupt").is_empty());
    within("its slot was given back", t.registry.live_until(|n| n == 0)).await;

    answered(&t.reply(ask).await);
    t.settled().await;
    assert_eq!(t.registry.spares(), 1);
    let (used, spare) = (t.fake.run(1), t.fake.run(2));
    within("A's process idled out", used.closed.wait()).await;
    within("the spare idled out", spare.closed.wait()).await;
    assert_eq!(used.requests("interrupt").len(), 1);
    assert!(spare.requests("interrupt").is_empty());
    assert!(spare.users().is_empty());
    within(
        "every slot was given back",
        t.registry.live_until(|n| n == 0),
    )
    .await;
    assert_eq!(t.registry.spares(), 0);
    // A resumes its own Claude session next time, as after any idle stop.
    assert_eq!(
        t.record("session-a").claude_session_id,
        used.session().unwrap()
    );
    t.close().await;
    check(&t.fake);
}
