//! Ports of OpenCode's goal plugin tests (`packages/core/test/plugin/`
//! `goal.test.ts` and `goal-watchdog.test.ts`, MIT): `/goal` keeps a
//! session working until a reviewer finds the objective met, pausing when
//! it stalls, cannot judge, is stopped, or waits too long. Its own test
//! binary, because the plugin registry and host are process-wide. See
//! `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: the kickoff and continuations are user messages (CodeSmith has
//! no synthetic notices), and each test runs a real session against a
//! scripted model.

mod plugin_support;

use codesmith_ai::{Role, Usage};
use codesmith_core::command;
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::goal::{Change, Goal, GoalPlugin, Status, WAIT_LIMIT};
use codesmith_core::plugin::host::NewSession;
use codesmith_core::plugin::{self, CommandContext, Event, Host, Plugin};
use codesmith_core::session::{Session, Sink};
use codesmith_core::store::Store;
use plugin_support::{overflow, serve, text, tool, Model, Reply as Answer, Seen};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REVIEW_MARKER: &str = "You are now the reviewer for the session's goal";

fn is_review(r: &Seen) -> bool {
    r.last_text().contains(REVIEW_MARKER)
}

fn is_summary(r: &Seen) -> bool {
    r.last_text().contains("summary")
}

/// The goal plugin, installed once for this binary.
fn installed() -> GoalPlugin {
    static GOAL: std::sync::OnceLock<GoalPlugin> = std::sync::OnceLock::new();
    GOAL.get_or_init(|| {
        let g = GoalPlugin::new();
        plugin::install(vec![Arc::new(g.clone())]);
        g
    })
    .clone()
}

/// Answers review requests from a script and every other request as work.
#[derive(Clone)]
struct Reviewer {
    queue: Arc<Mutex<std::collections::VecDeque<String>>>,
    asked: Arc<AtomicUsize>,
}

impl Reviewer {
    fn new(replies: &[&str]) -> Reviewer {
        Reviewer {
            queue: Arc::new(Mutex::new(replies.iter().map(|s| s.to_string()).collect())),
            asked: Arc::default(),
        }
    }
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
    fn review(&self) -> Answer {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let next = self.queue.lock().unwrap().pop_front();
        text(&next.unwrap_or_else(|| r#"{"verdict":"done","reason":"Out of script."}"#.into()))
    }
    fn respond(&self) -> impl Fn(&Seen) -> Answer + Send + Sync + 'static {
        let me = self.clone();
        move |r| {
            if is_review(r) {
                me.review()
            } else {
                text("Working on it")
            }
        }
    }
    /// Each round starts with a tool call and ends with text once the
    /// tool result is in.
    fn with_tools(&self) -> impl Fn(&Seen) -> Answer + Send + Sync + 'static {
        let me = self.clone();
        move |r| {
            if is_review(r) {
                return me.review();
            }
            let last = r.body["messages"].as_array().unwrap().last().unwrap();
            if last["role"] == "tool" {
                text("Checked.")
            } else {
                tool(
                    &format!("call-{}", me.asked()),
                    "missing_tool",
                    json!({"round": me.asked()}),
                )
            }
        }
    }
}

#[derive(Debug, Default)]
struct Out {
    compactions: usize,
}

impl Sink for Out {
    fn text(&mut self, _: &str) {}
    fn compacted(&mut self, _: u64, _: u64) {
        self.compactions += 1;
    }
}

