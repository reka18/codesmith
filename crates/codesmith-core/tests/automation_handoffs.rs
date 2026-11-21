//! Durable automation handoffs across restarts: a runner is stopped at each
//! phase boundary (its future dropped, as a killed process stops) and a new
//! runner on the same database finishes the run, without running a check
//! twice, writing a prompt twice or answering it twice. Also two runners on
//! one database, event handoffs, the permissions of resumed turns, and a
//! seeded random-kill loop checking the invariants.

mod handoff_support;

use codesmith_core::automation::{self as au, Action, Runner, Target, Trigger};
use codesmith_core::store::Record;
use handoff_support::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn helper() -> Target {
    Target::Subagent {
        subagent: "helper".into(),
    }
}

/// The replies (`a:` texts) in session `id`.
fn replies(w: &World, id: &str) -> usize {
    w.texts(id).iter().filter(|t| t.starts_with("a:")).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_after_claimed_delivers_a_prompt_once() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "check the build", helper());
    w.due_now(&id);
    // The process claimed the run and stopped before handing it off.
    let claimed = au::claim_due(&w.db, codesmith_core::store::now()).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::CLAIMED));
    let runner = w.runner();
    runner.tick().await.unwrap();
    assert_eq!(
        w.phase(&id, 1).as_deref(),
        Some(au::CLAIMED),
        "a run another process may still be handing off is left alone"
    );
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    let child = au::run_session_id(&id, 1);
    assert_eq!(
        w.texts(&child),
        ["u:check the build", "a:answer to: check the build"]
    );
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.get(&id).runs, 1, "nothing was claimed again");
    assert_eq!(replies(&w, &child), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_during_a_check_fails_it_as_a_strike_without_running_it_again() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let flag = w.data.path().join("go");
    let code = format!(
        "await tools.counter.post({{ text: 'run ' + input.runs }}); await tools.counter.wait({{ until: {:?} }}); return {{ state: 1, send: {{ prompt: 'p' }} }}",
        flag.display().to_string()
    );
    let id = w.code(None, &code, None);
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the check's first call", || w.posts().len() == 1).await;
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::CLAIMED));
    r.kill(&w.db).await;
    std::fs::write(&flag, "").unwrap();
    w.stale();
    w.runner().tick().await.unwrap();
    let a = w.get(&id);
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::FAILED));
    assert_eq!(w.posts(), ["run 1"], "the check did not run again");
    assert_eq!(
        (a.state.clone(), a.failures, a.status.as_str()),
        (Value::Null, 1, "active")
    );
    assert!(
        a.last_error
            .as_deref()
            .is_some_and(|e| e.contains("Interrupted before its check finished")),
        "{a:?}"
    );
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_after_checked_admits_the_saved_result_once() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = w.code(
        Some(&creator),
        "await tools.counter.post({ text: 'checked' }); return { state: 7, send: { prompt: 'ready' } }",
        None,
    );
    // A turn running in the target session holds the handoff after the check.
    assert!(w.db.lease_session(&creator, "other", 60_000).unwrap());
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the saved check", || {
        w.phase(&id, 1).as_deref() == Some(au::CHECKED)
    })
    .await;
    r.kill(&w.db).await;
    assert_eq!(w.get(&id).state, Value::Null, "nothing committed yet");
    w.db.release_session(&creator, "other").unwrap();
    w.stale();
    let runner = w.runner();
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    assert_eq!(w.posts(), ["checked"], "the check ran once");
    assert_eq!(w.get(&id).state, json!(7));
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
    assert_eq!(w.texts(&creator), ["u:ready", "a:answer to: ready"]);
    let run = &au::runs(&w.db, &id).unwrap()[0];
    assert!(
        run.detail.starts_with(&format!("Prompted {creator}")),
        "{}",
        run.detail
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_after_admitted_resumes_the_turn_with_exactly_one_reply() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "hold the line", helper());
    w.model.hold();
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the admitted turn's request", || {
        w.phase(&id, 1).as_deref() == Some(au::ADMITTED) && w.model.requests("hold") == 1
    })
    .await;
    r.kill(&w.db).await;
    let a = w.get(&id);
    assert_eq!(a.runs, 1);
    w.model.release();
    w.stale();
    w.runner().tick().await.unwrap();
    let child = au::run_session_id(&id, 1);
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    assert_eq!(
        w.texts(&child),
        ["u:hold the line", "a:answer to: hold the line"]
    );
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
    assert_eq!(
        w.model.requests("hold"),
        2,
        "one aborted request, one resumed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_turn_interrupted_again_is_failed_not_resumed_twice() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "hold on", helper());
    w.model.hold();
    w.due_now(&id);
    for n in 1..=2 {
        if n == 2 {
            w.stale();
        }
        let r = spawn_tick(w.runner());
        until("the held request", || w.model.requests("hold") == n).await;
        r.kill(&w.db).await;
    }
    w.model.release();
    w.stale();
    w.runner().tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::FAILED));
    let run = &au::runs(&w.db, &id).unwrap()[0];
    assert!(run.detail.contains("resumed once"), "{}", run.detail);
    assert_eq!(w.model.requests("hold"), 2, "no third attempt");
    assert_eq!(replies(&w, &au::run_session_id(&id, 1)), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_after_settled_runs_nothing_again() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.code(
        None,
        "await tools.counter.post({ text: 'once' }); return { state: 1, send: { prompt: 'done', to: { subagent: 'helper' } } }",
        None,
    );
    w.due_now(&id);
    w.runner().tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    w.stale();
    let again = w.runner();
    again.tick().await.unwrap();
    again.tick().await.unwrap();
    assert_eq!(w.posts(), ["once"]);
    assert_eq!(w.get(&id).runs, 1);
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
    assert_eq!(replies(&w, &au::run_session_id(&id, 1)), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_leaves_a_session_busy_elsewhere_and_settles_an_answered_one() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "hold please", helper());
    w.model.hold();
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the held request", || w.model.requests("hold") == 1).await;
    r.kill(&w.db).await;
    w.model.release();
    let child = au::run_session_id(&id, 1);
    // An open window runs a turn there: the resume waits for it.
    assert!(w.db.lease_session(&child, "window", 60_000).unwrap());
    w.stale();
    let runner = w.runner();
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::ADMITTED));
    assert_eq!(au::pending(&w.db, &id).unwrap()[0].attempts, 1);
    // That turn answered the prompt: the run settles with its answer.
    w.db.append(
        &child,
        &Record::Message {
            message: codesmith_ai::Message {
                role: codesmith_ai::Role::Assistant,
                parts: vec![codesmith_ai::Part::text("answered in the window")],
            },
        },
    )
    .unwrap();
    w.db.release_session(&child, "window").unwrap();
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    assert_eq!(
        au::runs(&w.db, &id).unwrap()[0].detail,
        "answered in the window"
    );
    assert_eq!(w.model.requests("hold"), 1, "no second turn");
    assert_eq!(replies(&w, &child), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_runners_on_one_database_produce_one_turn() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "once only", helper());
    let (a, b) = (w.runner(), w.runner());
    w.due_now(&id);
    for _ in 0..3 {
        let (x, y) = tokio::join!(a.tick(), b.tick());
        x.unwrap();
        y.unwrap();
        w.stale();
    }
    let a1 = w.get(&id);
    assert_eq!(a1.runs, 1, "{:?}", w.phases(&id));
    assert_eq!(w.model.requests("once only"), 1);
    assert_eq!(replies(&w, &au::run_session_id(&id, 1)), 1);
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
}

/// An event automation on the `rich` server following `plan`.
fn watch_plan(w: &World, session: Option<&str>, plan: Value, action: Action) -> String {
    let file = w.data.path().join("plan.json");
    std::fs::write(&file, plan.to_string()).unwrap();
    w.create(
        session,
        Trigger::Event {
            server: "rich".into(),
            event: "ticket.created".into(),
            args: Some(json!({"queue": "plan", "plan": file.display().to_string()})),
        },
        action,
    )
}

fn stream_cursors(w: &World) -> Vec<Value> {
    std::fs::read_to_string(w.data.path().join("plan.json.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Make every record written into session `id` fail, as a database error
/// during the handoff would.
fn refuse_writes(w: &World, id: &str) {
    w.sql(
        &format!("CREATE TRIGGER refuse BEFORE INSERT ON record WHEN NEW.session_id = '{id}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"),
        [],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_event_handoff_keeps_the_cursor_and_a_replayed_event_runs_once() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = watch_plan(
        &w,
        Some(&creator),
        json!([
            {"events": [["e1", "c1", "one"]]},
            {"events": [["e1", "c1", "one"], ["e1", "c1", "one"], ["e2", "c2", "two"]]}
        ]),
        Action::Prompt {
            prompt: "triage".into(),
            to: Target::Session {
                session: creator.clone(),
            },
        },
    );
    refuse_writes(&w, &creator);
    let runner = w.runner();
    tick_until(&runner, "the refused handoff", || {
        w.get(&id).status == "paused"
    })
    .await;
    let a = w.get(&id);
    assert_eq!(
        a.event.clone().unwrap_or_default().cursor.as_deref(),
        Some("c0"),
        "the cursor stays before the refused event: {a:?}"
    );
    assert_eq!(w.phases(&id), [(1, au::FAILED.to_string())]);
    assert!(w.texts(&creator).is_empty());
    w.sql("DROP TRIGGER refuse", []);
    au::set_status(&w.db, &id, "active").unwrap();
    tick_until(&runner, "both events", || {
        w.get(&id).event.and_then(|p| p.id).as_deref() == Some("e2")
            && au::pending(&w.db, &id).unwrap().is_empty()
    })
    .await;
    assert_eq!(stream_cursors(&w), [Value::Null, json!("c0")]);
    let triaged: Vec<String> = w
        .texts(&creator)
        .into_iter()
        .filter(|t| t.starts_with("u:"))
        .collect();
    assert_eq!(
        triaged.len(),
        2,
        "e1 once despite its replays, then e2: {triaged:?}"
    );
    assert!(triaged[0].contains("\"id\":\"e1\"") && triaged[1].contains("\"id\":\"e2\""));
    assert_eq!(w.get(&id).event.unwrap().cursor.as_deref(), Some("c2"));
    au::remove(&w.db, &id).unwrap();
    runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_turn_still_rejects_asks() {
    // Unattended runs deny shell; this opts back in to asking, so the
    // refusal of an ask is what is tested.
    let Some(w) = World::new(json!({"automation": {"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}})).await else {
        return;
    };
    let id = w.prompt(None, "hold and use shell", helper());
    w.model.hold();
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the held request", || w.model.requests("hold") == 1).await;
    r.kill(&w.db).await;
    w.model.release();
    w.stale();
    w.runner().tick().await.unwrap();
    let run = &au::runs(&w.db, &id).unwrap()[0];
    assert_eq!(run.outcome, "fired");
    assert!(run.detail.contains("runs unattended"), "{}", run.detail);
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_rules_still_deny_after_the_creator_is_deleted() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let secret = w.project.path().join("notes.txt");
    std::fs::write(&secret, "secret").unwrap();
    let creator = w.creator(json!([{"action": "read", "resource": "*", "effect": "deny"}]));
    let call = format!(
        "call read {}",
        json!({"path": secret.display().to_string()})
    );
    let id = w.code(
        Some(&creator),
        &format!(
            "return {{ send: {{ prompt: {}, to: {{ subagent: 'helper' }} }} }}",
            json!(call)
        ),
        None,
    );
    // Hold the handoff after the check, then delete the creating session.
    let child = au::run_session_id(&id, 1);
    assert!(w.db.lease_session(&child, "other", 60_000).unwrap());
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the saved check", || {
        w.phase(&id, 1).as_deref() == Some(au::CHECKED)
    })
    .await;
    r.kill(&w.db).await;
    w.db.delete(&creator).unwrap();
    w.db.release_session(&child, "other").unwrap();
    w.stale();
    w.runner().tick().await.unwrap();
    assert_eq!(
        w.phase(&id, 1).as_deref(),
        Some(au::SETTLED),
        "{:?}",
        w.get(&id)
    );
    let info = w.db.get(&child).unwrap().unwrap();
    assert_eq!(
        info.parent_id, None,
        "a session of its own once the creator is gone"
    );
    let detail = &au::runs(&w.db, &id).unwrap()[0].detail;
    assert!(detail.contains("tool said:"), "{detail}");
    assert!(!detail.contains("secret"), "{detail}");
    let rules = w.db.permissions(&child).unwrap().unwrap();
    assert!(
        rules
            .iter()
            .any(|r| r.action == "read" && r.effect == codesmith_core::permission::Effect::Deny),
        "{rules:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_config_deny_still_applies_on_resume() {
    let Some(w) =
        World::new(json!({"permissions": [{"action": "read", "resource": "*", "effect": "deny"}]}))
            .await
    else {
        return;
    };
    let secret = w.project.path().join("notes.txt");
    std::fs::write(&secret, "secret").unwrap();
    let prompt = format!(
        "hold then call read {}",
        json!({"path": secret.display().to_string()})
    );
    let id = w.prompt(None, &prompt, helper());
    w.model.hold();
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the held request", || w.model.requests("hold") == 1).await;
    r.kill(&w.db).await;
    w.model.release();
    w.stale();
    w.runner().tick().await.unwrap();
    let detail = &au::runs(&w.db, &id).unwrap()[0].detail;
    assert!(detail.contains("tool said:"), "{detail}");
    assert!(!detail.contains("secret"), "{detail}");
}

/// A deterministic generator for the kill points.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Kill the runner (or, with `pair`, one or both of two runners) at
/// random points of a run over many iterations, finish what is left, and
/// check what must hold whatever the kills hit.
async fn random_kills(seed: u64, pair: bool) {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let iterations: u64 = std::env::var("HANDOFF_KILLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let id = w.code(
        None,
        "await tools.counter.post({ text: 'run ' + input.runs }); return { state: (input.state || 0) + 1, send: { prompt: 'run ' + input.runs, to: { subagent: 'helper' } } }",
        None,
    );
    let mut rng = Lcg(seed);
    let end = Instant::now() + Duration::from_secs(300);
    let revive = |w: &World| {
        if w.get(&id).status == "paused" {
            au::set_status(&w.db, &id, "active").unwrap();
        }
    };
    for _ in 0..iterations {
        assert!(Instant::now() < end, "the kill loop ran out of time");
        revive(&w);
        if au::pending(&w.db, &id).unwrap().is_empty() {
            w.due_now(&id);
        }
        let first = spawn_tick(w.runner());
        let second = pair.then(|| spawn_tick(w.runner()));
        // Mostly within a run's first moments, where its phases change.
        let wait = match rng.next() % 4 {
            0 => rng.next() % 10,
            1 => rng.next() % 400,
            _ => rng.next() % 150,
        };
        tokio::time::sleep(Duration::from_millis(wait)).await;
        match (second, rng.next() % 3) {
            (Some(s), 0) => {
                first.kill(&w.db).await;
                tokio::time::timeout(Duration::from_secs(60), s.join(&w.db))
                    .await
                    .expect("the surviving runner finishes");
            }
            (Some(s), 1) => {
                s.kill(&w.db).await;
                first.kill(&w.db).await;
            }
            (Some(s), _) => {
                s.kill(&w.db).await;
                tokio::time::timeout(Duration::from_secs(60), first.join(&w.db))
                    .await
                    .expect("the surviving runner finishes");
            }
            (None, _) => first.kill(&w.db).await,
        }
        w.stale();
    }
    // Let one runner finish whatever is left.
    let runner = w.runner();
    loop {
        assert!(Instant::now() < end, "draining ran out of time");
        revive(&w);
        w.stale();
        runner.tick().await.unwrap();
        if au::pending(&w.db, &id).unwrap().is_empty() {
            break;
        }
    }
    let phases = w.phases(&id);
    eprintln!(
        "{} runs: {} settled, {} failed",
        phases.len(),
        phases.iter().filter(|(_, p)| p == au::SETTLED).count(),
        phases.iter().filter(|(_, p)| p == au::FAILED).count()
    );
    check_invariants(&w, &id);
}

fn check_invariants(w: &World, id: &str) {
    let a = w.get(id);
    let rows: Vec<(u64, String, Option<String>)> = w
        .db
        .with_conn(|c| {
            let mut st = c.prepare(
                "SELECT run, phase, message_id FROM automation_run WHERE automation_id = ?1 ORDER BY run",
            )?;
            let r = st.query_map([id], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)))?;
            Ok(r.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    // No run counted twice: each claim has exactly one row.
    assert_eq!(a.runs as usize, rows.len(), "{rows:?}");
    assert!(
        rows.iter()
            .all(|(_, p, _)| p == au::SETTLED || p == au::FAILED),
        "{rows:?}"
    );
    // No check ran twice.
    let posts = w.posts();
    let mut unique = posts.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), posts.len(), "a check ran twice: {posts:?}");
    let admitted = rows.iter().filter(|(_, _, m)| m.is_some()).count();
    for (run, phase, message) in &rows {
        let key = au::message_id(id, *run);
        // No duplicate message, and one exactly when the run was admitted.
        assert_eq!(w.tagged(&key), i64::from(message.is_some()), "run {run}");
        let child = au::run_session_id(id, *run);
        let n = replies(w, &child);
        assert!(n <= 1, "run {run} answered {n} times");
        if phase == au::SETTLED {
            assert_eq!(n, 1, "settled run {run} has its answer");
            assert!(posts.contains(&format!("run {run}")), "run {run}");
        }
    }
    // State is committed exactly with each admission.
    assert_eq!(a.state, json!(admitted), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn random_kills_keep_runs_messages_and_state_consistent() {
    random_kills(0x5eed_0001, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn random_kills_of_two_runners_keep_runs_messages_and_state_consistent() {
    random_kills(0x5eed_0002, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runners_on_one_database_never_resume_a_run_in_flight() {
    // A long turn outlives the staleness window in one process: the other
    // runner cannot take it over while the first holds the lease.
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "hold for a while", helper());
    w.model.hold();
    w.due_now(&id);
    let first: Runner = w.runner();
    let r = spawn_tick(first);
    until("the held request", || w.model.requests("hold") == 1).await;
    w.stale();
    let second = w.runner();
    second.tick().await.unwrap();
    assert_eq!(
        w.model.requests("hold"),
        1,
        "the lease holder keeps its run"
    );
    w.model.release();
    r.join(&w.db).await;
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::SETTLED));
    assert_eq!(replies(&w, &au::run_session_id(&id, 1)), 1);
}