struct G {
    dir: tempfile::TempDir,
    host: Arc<Host>,
    model: Model,
    goal: GoalPlugin,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl G {
    async fn new(respond: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> G {
        let lock = plugin_support::serial();
        let goal = installed();
        let dir = tempfile::tempdir().unwrap();
        let host =
            Arc::new(Host::open(&dir.path().join("db.sqlite"), dir.path().to_path_buf()).unwrap());
        plugin::set_host(host.clone());
        let model = serve(respond).await;
        G {
            dir,
            host,
            model,
            goal,
            _lock: lock,
        }
    }

    fn loaded(&self) -> codesmith_core::config::Loaded {
        plugin_support::loaded(self.dir.path(), &self.model.base, json!({}))
    }

    /// A stored session (untitled unless `title`), and a session object on it.
    fn session(&self, title: Option<&str>, parent: Option<&str>) -> Session {
        let info = self
            .host
            .create(NewSession {
                title: title.map(str::to_string),
                parent_id: parent.map(str::to_string),
                ..Default::default()
            })
            .unwrap();
        self.open(&info.id)
    }

    fn open(&self, id: &str) -> Session {
        let mut o = plugin_support::options();
        o.resume = Some(id.to_string());
        o.title = None;
        let store = Store::open(&self.dir.path().join("db.sqlite")).unwrap();
        Session::new(
            &self.loaded(),
            self.dir.path().to_path_buf(),
            o,
            Arc::new(Fixed(Reply::Once)),
            Some(store),
        )
        .unwrap()
    }

    async fn command(&self, s: &mut Session, args: &str) -> anyhow::Result<Out> {
        let c = command::plugin_commands(&self.loaded().config)["goal"].clone();
        let mut out = Out::default();
        s.run_command(&format!("/goal {args}"), &c, args, vec![], &mut out)
            .await?;
        Ok(out)
    }

    fn get(&self, id: &str) -> Option<Goal> {
        self.goal.get(id)
    }

    fn set(&self, id: &str, goal: Value) {
        self.host
            .storage("goal")
            .set(&format!("session:{id}"), &goal)
            .unwrap();
    }
}

/// The user messages the goal sent: kickoffs and continuations.
fn goal_messages(s: &Session) -> Vec<String> {
    s.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    codesmith_ai::Part::Text(t) => Some(t.as_str().to_string()),
                    _ => None,
                })
                .collect::<String>()
        })
        .filter(|t| t.contains("<untrusted_objective>"))
        .collect()
}

// goal.test.ts: "keeps working in one execution until the reviewer finds the goal met"
#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_working_in_one_execution_until_the_reviewer_finds_the_goal_met() {
    let verdicts = Reviewer::new(&[
        r#"{"verdict":"continue","reason":"The parser test still fails.","next_step":"Fix the date parser."}"#,
        "```json\n{\"verdict\":\"done\",\"reason\":\"All 12 tests pass.\"}\n```",
    ]);
    let g = G::new(verdicts.respond()).await;
    let mut s = g.session(None, None);
    g.command(&mut s, "  Make the test suite pass  ")
        .await
        .unwrap();

    assert_eq!(verdicts.asked(), 2);
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.objective, "Make the test suite pass");
    assert_eq!(goal.status, Status::Complete);
    assert_eq!(goal.reason.as_deref(), Some("All 12 tests pass."));
    assert_eq!(goal.rounds, 1);
    let sent = goal_messages(&s);
    assert_eq!(sent.len(), 2, "the kickoff and one continuation: {sent:?}");
    assert!(
        sent[0].contains("<untrusted_objective>\nMake the test suite pass\n</untrusted_objective>")
    );
    assert!(sent[1].contains("The parser test still fails.\nNext step: Fix the date parser."));
    assert_eq!(
        g.host.get(&s.id).unwrap().title.as_deref(),
        Some("Make the test suite pass"),
        "an untitled session is named after the objective"
    );
}

async fn pauses_when_it_stalls(tools: bool, reasons: &[&str]) {
    let mut replies: Vec<String> = reasons
        .iter()
        .map(|r| json!({"verdict": "continue", "reason": r}).to_string())
        .collect();
    replies.push(r#"{"verdict":"done","reason":"Unreachable."}"#.into());
    let replies: Vec<&str> = replies.iter().map(String::as_str).collect();
    let verdicts = Reviewer::new(&replies);
    let g = if tools {
        G::new(verdicts.with_tools()).await
    } else {
        G::new(verdicts.respond()).await
    };
    let mut s = g.session(Some("Goal session"), None);
    g.command(&mut s, "Fix the flaky test").await.unwrap();
    // The third stalled round in a row pauses instead of continuing.
    assert_eq!(verdicts.asked(), reasons.len());
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(
        goal.reason.unwrap(),
        format!("Not making progress: {}", reasons.last().unwrap())
    );
    assert_eq!(goal.rounds as usize, reasons.len() - 1);
}

// goal.test.ts: "pauses when the reviewer keeps giving the same reason, even with tool calls"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_when_the_reviewer_keeps_giving_the_same_reason_even_with_tool_calls() {
    pauses_when_it_stalls(true, &["The flaky test still fails."; 4]).await;
}

// goal.test.ts: "pauses when rounds keep passing without tool calls"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_when_rounds_keep_passing_without_tool_calls() {
    pauses_when_it_stalls(
        false,
        &["Two tests fail.", "One test fails.", "The lint step fails."],
    )
    .await;
}

// goal.test.ts: "keeps going while each round calls tools and finds a new reason"
#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_going_while_each_round_calls_tools_and_finds_a_new_reason() {
    let verdicts = Reviewer::new(&[
        r#"{"verdict":"continue","reason":"Two tests fail."}"#,
        r#"{"verdict":"continue","reason":"One test fails."}"#,
        r#"{"verdict":"continue","reason":"The lint step fails."}"#,
        r#"{"verdict":"done","reason":"Everything passes."}"#,
    ]);
    let g = G::new(verdicts.with_tools()).await;
    let mut s = g.session(Some("Goal session"), None);
    g.command(&mut s, "Make CI green").await.unwrap();
    assert_eq!(verdicts.asked(), 4);
    let goal = g.get(&s.id).unwrap();
    assert_eq!(
        (goal.status, goal.rounds, goal.stalls),
        (Status::Complete, 3, Some(0))
    );
}

// goal.test.ts: "pauses when the reviewer gives no usable verdict twice"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_when_the_reviewer_gives_no_usable_verdict_twice() {
    let verdicts = Reviewer::new(&["Looks good to me.", "I think so."]);
    let g = G::new(verdicts.respond()).await;
    let mut s = g.session(Some("Goal session"), None);
    g.command(&mut s, "Ship it").await.unwrap();
    assert_eq!(verdicts.asked(), 2);
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(
        goal.reason.as_deref(),
        Some("The goal reviewer gave no usable verdict")
    );
}

// goal.test.ts: "drops the reviewer's verdict when the goal is paused while it runs"
#[tokio::test(flavor = "multi_thread")]
async fn oc_drops_the_reviewers_verdict_when_the_goal_is_paused_while_it_runs() {
    let judging = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (j, r) = (judging.clone(), release.clone());
    let g = G::new(move |req| {
        if is_review(req) {
            j.notify_one();
            Answer::After(
                r.clone(),
                Box::new(text(r#"{"verdict":"continue","reason":"More to do."}"#)),
            )
        } else {
            text("Working on it")
        }
    })
    .await;
    let mut s = g.session(Some("Goal session"), None);
    let id = s.id.clone();
    let c = command::plugin_commands(&g.loaded().config)["goal"].clone();
    let run = tokio::spawn(async move {
        s.run_command(
            "/goal Rename the module",
            &c,
            "Rename the module",
            vec![],
            &mut Out::default(),
        )
        .await
        .unwrap();
        s
    });
    judging.notified().await;
    let pause = g.goal.commands()[0]
        .execute(CommandContext {
            session_id: id.clone(),
            cwd: g.dir.path().to_path_buf(),
            root: g.dir.path().to_path_buf(),
            args: "pause".into(),
            media: vec![],
        })
        .await;
    assert_eq!(pause, Ok(None));
    release.notify_one();
    let s = run.await.unwrap();
    let goal = g.get(&id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(goal.reason.as_deref(), Some("Paused by the user"));
    assert_eq!(goal.rounds, 0);
    assert_eq!(
        goal_messages(&s).len(),
        1,
        "no continuation after the pause"
    );
}

// goal.test.ts: "reminds the model of the goal after a compaction until a continuation restates it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_reminds_the_model_of_the_goal_after_a_compaction_until_a_continuation_restates_it() {
    let g = G::new(|r| {
        if is_summary(r) {
            text("## Objective\n- Make CI green")
        } else {
            text("On it.")
        }
    })
    .await;
    let mut s = g.session(Some("Goal session"), None);
    let mut out = Out::default();
    s.prompt("Look at the failing CI job", &mut out)
        .await
        .unwrap();
    s.prompt("And the flaky one", &mut out).await.unwrap();
    g.set(
        &s.id,
        json!({"id": "goal-compacted", "objective": "Make CI green", "status": "active",
               "rounds": 2, "maxRounds": 50, "boot": "an-earlier-process", "updatedAt": 0}),
    );
    s.compact(&mut out).await.unwrap();
    let before = g.model.requests().len();
    s.prompt("Carry on", &mut out).await.unwrap();
    let next = g.model.requests()[before..]
        .iter()
        .find(|r| !is_summary(r) && !is_review(r))
        .unwrap()
        .clone();
    let messages = next.body["messages"].as_array().unwrap();
    let all = serde_json::to_string(messages).unwrap();
    assert!(
        all.contains("A goal is active for this session. Keep working toward it."),
        "{all}"
    );
    // The reminder goes before the user's prompt and is not saved.
    assert!(messages.last().unwrap()["content"]
        .to_string()
        .contains("Carry on"));
    assert!(messages[messages.len() - 2]["content"]
        .to_string()
        .contains("Keep working toward it."));
    assert!(!s
        .messages
        .iter()
        .any(|m| format!("{m:?}").contains("Keep working toward it.")));
}

// goal.test.ts: "compacts and asks again when the reviewer's request is too large"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compacts_and_asks_again_when_the_reviewers_request_is_too_large() {
    let reviews = Arc::new(AtomicUsize::new(0));
    let n = reviews.clone();
    let g = G::new(move |r| {
        if is_summary(r) {
            return text("## Objective\n- Make CI green");
        }
        if !is_review(r) {
            return text("Working on it");
        }
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            overflow()
        } else {
            text(r#"{"verdict":"done","reason":"CI is green."}"#)
        }
    })
    .await;
    let mut s = g.session(Some("Goal session"), None);
    // Earlier work, so there is history to compact.
    s.prompt("Look at the CI job", &mut Out::default())
        .await
        .unwrap();
    let out = g.command(&mut s, "Make CI green").await.unwrap();
    assert_eq!(reviews.load(Ordering::SeqCst), 2);
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Complete);
    assert_eq!(goal.reason.as_deref(), Some("CI is green."));
    assert_eq!(out.compactions, 1);
}

// goal.test.ts: "pauses when the reviewer's request is too large even after compacting"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_when_the_reviewers_request_is_too_large_even_after_compacting() {
    let g = G::new(|r| {
        if is_summary(r) {
            text("## Objective\n- Make CI green")
        } else if is_review(r) {
            overflow()
        } else {
            text("Working on it")
        }
    })
    .await;
    let mut s = g.session(Some("Goal session"), None);
    s.prompt("Look at the CI job", &mut Out::default())
        .await
        .unwrap();
    g.command(&mut s, "Make CI green").await.unwrap();
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(
        goal.reason.as_deref(),
        Some("The goal reviewer's request is too large for the model")
    );
}

// goal.test.ts: "pauses an active goal when the user stops the session"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_an_active_goal_when_the_user_stops_the_session() {
    let started = Arc::new(tokio::sync::Notify::new());
    let never = Arc::new(tokio::sync::Notify::new());
    let (st, nv) = (started.clone(), never.clone());
    let g = G::new(move |_| {
        st.notify_one();
        Answer::After(nv.clone(), Box::new(text("Working on it")))
    })
    .await;
    let mut s = g.session(Some("Goal session"), None);
    let id = s.id.clone();
    let c = command::plugin_commands(&g.loaded().config)["goal"].clone();
    let run = tokio::spawn(async move {
        let _ = s
            .run_command(
                "/goal Refactor the store",
                &c,
                "Refactor the store",
                vec![],
                &mut Out::default(),
            )
            .await;
    });
    started.notified().await;
    run.abort();
    let _ = run.await;
    let goal = g.get(&id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(goal.reason.as_deref(), Some("Stopped by the user"));
    never.notify_waiters();
}

// goal.test.ts: "pauses a goal set by an earlier server process instead of continuing it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_a_goal_set_by_an_earlier_server_process_instead_of_continuing_it() {
    let verdicts = Reviewer::new(&[r#"{"verdict":"continue","reason":"Not yet."}"#]);
    let g = G::new(verdicts.respond()).await;
    let mut s = g.session(Some("Goal session"), None);
    g.set(
        &s.id,
        json!({"id": "goal-earlier", "objective": "Migrate the database", "status": "active",
               "rounds": 3, "maxRounds": 20, "boot": "an-earlier-process", "updatedAt": 0}),
    );
    s.prompt("Server came back", &mut Out::default())
        .await
        .unwrap();
    assert_eq!(verdicts.asked(), 0);
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(
        goal.reason.as_deref(),
        Some("The server restarted while the goal was active")
    );
}

// goal.test.ts: "does not give a child session its parent's goal"
#[tokio::test(flavor = "multi_thread")]
async fn oc_does_not_give_a_child_session_its_parents_goal() {
    let verdicts = Reviewer::new(&[r#"{"verdict":"blocked","reason":"Waiting on the user."}"#]);
    let g = G::new(verdicts.respond()).await;
    let mut s = g.session(Some("Goal session"), None);
    g.command(&mut s, "Tidy the docs").await.unwrap();
    assert_eq!(verdicts.asked(), 1);
    let mut child = g.session(Some("Helper"), Some(&s.id));
    assert_eq!(g.get(&child.id), None);
    child.prompt("Help out", &mut Out::default()).await.unwrap();
    assert_eq!(verdicts.asked(), 1);
}

// goal.test.ts: "pauses, resumes and clears a goal through its subcommands"
#[tokio::test(flavor = "multi_thread")]
async fn oc_pauses_resumes_and_clears_a_goal_through_its_subcommands() {
    let verdicts = Reviewer::new(&[
        r#"{"verdict":"paused","reason":"The user asked to stop for now."}"#,
        r#"{"verdict":"done","reason":"The changelog lists every PR."}"#,
    ]);
    let g = G::new(verdicts.respond()).await;
    let mut s = g.session(Some("Goal session"), None);
    g.command(&mut s, "Update the changelog").await.unwrap();
    let goal = g.get(&s.id).unwrap();
    assert_eq!(goal.status, Status::Paused);
    assert_eq!(
        goal.reason.as_deref(),
        Some("The user asked to stop for now.")
    );
    let e = g.command(&mut s, "pause").await.unwrap_err();
    assert_eq!(e.to_string(), "No active goal to pause.");

    g.command(&mut s, "Resume").await.unwrap();
    let goal = g.get(&s.id).unwrap();
    assert_eq!((goal.status, goal.rounds), (Status::Complete, 1));
    let sent = goal_messages(&s);
    assert_eq!(sent.len(), 2, "the kickoff and the resume continuation");
    assert!(sent[1].contains("The user resumed the goal."));

    let e = g.command(&mut s, "").await.unwrap_err();
    assert!(
        e.to_string()
            .starts_with("Goal achieved: Update the changelog"),
        "{e}"
    );
    g.command(&mut s, "clear").await.unwrap();
    assert_eq!(g.get(&s.id), None);
    let e = g.command(&mut s, "clear").await.unwrap_err();
    assert_eq!(e.to_string(), "No goal is set.");
}

// ---- goal-watchdog.test.ts: the plugin driven through its hooks ----

struct W {
    _dir: tempfile::TempDir,
    host: Arc<Host>,
    goal: GoalPlugin,
    session: String,
    child: String,
    grandchild: String,
    unrelated: String,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl W {
    fn new() -> W {
        let lock = plugin_support::serial();
        let dir = tempfile::tempdir().unwrap();
        let host =
            Arc::new(Host::open(&dir.path().join("db.sqlite"), dir.path().to_path_buf()).unwrap());
        plugin::set_host(host.clone());
        let mk = |parent: Option<&str>| {
            host.create(NewSession {
                title: Some("Release".into()),
                parent_id: parent.map(str::to_string),
                ..Default::default()
            })
            .unwrap()
            .id
        };
        let session = mk(None);
        let child = mk(Some(&session));
        let grandchild = mk(Some(&child));
        let unrelated = mk(None);
        W {
            _dir: dir,
            host: host.clone(),
            goal: GoalPlugin::with_wait_limit(WAIT_LIMIT),
            session,
            child,
            grandchild,
            unrelated,
            _lock: lock,
        }
    }

    async fn command(&self, args: &str) -> Result<Option<String>, String> {
        self.goal.commands()[0]
            .execute(CommandContext {
                session_id: self.session.clone(),
                cwd: std::env::temp_dir(),
                root: std::env::temp_dir(),
                args: args.into(),
                media: vec![],
            })
            .await
    }

    async fn turn_end(&self) {
        self.goal.event(&Event::TurnEnd {
            session_id: &self.session,
            error: None,
        });
        settle().await;
    }

    async fn turn_start(&self) {
        self.goal.event(&Event::TurnStart {
            session_id: &self.session,
            text: "",
        });
        settle().await;
    }

    async fn step(&self, id: &str) {
        self.goal.event(&Event::StepEnd {
            session_id: id,
            model: "fake/m",
            usage: &Usage::default(),
        });
        settle().await;
    }

    fn status(&self) -> (Status, Option<String>) {
        let g = self.goal.get(&self.session).unwrap();
        (g.status, g.reason)
    }
}

async fn settle() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

async fn advance(d: Duration) {
    tokio::time::advance(d).await;
    settle().await;
}

const MINUTE: Duration = Duration::from_secs(60);
const WAITED: &str = "Nothing ran for 30 minutes while the goal waited on background work";

// goal-watchdog.test.ts: "pauses a goal an earlier server process left active when the plugin starts"
#[tokio::test(start_paused = true)]
async fn oc_pauses_a_goal_an_earlier_server_process_left_active_when_the_plugin_starts() {
    let w = W::new();
    w.host
        .storage("goal")
        .set(
            &format!("session:{}", w.session),
            &json!({"id": "goal-earlier", "objective": "Ship the release", "status": "active",
                    "rounds": 1, "maxRounds": 50, "boot": "an-earlier-process", "updatedAt": 0}),
        )
        .unwrap();
    w.host
        .storage("goal")
        .set("session:ses_gone", &json!({"id": "g"}))
        .unwrap();
    w.goal.sweep();
    assert_eq!(
        w.status(),
        (
            Status::Paused,
            Some("The server restarted while the goal was active".into())
        )
    );
    assert_eq!(
        w.host.storage("goal").get("session:ses_gone").unwrap(),
        None,
        "the goals of sessions that are gone are dropped"
    );
}

// goal-watchdog.test.ts: "pauses a goal that sits idle past the limit, and not while the Session keeps running"
#[tokio::test(start_paused = true)]
async fn oc_pauses_a_goal_that_sits_idle_past_the_limit_and_not_while_the_session_keeps_running() {
    let w = W::new();
    w.command("Ship the release").await.unwrap();
    assert_eq!(w.status().0, Status::Active);
    // Waiting on background work: the turn ended with the goal active.
    w.turn_end().await;
    advance(WAIT_LIMIT - MINUTE).await;
    // The work reports: the session runs again, which cancels the wait.
    w.turn_start().await;
    advance(5 * MINUTE).await;
    assert_eq!(w.status().0, Status::Active);
    w.turn_end().await;
    advance(WAIT_LIMIT).await;
    assert_eq!(w.status(), (Status::Paused, Some(WAITED.into())));
}

// goal-watchdog.test.ts: "keeps waiting while a descendant Session keeps running steps"
#[tokio::test(start_paused = true)]
async fn oc_keeps_waiting_while_a_descendant_session_keeps_running_steps() {
    let w = W::new();
    w.command("Ship the release").await.unwrap();
    w.turn_end().await;
    advance(WAIT_LIMIT - MINUTE).await;
    w.step(&w.child.clone()).await;
    advance(WAIT_LIMIT - MINUTE).await;
    w.step(&w.grandchild.clone()).await;
    advance(WAIT_LIMIT - MINUTE).await;
    assert_eq!(w.status().0, Status::Active);
    // Another session's steps say nothing about this goal's work.
    w.step(&w.unrelated.clone()).await;
    advance(2 * MINUTE).await;
    assert_eq!(w.status(), (Status::Paused, Some(WAITED.into())));
}

// goal-watchdog.test.ts: "never overwrites what the user did after the loop decided"
#[tokio::test(start_paused = true)]
async fn oc_never_overwrites_what_the_user_did_after_the_loop_decided() {
    let w = W::new();
    w.command("Ship the release").await.unwrap();
    w.turn_end().await;
    w.command("pause").await.unwrap();
    advance(WAIT_LIMIT).await;
    assert_eq!(
        w.status(),
        (Status::Paused, Some("Paused by the user".into()))
    );
}

// goal-watchdog.test.ts: "serves the goal over RPC and publishes every change"
#[tokio::test(start_paused = true)]
async fn oc_serves_the_goal_and_publishes_every_change() {
    let w = W::new();
    let mut changes = w.goal.subscribe();
    assert_eq!(w.goal.get(&w.session), None);
    w.command("Ship the release").await.unwrap();
    w.command("pause").await.unwrap();
    let g = w.goal.get(&w.session).unwrap();
    assert_eq!(
        (g.objective.as_str(), g.status),
        ("Ship the release", Status::Paused)
    );
    w.command("clear").await.unwrap();
    assert_eq!(w.goal.get(&w.session), None);
    let mut seen: Vec<Change> = Vec::new();
    while let Ok(c) = changes.try_recv() {
        seen.push(c);
    }
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|c| c.session_id == w.session));
    let first = seen[0].goal.as_ref().unwrap();
    assert_eq!(
        (first.objective.as_str(), first.status),
        ("Ship the release", Status::Active)
    );
    let second = seen[1].goal.as_ref().unwrap();
    assert_eq!(
        (second.status, second.reason.as_deref()),
        (Status::Paused, Some("Paused by the user"))
    );
    assert_eq!(seen[2].goal, None);
}
